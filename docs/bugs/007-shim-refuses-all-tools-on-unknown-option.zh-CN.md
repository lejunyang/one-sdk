# 007 — 清单中一个未知选项让 shim 拒绝执行**所有**工具

**状态**：已修复 · **严重度**：严重 · **实测环境**：Windows x64、osdk 0.0.2

## 现象

在 `with` 选项合入后、`osdk-shim.exe` 尚未同步替换的那个窗口期内，任何经由 shim 的命令都失败了 —— 包括与 conda 毫无关系的 `cargo`：

```
$ cargo build --release -p osdk-shim
osdk-shim: refusing dynamic tool inventory scan because config error:
unsupported option `with` for dynamic backend `conda:m2-base`
at E:\osdk-data\...\m2-base\2022.6.1\b3-v2-dc99b040...\.osdk-install.json
```

连 shim 自己的 `--version` 也一样：

```
$ osdk-shim.exe --version
osdk-shim: refusing dynamic tool inventory scan because config error:
unsupported option `with` for dynamic backend `conda:m2-base` ...
```

**一个 conda 安装清单里的一个不认识的选项，让整台机器上所有由 osdk 管理的工具都不可用。**

## 根因

`osdk-shim` 与 `osdk-cli` 是**分开编译**的两个二进制（shim 刻意不启用 `install` feature，以免把下载与 sigstore 校验链接进去）。因此它们的版本可以不同步：先替换 `osdk.exe` 而后替换 `osdk-shim.exe`，中间就存在一个窗口。

在这个窗口里：

1. 新 `osdk.exe` 安装了带 `with` 的 conda tool，把 `with` 写进了 `.osdk-install.json`；
2. 旧 `osdk-shim.exe` 的选项 schema 里没有 `with`，读该清单时判为 `unsupported option`；
3. 这个错误发生在 **inventory scan** 阶段 —— 而 scan 是全局的、在确定要执行哪个工具**之前**就已完成，所以一条坏记录会让扫描整体失败；
4. shim 选择 fail-closed，于是拒绝执行任何工具。

要命的组合是「全局扫描 + 任一条目出错即整体失败 + fail-closed」。前两条让故障范围从「一个 tool」放大到「所有 tool」，第三条让它从「降级」变成「完全不可用」。

## 为何严重

- **影响面与起因完全不成比例**：起因是一个 conda tool 的一个选项，后果是 `node`、`go`、`cargo`、`java` 全部不可用。
- **自锁**：修复它需要重新编译 shim，而编译要用 `cargo` —— `cargo` 正被这个 shim 拦着。实测必须把 shims 目录从 PATH 摘掉才能构建出新 shim，这对不了解内情的用户是个死结。
- **错误信息把人指向错误方向**：用户看到的是 `cargo build` 失败并抱怨一个 conda 包，两者看不出关系。
- **不限于 `with`**：任何「新版 CLI 写入、旧版 shim 不认识」的清单字段都会触发。这次是 `with` 撞上它，下一个新选项同样会。升级顺序（先 CLI 后 shim）是自然的，甚至 `osdk self-update` 也可能有同样的先后。

## 修复

分三处，都建立在同一个判断上：**扫描从来不是安全边界**。真正要执行的东西都会走
`validated_dynamic_install`，它自己重读 manifest、重新校验 identity 指纹、完成标记
与 provider 证据。而指纹覆盖**全部** material_options，包括本 build 叫不出名字的
那些——所以「跳过一个读不懂的邻居」不会放过任何东西，只是让它不可见。

### 1. 区分「版本偏斜」与「数据损坏」（`inventory.rs`）

新增诊断类别 `InventoryDiagnosticKind::UnrecognizedByThisBuild`，与
`InvalidManifest` 并列。判据 `is_version_skew` 刻意收得很窄，只认两种错误：

- 选项 schema 里没有定义的选项名；
- 本 build 尚不认识的 inventory schema 号。

其余一律保持 fail-closed —— 非法 JSON、指纹不匹配、非规范 identity、未知字段、
坏依赖，这些是真损坏或篡改，拒绝才是对的。

区分开的意义不只是内部分类：两者要求的**用户动作相反**。损坏意味着「删掉这个
安装」，偏斜意味着「两个二进制不同步，升级旧的那个」。混为一谈会让用户以为数据
坏了，而其实什么都没坏。

### 2. shim 的归属扫描改用 tolerant（`osdk-shim/src/main.rs`）

那次扫描只回答「哪个 backend 拥有这个工具名」，属于枚举而非解析执行目标，本就该
用 `ScanOptions::tolerant()`。`osdk-core` 早已提供它（见
`scan_dynamic_installs_tolerant` 的注释），只是 shim 这条路径漏用了。

### 3. `--version` 不再触发任何状态读取（`osdk-shim/src/main.rs`）

自我识别提前到 config / registry / inventory 之前。否则「唯一能看出装的是哪个
shim」的命令，恰好在版本不一致时不可用——而版本不一致正是要诊断的东西。

只有直接以 `osdk-shim` 形式调用时才解析该标志；顶着工具名时 `--version` 属于那个
工具，必须原样转发。判据与 `parse_invocation` 共用一个函数，防止两处漂移。

## 原修复方向

1. **扫描单条失败不应否决全局**：无法解析的清单条目应被跳过并告警，而不是让整次 inventory scan 失败。shim 要执行的那个工具若不在坏条目里，就应当照常运行。
2. **未知选项在 shim 侧应可容忍**：shim 从不安装任何东西，它只需要知道「这个 install root 在哪、有哪些 bin」。选项 schema 的严格校验对 `osdk use` 是必要的（能及早拦住拼写错误），但对 shim 是过度的 —— 它应当忽略不认识的选项而非拒绝服务。**注意**：这不等于放弃校验，identity 相关字段仍须严格比对，否则会执行到错误的安装。
3. **版本偏斜要显式处理**：清单里已有 `schema` 字段，应利用它区分「schema 更新、我读不懂」与「数据损坏」。前者提示用户升级 shim，后者才是错误。
4. **让 shim 的自举不被自己挡住**：`osdk-shim --version` 这类不需要 inventory 的调用，不应触发 inventory scan。

## 回归防线

三条单测（`inventory.rs`），刻意都在 `ScanOptions::default()`（fail-closed）下断言 ——
因为 shim 执行路径用的就是它，只在 tolerant 下通过没有意义：

| 测试 | 断言 | 变异验证 |
| --- | --- | --- |
| `an_install_written_by_a_newer_osdk_does_not_disable_the_healthy_ones` | 带未知选项的清单被跳过、健康工具仍可见、留下 `UnrecognizedByThisBuild` 诊断 | 关掉 `is_version_skew` 后**准确变红** |
| `real_damage_still_fails_closed` | 截断的 JSON 仍然让扫描失败 | 关掉判据后仍绿（正确：不该受影响） |
| `a_tampered_identity_is_not_mistaken_for_version_skew` | 指纹不匹配仍然是硬错误 | 关掉判据后仍绿（正确） |

变异验证的结果本身就是证据：关掉判据时**只有第一条**变红，另两条不动 —— 说明放宽的
范围精确，没有连带削弱 fail-closed 的安全防线。

未知选项的清单是手工拼 JSON 造的，而不是走 `from_identity`：本 build 的 schema 根本
构造不出这样的选项，这正是要模拟的版本偏斜。

### 端到端实测

同一份磁盘状态（`m2-base` 清单含 `with`），旧 shim 与新 shim 对照：

| 命令 | 旧 shim（不认识 `with`） | 新 shim |
| --- | --- | --- |
| `osdk-shim --version` | `refusing dynamic tool inventory scan ...` | `osdk-shim 0.0.2` |
| `go version`（与 conda 无关） | `refusing dynamic tool inventory scan ...` | `go version go1.26.5 windows/amd64` |

第二行是核心：一个 conda 选项曾让完全无关的 `go` 不可用，现在不会了。

## 备注

本缺陷是在实现 `with`、替换二进制时被动撞见的，不是设计评审发现的。它与 006 有一条共同的教训：**跨进程边界的数据格式演进，必须假设读侧比写侧旧**。006 是「serde 默认值让旧数据静默沿用旧语义」，007 是「旧读侧遇到新字段直接拒绝服务」 —— 一个太宽容，一个太严格，都出在同一个「谁先升级」的假设上。

另一条教训是关于验证的：这个缺陷本可以在替换二进制前就想到，但当时只验证了「新二进制行为正确」，没有验证「新旧混合状态下会怎样」。**升级路径本身也是需要测试的行为。**
