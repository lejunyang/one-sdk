# 007 — 清单中一个未知选项让 shim 拒绝执行**所有**工具

**状态**：待修复 · **严重度**：严重 · **实测环境**：Windows x64、osdk 0.0.2

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

## 修复方向

1. **扫描单条失败不应否决全局**：无法解析的清单条目应被跳过并告警，而不是让整次 inventory scan 失败。shim 要执行的那个工具若不在坏条目里，就应当照常运行。
2. **未知选项在 shim 侧应可容忍**：shim 从不安装任何东西，它只需要知道「这个 install root 在哪、有哪些 bin」。选项 schema 的严格校验对 `osdk use` 是必要的（能及早拦住拼写错误），但对 shim 是过度的 —— 它应当忽略不认识的选项而非拒绝服务。**注意**：这不等于放弃校验，identity 相关字段仍须严格比对，否则会执行到错误的安装。
3. **版本偏斜要显式处理**：清单里已有 `schema` 字段，应利用它区分「schema 更新、我读不懂」与「数据损坏」。前者提示用户升级 shim，后者才是错误。
4. **让 shim 的自举不被自己挡住**：`osdk-shim --version` 这类不需要 inventory 的调用，不应触发 inventory scan。

## 回归防线

- 造一份含未知选项的 `.osdk-install.json`，断言 shim **仍能**执行另一个不相关的工具（修复前必红）。
- 断言 `osdk-shim --version` 不读 inventory。
- 断言坏条目会产生告警，而不是被静默忽略 —— 否则会掩盖真正的损坏。
- 版本偏斜用例：用「CLI 新、shim 旧」的组合装一个带新选项的 tool，断言其他 tool 不受影响。

## 备注

本缺陷是在实现 `with`、替换二进制时被动撞见的，不是设计评审发现的。它与 006 有一条共同的教训：**跨进程边界的数据格式演进，必须假设读侧比写侧旧**。006 是「serde 默认值让旧数据静默沿用旧语义」，007 是「旧读侧遇到新字段直接拒绝服务」 —— 一个太宽容，一个太严格，都出在同一个「谁先升级」的假设上。

另一条教训是关于验证的：这个缺陷本可以在替换二进制前就想到，但当时只验证了「新二进制行为正确」，没有验证「新旧混合状态下会怎样」。**升级路径本身也是需要测试的行为。**
