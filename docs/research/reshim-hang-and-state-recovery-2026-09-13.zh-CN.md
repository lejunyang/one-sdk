# reshim 卡死与状态恢复能力调查

日期：2026-09-13

状态：**已定位根因，含实测数据。** 三项修改随后在同一批提交中实现。

起因：在一个配了 11 个 `conda:` 工具的项目里执行 `osdk reshim`，跑满 695 CPU-秒仍不返回，只能强制终止。随后怀疑强杀破坏了状态数据，因此一并调查 osdk 的崩溃安全性与恢复能力。

本文的每个数字都来自本机实测（Windows x64，`osdk 0.0.2`，`E:\osdk-data`，installs 树 96,350 个条目 / 87,449 个文件）。

---

## 执行摘要

三个结论，其中第二个推翻了调查初期的假设：

**一、reshim 卡死是真实的性能缺陷，成因是无缓存的重复全量扫描。** 代价随「工具发布的命令数 × installs 树中的文件数」**乘性**增长。单次 `scan_installs` 在本机耗时 5.8 秒，而一次 reshim 会触发 O(backend × version × name) 次。详见 §1。

**二、强杀并没有破坏状态——安装路径本身是崩溃安全的。** 在安装进行中的四个不同阶段各强杀一次，每次检查「有 manifest 却无 `.osdk-complete` 标记」的半成品目录，结果都是 0 个。`finalize_artifact_install` 的写入顺序（manifest 原子写 → 最后写标记 → 失败则整目录删除）是正确的，`fslock` 也随进程退出由内核释放。**调查初期「强杀破坏了状态」的判断是错的。** 详见 §2。

**三、但发现了一个更严重的缺陷：单个 manifest 损坏会让全部 dynamic 工具不可用，且恢复命令自身也被挡住。** `CorruptManifestPolicy::FailClosed` 让任一 manifest 不可解析就使整个扫描失败，波及所有无辜工具，连用来修复的 `uninstall` 都无法执行。用户唯一的出路是手工删目录。详见 §3。

---

## 1. reshim 卡死

### 1.1 先定性：不是死锁，是文件系统遍历

按 5 秒间隔采样进程，CPU 时间稳定以每秒约 3.2 秒的速度增长——**持续满载而非阻塞**，排除死锁。

再拆分内核态与用户态：

| 采样点 | 用户态 | 内核态 | 内核占比 |
| --- | --- | --- | --- |
| 8 秒 | 1.1 s | 9.5 s | 89.3% |
| 16 秒 | 2.0 s | 19.1 s | 90.3% |
| 24 秒 | 3.2 s | 28.0 s | 89.6% |

九成时间在内核态，指向文件系统遍历。`ReadOperationCount` 偏低（12 秒仅 1,388 次）也吻合：目录枚举走 `NtQueryDirectoryFile`，不计入读操作计数。

两项交叉验证：

- `--offline` 下同样卡死，`Get-NetTCPConnection` 全程 0 个已建立连接——与网络下载、conda 的 SAT 求解无关。
- 把 `OSDK_DATA_DIR` 指向空目录，同样的配置 **0.1 秒**完成。

### 1.2 对照实验：只换工作目录

同一个 osdk 二进制，同一台机器，只改当前目录：

| 场景 | 结果 |
| --- | --- |
| `C:\Users\<user>`（无项目级 conda 工具） | **27.9 秒**完成，`regenerated 500 shim(s)` |
| `E:\Projects\omy`（配 11 个 conda 工具） | **>120 秒未完成**，CPU 已耗 122 秒 |

### 1.3 分离变量：命令数 × 树规模

用受控的临时 data 目录，只放一个 conda 包，再用与 conda 无关的文件填充 installs 树。两个包的命令数差别很大（`osdk where --bins` 实测）：`nasm` 发布 2 个命令、withheld 0 个；`m2-pkg-config` 发布 2 个、withheld 31 个，合计 33 个。

| 包（命令数） | 空树 | +1 万文件 | +4 万文件 |
| --- | --- | --- | --- |
| `nasm`（2） | 0.1 s | 0.1 s | 0.3 s |
| `m2-pkg-config`（33） | 0.7 s | 1.2 s | **2.9 s** |

单独放大任一维度都不足以卡死，**两者同时放大才会**——代价是乘性的。逼近真机规模验证：

| 配置 | 耗时 |
| --- | --- |
| 8 万填充文件 + 1 个工具 | 5.6 秒 |
| 8 万填充文件 + 3 个工具 | **25.0 秒** |

真机是 87,449 个文件 × 11 个 conda 工具，695 CPU-秒不返回与此完全吻合。

补充一个反常现象的解释：按工具条数递增测试时，2 个工具 37.4 秒、3 个工具就超时。这不是阈值效应，而是各包命令数差异所致——第 3 个加入的包恰好命令数较多。

### 1.4 根因：调用链上每一层都重新全量扫描

```
reshim                                    commands.rs:3778
└─ all_display_backends()                 commands.rs:1449
   │  └─ dynamic_scan_report()            commands.rs:5792  ← 无缓存
   └─ for backend                         （本机 11+ 个）
      ├─ list_installed()                 conda.rs:1077     ← 全量 scan_installs
      └─ for version
         ├─ request_selects_installed_version()   commands.rs:5875
         │  └─ list_installed()            commands.rs:5889  ← 闭包内又一次
         └─ generate_shims_for()           commands.rs:3894
            ├─ ensure_no_shim_conflicts()  commands.rs:6072
            │  └─ installed_shim_owners()  commands.rs:5945  ← 再一次
            └─ installed_shim_owners()     commands.rs:5945  ← 再一次
```

三处关键事实：

1. **`conda::list_installed`（`conda.rs:1077`）扫描整个 installs 根目录**，而不是只扫自己那一支。本机该目录下 zig 占 34,227 个文件、android-* 合计约 28,000 个，与 conda 完全无关却每次都被完整遍历。
2. **`dynamic_scan_report`（`commands.rs:5792`）没有任何缓存**，只是 `shim::scan_dynamic_installs` 的薄封装。
3. `scan_installs`（`inventory.rs:382`）用 walkdir 递归，深度上限 `DEFAULT_MAX_DEPTH = 8` 再 `saturating_add(1)`，即 9 层。本机单次全量遍历 5.8 秒。

这不是 conda 独有。`scan_installs(` / `scan_dynamic_installs(` 在 backend 层共 49 处命中，其中 `github.rs:961` 与 `:1095`、`http.rs:381`、`cargo_package.rs:285`、`go_package.rs:232`、`native_tool.rs:1648`、`conda.rs:262` 都是同一模式——**装的工具多、树大时都会被放大**。

---

## 2. 强杀是否破坏状态：安装路径是崩溃安全的

### 2.1 实测

在一个独立的 data 目录（与 store 同盘，避免跨盘移动干扰）里，于安装进行中的四个不同阶段各强杀一次，每次统计「有 `.osdk-install.json` 但无 `.osdk-complete`」的目录数：

| 强杀时机 | `list` 状态 | 半成品目录 |
| --- | --- | --- |
| 1.5 秒后 | no tools installed yet | **0** |
| 3.0 秒后 | no tools installed yet | **0** |
| 5.0 秒后 | no tools installed yet | **0** |
| 7.0 秒后 | no tools installed yet | **0** |

随后不做任何手工修复，直接 `exec` 即自动重装成功并跑出 `NASM version 2.16.0`。

### 2.2 为什么是安全的

`finalize_artifact_install`（`dynamic.rs:217`）的顺序是承重设计：

```
manifest.write_atomic(root)                    // 先原子写 manifest
std::fs::write(root.join(".osdk-complete"), b"")   // 最后才写完成标记
if result.is_err() { remove_dir_all(root) }    // 任何失败整目录删除
```

因为 `list_installed` 的过滤条件要求 `.osdk-complete` 存在（`conda.rs:1090`），**没写到最后一步的安装一律不可见**，不存在「半装可见」的窗口。

锁也没有问题：`FileLock`（`lock.rs`）基于 `fslock`，是 OS 级锁，进程被杀时由内核释放，不会留下需要清理的死锁文件。

### 2.3 三种损坏场景都能自愈

| 人为损坏 | `list` 表现 | `exec` 能否自愈 |
| --- | --- | --- |
| 删除 `.osdk-complete` | no tools installed yet | 能，标记文件被重建 |
| manifest 截断成非法 JSON | 报错（见 §3） | 能 |
| manifest 整个删除 | no tools installed yet | 能 |

### 2.4 调查中被推翻的两个假设

- **「`.osdk-complete` 在强杀时丢失」**：核对全部 12 个 conda 包，标记文件与 manifest 都在，无一缺失。
- **「`osdk list conda:m2-bash` 报 no tools 是状态损坏」**：该现象已自愈，现正常返回 `5.2.37.2`；`osdk list` 全量输出也包含它。真正原因是当时该工具尚未完成首次注册，而非记录损坏。

另外，`osdk exec` 每次都打印 `installing ... / installed ...` 是正常输出，不代表重复安装。

---

## 3. 真正的缺陷：一个坏 manifest 让全部工具不可用

### 3.1 影响半径

`ScanOptions::default()` 的 `corrupt_manifest_policy` 是 `CorruptManifestPolicy::FailClosed`（`inventory.rs:195`）——任一 manifest 不可解析就让**整个扫描**失败。

实测：装好 `conda:nasm` 与 `conda:m2-sed` 两个工具，只把 nasm 的 manifest 截断成 `{"schema":`，然后：

| 命令 | 退出码 | 说明 |
| --- | --- | --- |
| `list`（全部） | 1 | |
| `list conda:m2-sed` | 1 | **无辜工具** |
| `where conda:m2-sed` | 1 | **无辜工具** |
| `exec conda:m2-sed` | 1 | **无辜工具** |
| `reshim` | 1 | |
| `uninstall conda:nasm@2.16.3` | 1 | **本该用来修复的命令** |

**恢复命令自己也被同一个扫描挡住**，用户被锁死在这个状态里。`doctor --verify --tool conda:nasm` 返回退出码 1，但只打印常规的目录信息，不指出任何损坏细节；不带 `--verify` 的 `doctor` 甚至返回 0，看不出异常。

### 3.2 唯一可行的恢复路径

报错信息是这个状态下唯一有用的线索，它**给出了坏文件的完整路径**：

```
error: refusing dynamic tool inventory scan because json error:
EOF while parsing a value at line 1 column 10
at E:\...\installs\conda\nasm\2.16.3\b3-v2-35b8...\.osdk-install.json
```

据此手工删除该 `b3-v2-...` 指纹目录后，`list` 立即恢复正常，无辜工具重新可用，被删的工具下次 `exec` 时自动重装。

### 3.3 附带发现：install 失败会回滚已成功的工具

配置里同时含 conda 工具与需要接受许可证的 android 包时，`install` 因 android 许可证报错退出，**已经装好的 conda 工具目录也被一并清空**（只剩 `.locks`，0 个 manifest）。这一项本次未深究，记录备查。

---

## 4. 三项修改

按风险从低到高排列，各自独立提交。

### 4.1 给单次进程内的 dynamic 扫描加缓存

`dynamic_scan_report` 改为进程内缓存，一次 reshim 只扫一次，把 O(backend × version × name) 次全量扫描降到 1 次。这是直接解决卡死的一项，风险最低——扫描结果在单条命令的生命周期内本就应当一致。

### 4.2 让 `FailClosed` 不再牵连无辜工具

单个 manifest 损坏时跳过并记录诊断，而不是让整棵树的扫描失败。至少要保证 `uninstall` 与 `list` 在这种状态下仍可执行，否则用户没有恢复出口。

### 4.3 让 `conda::list_installed` 只扫自己那一支

不再遍历整个 installs 根目录，从根上消除「zig 和 android 的文件拖慢 conda」这一类无谓开销。

---

## 附：证据来源

全部数据来自 2026-09-13 本机实测，环境为 Windows x64 / `osdk 0.0.2` / `E:\osdk-bin\osdk.exe`。涉及的源码位置以本次调查时的工作树为准：

- `crates/osdk-cli/src/commands.rs`：`reshim`(3778)、`all_display_backends`(1449)、`dynamic_scan_report`(5792)、`generate_shims_for`(3894)、`installed_shim_owners`(5945)、`ensure_no_shim_conflicts`(6072)、`request_selects_installed_version`(5875)
- `crates/osdk-core/src/inventory.rs`：`scan_installs`(382)、`ScanOptions`(178)、`impl Default`(189)、`CorruptManifestPolicy`(170)、`DEFAULT_MAX_DEPTH`(24)
- `crates/osdk-core/src/backend/conda.rs`：`list_installed`(1077)、完成标记过滤(1090)、`bin_names`(1121)
- `crates/osdk-core/src/backend/dynamic.rs`：`finalize_artifact_install`(217)
- `crates/osdk-core/src/lock.rs`：`FileLock`
- `crates/osdk-core/src/shim/mod.rs`：`validated_dynamic_install`(351)、`dynamic_bin_ownership`(427)

测试期间创建的临时 data 目录与项目目录均已删除，用户真实的 `E:\osdk-data` 未受影响（12 个 conda 工具完好）。
