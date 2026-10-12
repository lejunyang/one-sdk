# 运行时总览

osdk 用同一套命令管理各语言运行时。本页给出通用命令形态与各 backend 速览，具体选项见左侧每种语言的专页；JavaScript 包管理器见[包管理器](../tools/package-managers)，任意 GitHub Release 工具见[GitHub Release 工具](../tools/github-releases)。

## 通用命令形态

```text
osdk install TOOL[@VERSION]... [-o|--opt KEY=VALUE ...]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk use|u TOOL[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
osdk where TOOL[@VERSION]
osdk uninstall|rm TOOL@VERSION
```

`-o/--opt` 可重复，必须写成 `KEY=VALUE`。它会应用到本次调用中的每个工具；
一次命令混合不同 backend 时，不要传只适用于其中一个 backend 的选项。

已有结构化工具若被 `when` 排除在当前平台之外，`osdk use TOOL@VERSION` 只更新它的
`version`，并保留 `when`、`lazy` 与 backend 选项；它不会绕过平台条件执行安装，也不会
为当前平台生成该工具的 lock 条目。此时不能同时修改 `-o/--opt`，backend 选项应在匹配的
平台上更新并验证。

内联选项块写成 `tool[key=value,...]@selector`，选项块在 `@` 之前。**在 PowerShell 下要给
整个操作数加引号**：它把参数内未加引号的逗号当数组分隔符，会把一个表达式拆成两个参数。

```bash
osdk install 'npm:esbuild[installer=pnpm,allow_builds=true]@0.21'
```

选项值本身含逗号时再套一层引号：`allow_builds="a,b"`。不加引号被拆开时，osdk 会识别出
这种情况并给出加好引号的完整命令，而不是报括号没写完。

## 后端速览

| backend | 工具名别名 | 生态版本文件 | 专用安装选项 |
| --- | --- | --- | --- |
| `node` | `nodejs` | `.nvmrc`、`.node-version`、`package.json` | `arch`、`corepack` |
| `python` | `py`、`cpython` | `.python-version` | `variant`、`tag` |
| `java` | `jdk`、`openjdk` | `.java-version`、`.sdkmanrc` | `distribution`、`package-type` |
| `go` | `golang` | `go.mod`、`.go-version` | 无 |
| `rust` | `rustup` | `rust-toolchain.toml`、`rust-toolchain` | `profile`、`components`、`targets` |
| `maven` | `mvn` | `.mvn-version` | 无 |
| `gradle` | — | `.gradle-version` | 无 |
| `kotlin` | `kotlinc` | `.kotlin-version` | 无 |
| `zig` | — | `.zig-version` | 无 |

活动版本的完整来源优先级，以及无参数生命周期命令对生态文件的当前限制，见
[项目版本发现](../project/discovery#项目版本发现)。
