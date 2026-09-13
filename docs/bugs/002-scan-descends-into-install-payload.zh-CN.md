# 002 — 扫描下探到安装包内部，`hook-env` 每次多花几百毫秒

**状态**：部分修复（剪枝已落地，深度上限的大头待做） · **严重度**：中 · **实测环境**：Windows x64、PowerShell 7、osdk 0.0.2

## 现象

`osdk hook-env` 单次约 400~500 ms（`E:\Projects\omy` 504 ms、无项目配置的目录 395 ms）。它由 004 的 shell 钩子在每个提示符执行一次，所以这个数字直接变成"每敲一条命令的等待"。

## 根因

`compute_env_delta`（`activate/mod.rs:227`）调 `scan_dynamic_installs`，后者用 `walkdir` 遍历整个 `installs` 树找 `.osdk-install.json`。

浪费来自两处，**大小差别很大**：

1. **下探到 install 内部（本次已修）**。manifest 位于 install root，而 `is_canonical_install_root`（`dirs.rs:144`）只接受 `installs/<tool>/<version>/<install_id>`，所以 install 内部不可能再有合法 install root。继续下探只是遍历解包后的负载——一个 conda prefix 里是完整的 Python/mingw 发行版。
2. **对不含 manifest 的顶层目录也下探 8 层（未修）**。`android-sdk`、`zig`、`node`、`java` 这些静态 backend 永远不写 manifest，但扫描并不知道，仍要走满深度。

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

| 策略 | 目录 | stat 条目 | 秒 |
| --- | --- | --- | --- |
| 修复前（深度 8，不剪枝） | 8,268 | 84,352 | 0.70 |
| **本次修复（深度 8 + 剪枝）** | **7,268** | **65,736** | **0.64** |
| 假想：深度收窄到 4 | 466 | 4,483 | 0.04 |

端到端（新旧二进制对比，`-NoProfile` + `Process` API，输出逐字节一致）：

| 命令 | 修复前 | 修复后 | 加速 |
| --- | --- | --- | --- |
| `hook-env` @ omy | 504 ms | 424 ms | 1.2x |
| `hook-env` @ 无项目配置 | 395 ms | 357 ms | 1.1x |
| `osdk list` | 443 ms | 341 ms | 1.3x |

拆解修复后的 424 ms：进程启动基线（`--version`）12 ms，配置/trust/registry/渲染约 8 ms，**扫描仍占约 413 ms**。

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

### 正确的方向

按 backend 区分：只进入「可能存在动态安装」的顶层目录。动态工具 id 必含 `:`，`sanitize_tool_id` 按 `:` 分段，所以顶层目录名就是 backend 名（`conda`、`npm`、`github`…）。静态 backend（`node`、`java`、`zig`、`android-*`）从不写 manifest，其子树可以完全不进——与深度无关，因此不会漏扫任何动态工具。

本机效果：只需进入 `conda` 一个目录（1,602 个），而非全树 10,305 个。

尚未实施的原因：`scan_installs` 有 40+ 处调用（含体积敏感的 `osdk-shim`），传入 backend 集合是接口变更，应作为独立提交并单独量体积。

## 回归防线

- `scan_stops_at_a_manifest_and_never_walks_the_payload` — 在 install root 内部两级处植入一个名为 `.osdk-install.json` 的坏文件。剪枝生效时它永远不被打开；不剪枝则会被读取、解析失败并产生诊断。
- `pruning_still_finds_every_sibling_install` — 三个同级 install 必须全部找到，防止把剪枝写成"跳过整个 tool 目录"。

### 变异测试记录（两个教训）

**教训一：断言前提写错，测试等于空的。** 第一版把探针放在深度 `DEFAULT_MAX_DEPTH + 4`，结果**深度上限先把它拦住了**，有没有剪枝都发现不了它——三个变异全部存活。改到 install root 下两级（深度 6，在上限内）后，移除剪枝的变异立刻被抓到。所以探针必须自带断言，确认自己在扫描范围内：

```rust
assert!(probe_depth <= DEFAULT_MAX_DEPTH, "...否则深度上限先隐藏它，这个测试什么都没证明");
```

**教训二：变异存活也可能是变异本身等于空操作。** "连续调用两次 `skip_current_dir`"这个变异存活，原因是 `walkdir` 的第二次调用本身是无操作，并没有改变行为。换成"找到第一个 install 就停止"这个真正的过度剪枝后，6 个测试变红（含新加的 sibling 测试）。
