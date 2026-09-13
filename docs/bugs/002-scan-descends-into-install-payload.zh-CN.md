# 002 — 扫描下探到安装包内部，`hook-env` 每次多花几百毫秒

**状态**：已修复（两阶段：manifest 剪枝 + 按 namespace 裁剪） · **严重度**：中 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

## 现象

`osdk hook-env` 单次约 400~500 ms（`E:\Projects\omy` 504 ms、无项目配置的目录 395 ms）。它由 004 的 shell 钩子在每个提示符执行一次，所以这个数字直接变成"每敲一条命令的等待"。

## 根因

`compute_env_delta`（`activate/mod.rs:227`）调 `scan_dynamic_installs`，后者用 `walkdir` 遍历整个 `installs` 树找 `.osdk-install.json`。

浪费来自两处，**大小差别很大**：

1. **下探到 install 内部（本次已修）**。manifest 位于 install root，而 `is_canonical_install_root`（`dirs.rs:144`）只接受 `installs/<tool>/<version>/<install_id>`，所以 install 内部不可能再有合法 install root。继续下探只是遍历解包后的负载——一个 conda prefix 里是完整的 Python/mingw 发行版。
2. **对不含 manifest 的顶层目录也下探 8 层（第二阶段已修）**。`android-sdk`、`zig`、`node`、`java` 这些静态 backend 永远不写 manifest，但扫描并不知道，仍要走满深度。这是大头，占 88%。

## 实测证据

本机 `installs` 树：87,804 个文件、10,305 个目录，manifest 只有 14 个，全部在深度 4（`conda/<pkg>/<version>/<install_id>`）。

按顶层目录看遍历量：

| 顶层目录 | 目录数 | 文件数 | 含 manifest |
| --- | --- | --- | --- |
| android-sdk | 4,309 | 29,129 | 无 |
| zig | 2,242 | 34,225 | 无 |
| **conda** | **1,602** | **21,219** | **14 个** |
| android-ndk | 945 | 16,719 | 无 |
| node | 544 | 2,137 | 无 |

各策略的遍历量（找到的 manifest 均为 14 个，结果一致）：

| 策略 | 目录 | stat 条目 | 秒 | 找到 manifest |
| --- | --- | --- | --- | --- |
| 修复前（不剪枝） | 9,764 | 101,486 | 2.89 | 14 |
| 第一阶段（manifest 处剪枝） | 8,764 | 82,377 | 2.25 | 14 |
| **第二阶段（再加 namespace 裁剪）** | **1,555** | **16,686** | **0.45** | **14** |

三种策略找到的 14 个 manifest 完全一致——省掉的全是白走的路。累计遍历目录省 84%、stat 次数省 84%。

裁剪后各顶层目录被走进的目录数：

| 顶层目录 | 走进 | 说明 |
| --- | --- | --- |
| `go` | 1,496 | namespace，走全深度（见下方遗留问题） |
| `conda` | 45 | namespace，14 个 manifest 都在这里 |
| 其余 13 个（android-*/zig/node/java/…） | 各 1 | 浅探一层即返回 |

端到端（新旧二进制对比，`-NoProfile` + `Process` API，输出逐字节一致）：

| 命令 | 第一阶段后 | 第二阶段后 | 加速 |
| --- | --- | --- | --- |
| `hook-env` @ one-sdk | 642 ms | 306 ms | 2.1x |
| `hook-env` @ 无项目配置 | 677 ms | 314 ms | 2.2x |
| `osdk list` | 728 ms | 218 ms | 3.3x |

两个版本交替测量、各取 7 次中位数，输出**逐字节一致**。

第一阶段单独的收益只有 1.2x（504 → 424 ms），当时如实记为"部分修复"。

## 影响

每个提示符都要付一次。而且它**随无关的 SDK 增长**：android-sdk 和 zig 占了 63% 的遍历量，却与动态工具毫无关系——用户装的静态 SDK 越多，提示符越慢。

## 修复

发现 manifest 后 `walker.skip_current_dir()`，不再下探安装负载。两处（当前格式与 legacy 格式）都要做。

**这只解决了 12% 的浪费，是小头**。剩下 88% 需要另一种手段：不对静态 backend 的子树下探 8 层。

### 为什么不收窄 `max_depth`（已实测否决）

看起来最省事的做法是把深度上限从 8 收窄——本机实测收益极大：

| 深度上限 | 目录 | stat 条目 | 秒 | 找到 manifest |
| --- | --- | --- | --- | --- |
| 5 | 815 | 7,575 | 0.08 | 14 |
| 6 | 4,505 | 37,072 | 0.37 | 14 |
| 9（当前） | 7,268 | 65,736 | 0.62 | 14 |

**但这是错的**，因为本机只装了 `conda:xxx` 这类两段 id。枚举各类动态 id 展开后的实际深度：

| tool id | sanitize 后 | manifest 深度 |
| --- | --- | --- |
| `conda:nasm` / `npm:prettier` / `pypi:black` | `conda/nasm` | 5 |
| `npm:@scope/pkg` / `github:owner/repo` | `github/owner/repo` | 6 |
| **`go:github.com/user/cmd/tool`** | `go/github.com/user/cmd/tool` | **8** |

收窄到 5 会让 `go:`、`github:`、`npm:@scope` 的安装**被漏扫**——扫不到的动态工具等于不存在，路由和激活都会失效。这是正确性缺陷，不能用来换 8.9x 的速度。`max_depth = 8` 正是 `MAX_TOOL_ID_SEGMENTS(5) + version + install_id` 的上界，是算出来的，不是随手设的。

### 第二阶段：按 namespace 裁剪

动态 id 必含 `:`，`sanitize_tool_id` 按 `:` 分段，所以 `installs/<首段>` 就是 namespace。静态 backend 从不写 manifest，其子树不可能有——与深度无关，因此不会漏扫任何动态工具。

原先担心这需要改接口（`scan_installs` 有 40+ 处调用，含体积敏感的 `osdk-shim`），实际不需要：`namespace_schema`（`tool.rs:543`）本来就在 osdk-core 内、无 feature gate、不依赖 Registry，直接在遍历判据里查即可，调用点一处都不用动。

#### 浅探，而不是整棵跳过

第一版按白名单硬跳过非 namespace 目录，**被测试否决**——三个失败各自都是真问题：

| 失败的测试 | 暴露的问题 |
| --- | --- |
| `scan_rejects_current_manifest_at_wrong_identity_root` | 放错位置的 manifest 变成**静默通过**。那是阻止伪造 manifest 劫持命令路由的防线，不能为性能牺牲。 |
| `legacy_files_are_detected_but_never_parsed_as_installs` | 同上，legacy 格式也一并看不见了。 |
| `scoped_queries_filter_versions_before_selection` | **真实漏扫**：npm 全局安装落在 `installs/npm-global/` 下（`npm_package.rs:24` 的 `GLOBAL_INSTALL_NAMESPACE`），而 `npm-global` 并不是 namespace，被裁掉了。 |

改为浅探：非 namespace 的顶层目录**仍然进入**，但深度 ≥2 时不再下探。这样"直接放在顶层目录里"的 manifest 照样被发现并拒绝，而深树不再被走穿。判据用 `identity_root == scan_root` 区分全树扫描与 `scan_installs_for_tool`——后者从 tool 子树开始，深度 1 是 *version* 目录，误用会把正在扫的那个 tool 裁掉。

#### namespace 列表必须只有一处

`namespace_schema` 改为从新增的 `DYNAMIC_NAMESPACES` 查找，而不是列表与 `match` 各写一份。理由就是上面那个 `npm-global`：**新增 backend 时若只改了 match，整个 backend 的安装都会对扫描隐形**，而这种缺陷不会立刻报错，只会表现为"装了却用不了"。派生目录名另有 `DERIVED_INSTALL_DIRECTORIES` 覆盖，注释里写明新增时必须让新名字从这里可达。

### 遗留：`go` 占了裁剪后的 96%

裁剪后总共走 1,555 个目录，其中 1,496 个在 `go` 下——而本机 14 个 manifest 全在 conda 下，`go` 下一个都没有。

原因是 `installs/go/` 被两种东西共用：Go 语言本体（静态工具，`installs/go/1.26.5/`）和 `go:` 动态工具（`installs/go/github.com/user/.../`）。目录名一样，无法在深度 1 区分，所以只能按 namespace 放行、走全深度，白走了 Go 的整棵源码树。

进一步优化需要别的判据（例如按已配置/已知 tool 收窄，参考 `scan_installs_for_tool`），尚未评估。当前收益的天花板由它决定。

## 回归防线

- `scan_stops_at_a_manifest_and_never_walks_the_payload` — 在 install root 内部两级处植入一个名为 `.osdk-install.json` 的坏文件。剪枝生效时它永远不被打开；不剪枝则会被读取、解析失败并产生诊断。
- `pruning_still_finds_every_sibling_install` — 三个同级 install 必须全部找到，防止把剪枝写成"跳过整个 tool 目录"。
- `a_static_backend_subtree_is_probed_but_never_walked` — 往静态子树深处（`zig/0.13.0/deadbeef`，形似合法 install root）埋一个 identity 与路径不符的 manifest。走到它，fail-closed 扫描必然报错；扫描成功即证明没走进去。
- `a_manifest_sitting_directly_in_a_static_directory_is_still_rejected` — 反方向：放在 `node/` 里的 manifest 仍必须被拒绝。这条与上一条成对，缺了它就会退回被否决的硬裁方案。

两条的变异测试结果正交，各守一个方向：

| 变异 | `..._never_walked` | `..._still_rejected` |
| --- | --- | --- |
| 判据恒真（取消裁剪） | **被杀死** | 存活 |
| 深度阈值 2→1（连浅探也不做） | 存活 | **被杀死** |

### 变异测试记录（两个教训）

**教训一：断言前提写错，测试等于空的。** 第一版把探针放在深度 `DEFAULT_MAX_DEPTH + 4`，结果**深度上限先把它拦住了**，有没有剪枝都发现不了它——三个变异全部存活。改到 install root 下两级（深度 6，在上限内）后，移除剪枝的变异立刻被抓到。所以探针必须自带断言，确认自己在扫描范围内：

```rust
assert!(probe_depth <= DEFAULT_MAX_DEPTH, "...否则深度上限先隐藏它，这个测试什么都没证明");
```

**教训二：变异存活也可能是变异本身等于空操作。** "连续调用两次 `skip_current_dir`"这个变异存活，原因是 `walkdir` 的第二次调用本身是无操作，并没有改变行为。换成"找到第一个 install 就停止"这个真正的过度剪枝后，6 个测试变红（含新加的 sibling 测试）。

**教训三：在基准里复刻被测判据，等于没测。** 第二阶段起初在基准里另写了一个 `measure_scan_walk`，复刻裁剪判据来数"扫描走过多少目录"。它与产品代码完全脱钩——把 `inventory.rs` 的判据改成恒真，它照样输出同一个数字（37 → 37），变异一动不动地存活。

改成诱饵式：埋一个 identity 与路径不符的 manifest，让失败信号来自被测代码自己的行为（fail-closed 扫描报错）。同一个变异立刻被杀死，退出码 101。**判断标准是"这个断言的结果会不会随产品代码改变"，而不是"它看起来测得准不准"。**
