# 安装、切换与查询

本页是日常最常用的命令：安装与锁定、设置当前版本、查看本地与远程版本、临时执行，以及版本别名和工具名别名。工具专用选项见各[语言与运行时](../runtimes/overview)页面，lock 的精确读写规则见[可复现锁文件](../project/lockfiles)。

## 安装、锁定、检查和升级

```text
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...] [--include-lazy]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
```

| 命令 | 行为 |
| --- | --- |
| `install` | 安装一个或多个工具并生成 shim；裸项目安装优先消费当前平台 lock，缺失时从配置解析并写回精确 lock；除非带 `--include-lazy`，否则跳过 lazy 条目 |
| `lock` | 解析请求并写入按平台分区的 `osdk.lock`，不安装；Rust 浮动 channel 仍保存为 channel 名 |
| `outdated` | 重新解析配置或显式请求，报告目标精确版本尚未安装的工具；不读取 lock |
| `upgrade` | 重新解析、安装，并刷新 lock；不以旧 lock 为输入 |

`-o/--opt` 可重复且必须为 `KEY=VALUE`。同一键后值覆盖前值；在多工具调用中，
同一组选项会应用到每个工具，因此不要把只属于一个 backend 的选项混用于不同工具。

```bash
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk install --include-lazy
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk lock node@20 -o arch=arm64
osdk outdated node@20 python@3.12
osdk upgrade
```

显式 `install TOOL...` 只做一次性共享安装，不修改项目 `[tools]` 或 lock。要把工具加入项目，
使用 `osdk use TOOL@VERSION`；它会安装工具，并原子更新项目配置与当前平台 lock。

结构化 `[tools]` 条目可设置 `lazy = true`，使裸安装默认跳过它；
`--include-lazy` 会把所有这类条目纳入安装。显式点名工具已经表达安装意图，因此不需要该
开关。被纳入工具所需的 runtime 依赖即使自身标为 lazy，也仍会安装。

更精确的读写矩阵、跨平台区段和陈旧 lock 行为见[可复现锁文件](../project/lockfiles)。
Rust 的 `stable`、`beta`、`nightly` 等浮动 channel 不会被锁成具体发行版本；需要
不可变重建时应使用明确版本或带日期的 toolchain。

## 设置当前版本与卸载

```text
osdk use|u TOOL[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk uninstall|rm TOOL@VERSION
```

`use` 先安装并生成 shim，再写 pin：默认修改最近的项目配置；若不存在，则在当前
目录创建 `osdk.toml`。`--global` 改写用户 `config.toml`。显式输入的版本前缀或通道
会原样保存；裸工具名则保存刚解析出的精确版本。

`uninstall` 通常要求精确版本；前缀会从已安装版本中选择最后一个字符串排序匹配项，
其他非精确请求会拒绝。裸 `rust` 是例外，会卸载 `stable`。该命令需要交互确认，
自动化中应传 `--yes`。卸载普通动态工具时，若用户全局配置仍选择被删除的版本，
osdk 会同步移除该全局 pin 与对应用户 lock 条目；项目配置和项目 lock 不变。卸载完成后
会回收刚变为无引用的 CAS 对象。Shell 激活遇到已配置但未安装的运行时会明确失败，
不会静默使用系统 PATH 上的同名命令。

## 查看本地与远程版本

```text
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
osdk current TOOL --sources [--project DIR] [--json] [--probe]
osdk where TOOL[@VERSION]
osdk reshim
```

| 命令 | 参数与精确语义 |
| --- | --- |
| `list [TOOL]` | 列出带完成标记的本地版本；无参数时包括全部已注册 backend 和磁盘上发现的 inventory 型动态工具，包括 GitHub、Cargo 与 Go command 工具 |
| `list-remote TOOL [FILTER]` | 列出远端稳定版本；可选 `FILTER` 是字符串前缀，不列预发布版本 |
| `current [TOOL]` | 显示当前目录解析到的原始版本请求和来源；不保证版本已安装，也不是远端解析后的精确版本 |
| `where TOOL[@VERSION]` | 带显式选择器（含 `21`、`21.0.12` 这类前缀或省略 build 号的版本）时，只按与安装相同的匹配规则在已安装版本中选择，选不到即报错，不读取项目当前版本；裸工具名解析当前激活版本（项目生态文件、配置、动态 shim 请求），都没有时取已安装列表最后一项 |
| `reshim` | 为所有已安装的内置及 inventory 型动态工具重新生成 shim，并协调 npm/npx 路由 |

因此，`osdk current node` 与 `osdk where node` 回答的是不同问题：前者显示项目
选择（不要求已安装），后者给出一个已安装目录——裸工具名跟随当前激活版本，显式
`where node@<选择器>` 则严格按选择器在已安装版本中定位。脚本需要确定路径时应写
明选择器。

## 临时执行

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

`-t/--tool` 至少一个且可重复。osdk 先安装指定版本，再把 backend bin 和环境加入
单个子进程；不修改项目 pin，也不读取 lock。

**不带 `@` 时继承当前生效的 pin。** `-t java` 表示「这个项目选定的那个 Java」，按
[工作目录解析优先级](../../internals/resolution#工作目录解析优先级)向上查找
`osdk.toml`、`.tool-versions` 与惯用版本文件；只有写成 `-t java@<选择器>` 才覆盖它，
包括显式写 `@latest`。这条规则对 `install`、`exec`、`lock`、`outdated`、`upgrade`
一致。什么都没配置时才回退到 `latest`。

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool node@20 --tool pnpm@10 -- pnpm test
osdk exec -t bun@latest -- bunx vite
osdk exec -t java -- ./gradlew build   # 用项目 pin 的 JDK，无需重复写版本
```

`pnpx` 路由到受管 `pnpm dlx`，`bunx` 路由到受管 `bun x`；未声明对应 backend
会报错。包管理器调用可能在启动前执行 Registry 预检。子命令失败会使 osdk 返回
错误，但当前不保证原样透传其数值退出码。

## 版本别名

```text
osdk alias set TOOL NAME TARGET
osdk alias list [TOOL]
osdk alias unset TOOL NAME
```

```bash
osdk alias set node maintenance 20
osdk alias set node default maintenance
osdk alias list node
osdk use node@default
osdk alias unset node maintenance
```

CLI 始终在用户全局配置中编辑别名；项目也可手写 `[alias.tools.<tool>]` 并覆盖同名
全局别名。别名可以链式引用，但循环会被拒绝。名称不能留空、包含空白或 `@`，
也不能使用 `latest`、`current`、`stable`、`system`、`lts`、`lts/*`、
`lts-latest` 以及任何 `lts/`、`lts-` 前缀。工具别名会规范化后保存，例如
`nodejs` 保存为 `node`。

这类别名只替换**版本请求**：`install`、`use`、`uninstall`、激活、shim 选择与全局
npm 工具解析版本时都会展开它；它不会给可执行文件改名，也不会复制一份安装。
`[alias]` 作为类别命名空间是为将来的 `alias.shell` 等能力预留的，目前只实现
`alias.tools`，写入未知类别会明确报错。

## 工具名别名

| 输入 | 规范 backend |
| --- | --- |
| `nodejs` | `node` |
| `py`, `cpython` | `python` |
| `jdk`, `openjdk` | `java` |
| `golang` | `go` |
| `rustup` | `rust` |
| `mvn` | `maven` |
| `kotlinc` | `kotlin` |

下一步可阅读[项目与配置](../config/files)，把个人命令变成可共享的项目环境。

使用[来源诊断](../config/tool-selection#inspect-sources)查看支持、存在、缺失和不适用类型及有效排序；默认检查不执行系统工具。
