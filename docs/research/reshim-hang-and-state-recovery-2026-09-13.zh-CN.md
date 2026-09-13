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

### 3.3 附带发现：install 失败会丢弃同批并发安装的成果

初次记录时把这一项写成「已经装好的 conda 工具目录被一并清空」，**这个描述是错的**，事后用隔离环境三步对照实验推翻：

| 场景 | 结果 | conda:nasm 状态 |
| --- | --- | --- |
| A 只装 conda | rc=0 | 完成标记=1，manifest=1 |
| B 在 A 的基础上加入 android 再 `install` | rc=1（许可证） | **完成标记=1，manifest=1，完好无损** |
| C 全新目录，一次性装 conda + android | rc=1（许可证） | 无 conda 目录 |

B 证明**已存在的安装不会被后来的失败牵连**。真实现象只有 C：同一条 `install` 命令里，与失败项**并发进行**的那个安装被取消了。

机制在 `crates/osdk-cli/src/commands.rs` 的 `install_requests`：请求经 `buffer_unordered(jobs)` 并发执行后由 `try_collect().await?` 收集，第一个 `Err` 立即返回，其余仍在执行的 future 被直接丢弃。被取消的安装不会留下半成品——`finalize_artifact_install` 的约定是任何失败都 `remove_dir_all` 整个目录——所以留下的是干净的「没装」，而不是损坏状态。

代价本身不大，因为已下载的字节留在 CAS 里：

| 观测 | 数值 |
| --- | --- |
| 失败后 store/cache 残留 | 1.45 MB（下载没有白费） |
| 修好配置后重装 conda | 5.3 s |
| 全新目录首次装 conda（对照） | 5.7 s |

但时机是错的。`crates/osdk-core/src/backend/android.rs` 的门禁注释写明它跑在 "before any bytes are fetched"，也就是说**这个失败在任何字节下载之前就能判定**，本不该等到别的工具已经开工。因此修法不是改并发语义（那会让用户等完所有下载才看到一个开头就能报的错），而是把这次必然发生的失败提前到所有安装开工之前，见 §4.4。

---

## 4. 四项修改

按风险从低到高排列，各自独立提交。

### 4.1 给单次进程内的 dynamic 扫描加缓存

`dynamic_scan_report` 改为进程内缓存，一次 reshim 只扫一次，把 O(backend × version × name) 次全量扫描降到 1 次。这是直接解决卡死的一项，风险最低——扫描结果在单条命令的生命周期内本就应当一致。

### 4.2 让 `FailClosed` 不再牵连无辜工具

单个 manifest 损坏时跳过并记录诊断，而不是让整棵树的扫描失败。至少要保证 `uninstall` 与 `list` 在这种状态下仍可执行，否则用户没有恢复出口。

> **后续状态（2026-09-13 晚）**：枚举类路径已通过 `ScanOptions::tolerant()` 解决，
> `list` / `reshim` / `uninstall` 不再被一个坏 manifest 挡住。
>
> 但本节当时只看到「损坏」这一种触发方式，漏掉了另一种更容易发生的：清单**完好无损**，
> 只是被更新版本的 `osdk` 写入了本 build 不认识的字段。它同样会落进 `FailClosed`，
> 而且因为 shim 的执行路径用的是 `ScanOptions::default()`，后果比损坏更重 ——
> 所有工具（含与 conda 无关的 `cargo`、`go`）全部不可用，且自锁。
>
> 这一半由 [007](../bugs/007-shim-refuses-all-tools-on-unknown-option.zh-CN.md) 修完：
> 版本偏斜与数据损坏分成两类诊断，前者跳过并告警，后者仍然 fail-closed。
>
> 教训：当时把这个问题定义为「损坏怎么处理」，于是只在「损坏」这条线上找解法。
> 真正的问题是「读不懂的清单怎么处理」，而读不懂有两个来源，其中一个不是故障。

### 4.3 让 `conda::list_installed` 只扫自己那一支

不再遍历整个 installs 根目录，从根上消除「zig 和 android 的文件拖慢 conda」这一类无谓开销。

### 4.4 许可证在开工前统一预检

`install_requests` 在任何工具开始安装之前，先对批次里所有 android 请求跑一遍许可证检查，缺少同意就整批拒绝。这不新增任何权限判断——backend 内部那道门禁仍然是最终裁决者——只把一次必然发生的失败提前到不会牵连他人的时刻。

判定逻辑抽成 `AndroidBackend::gated_packages`，由 `install` 的门禁与预检共用：一个与真实门禁不一致的预检比没有更糟，要么拦下本可成功的安装，要么放过之后仍会失败的安装。实现上不给 `Backend` trait 加方法（新方法会进 vtable 从而无法从 shim 裁剪掉，体积是刻意守住的指标），而是沿用 android doctor / licenses 命令已有的做法，直接构造具体 backend。

新旧二进制在同一场景下的对照：

| 观测 | 修复前 | 修复后 |
| --- | --- | --- |
| 输出中的 `installing ...` 行 | 2 条（含 `installing conda:nasm@2.16.3`） | **0 条** |
| store/cache 中被浪费的字节 | 3.07 MB | 1.45 MB（仅 manifest） |
| 报错内容 | 许可证清单 + 接受方式 | 不变 |

同时验证预检没有变成永久拦路：带 `-o accept-license=android-sdk-license` 时 `install` 仍 rc=0 正常安装；不含 android 的批次一次 manifest 都不会读。

---

## 附：证据来源

全部数据来自 2026-09-13 本机实测，环境为 Windows x64 / `osdk 0.0.2` / `E:\osdk-bin\osdk.exe`。涉及的源码位置以本次调查时的工作树为准：

- `crates/osdk-cli/src/commands.rs`：`reshim`(3778)、`all_display_backends`(1449)、`dynamic_scan_report`(5792)、`generate_shims_for`(3894)、`installed_shim_owners`(5945)、`ensure_no_shim_conflicts`(6072)、`request_selects_installed_version`(5875)
- `crates/osdk-core/src/inventory.rs`：`scan_installs`(382)、`ScanOptions`(178)、`impl Default`(189)、`CorruptManifestPolicy`(170)、`DEFAULT_MAX_DEPTH`(24)
- `crates/osdk-core/src/backend/conda.rs`：`list_installed`(1077)、完成标记过滤(1090)、`bin_names`(1121)
- `crates/osdk-core/src/backend/dynamic.rs`：`finalize_artifact_install`(217)
- `crates/osdk-core/src/lock.rs`：`FileLock`
- `crates/osdk-core/src/shim/mod.rs`：`validated_dynamic_install`(351)、`dynamic_bin_ownership`(427)
- `crates/osdk-cli/src/commands.rs`：`install_requests` 的 `buffer_unordered` + `try_collect`（并发取消的来源）、`preflight_android_licenses`（本次新增）
- `crates/osdk-core/src/backend/android.rs`：`install` 内的许可证门禁、`gated_packages` 与 `pending_licenses`（本次新增）

测试期间创建的临时 data 目录与项目目录均已删除，用户真实的 `E:\osdk-data` 未受影响（12 个 conda 工具完好）。
