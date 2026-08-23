# 开始使用

本页介绍 osdk 的通用命令面。工具专用选项见[运行时与生态工具](./runtimes)，
项目发现和配置见[项目与配置](./projects)，lock 的精确读写规则与 Rust 浮动 channel
例外见[可复现锁文件](./lockfiles)。

## 第一个工作流

```bash
# 安装指定版本；前缀会解析为当前可用的最高匹配稳定版
osdk install node@20 python@3.12

# 安装并把用户输入的版本请求写入当前项目
osdk use node@20

# 生成并提交当前平台的解析结果
osdk lock

# 在另一台同平台机器按 lock 安装
osdk install

# 临时运行，不修改项目 pin
osdk exec --tool node@20 -- node --version
```

工具请求通常写成 `TOOL@VERSION`。省略 `@VERSION` 等价于 `latest`；常用请求包括
精确版本 `20.11.1`、前缀 `20`/`20.11`、`latest`/`current`/`stable`、`lts`、
`lts/iron` 和用户别名。不同 backend 支持的通道与范围并不完全相同。

## 全局参数

完整形态为：

```text
osdk [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

全局参数可写在子命令前或后：

| 参数 | 环境变量 | 作用 |
| --- | --- | --- |
| `-v`, `--verbose` | `OSDK_LOG` 可另行设置过滤器 | 可重复；`-v`、`-vv`、`-vvv` 分别提高到 info、debug、trace |
| `-q`, `--quiet` | — | 关闭下载/安装进度；不关闭普通结果，也不代表确认删除 |
| `-j N`, `--jobs N` | `OSDK_JOBS` | 最大并发下载/安装数；CLI 的 `0` 不覆盖配置，执行时至少为 1 |
| `-y`, `--yes` | `OSDK_YES` | 自动确认卸载、清缓存和实际 GC 等操作 |
| `--source ID` | — | 本次调用把 `ID` 移到来源候选首位并保留回退；工具请求须使用规范 backend ID（如 `node`，不能用 `nodejs`） |
| `--refresh-sources` | — | 为 `install`、`use`、`upgrade`、`exec` 强制重新探测；`model pull` 仅在无显式 endpoint/pin、选择策略为 `auto` 且非 offline 时刷新；当前不影响 `lock`、`outdated`、`list-remote` |
| `--offline` | `OSDK_OFFLINE` | 禁止网络，只使用缓存的 metadata 与 artifact |
| `--require-checksums` | `OSDK_REQUIRE_CHECKSUMS` | 没有普通 checksum 或可信 attestation 摘要时拒绝 artifact |
| `--attestations POLICY` | `OSDK_ATTESTATIONS` | `off`、`if-available` 或 `required` |
| `--prerelease POLICY` | `OSDK_PRERELEASE` | `never`、`if-explicit` 或 `allow` |
| `--lang LANG` | `OSDK_LANG` | `en` 或 `zh`；同时影响帮助和参数错误 |
| `-h`, `--help` | — | 显示帮助 |
| `-V`, `--version` | — | 显示版本 |

CLI 布尔开关用于开启本次行为，不能用同一个开关把配置中的 `true` 关回 `false`。
例如关闭签名验证只能通过 `OSDK_VERIFY_SIGNATURES=false` 或配置文件完成。

## 安装、锁定、检查和升级

```text
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
```

| 命令 | 行为 |
| --- | --- |
| `install` | 安装一个或多个工具并生成 shim；无工具且无 `-o` 时优先消费当前平台 lock |
| `lock` | 解析请求并写入按平台分区的 `osdk.lock`，不安装；Rust 浮动 channel 仍保存为 channel 名 |
| `outdated` | 重新解析配置或显式请求，报告目标精确版本尚未安装的工具；不读取 lock |
| `upgrade` | 重新解析、安装，并刷新 lock；不以旧 lock 为输入 |

`-o/--opt` 可重复且必须为 `KEY=VALUE`。同一键后值覆盖前值；在多工具调用中，
同一组选项会应用到每个工具，因此不要把只属于一个 backend 的选项混用于不同工具。

```bash
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk lock node@20 -o arch=arm64
osdk outdated node@20 python@3.12
osdk upgrade
```

更精确的读写矩阵、跨平台区段和陈旧 lock 行为见[可复现锁文件](./lockfiles)。
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
自动化中应传 `--yes`。卸载完成后会回收刚变为无引用的 CAS 对象。

## 查看本地与远程版本

```text
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
osdk where TOOL[@VERSION]
osdk reshim
```

| 命令 | 参数与精确语义 |
| --- | --- |
| `list [TOOL]` | 列出带完成标记的本地版本；无参数时包括全部已注册 backend 和磁盘上发现的 GitHub backend |
| `list-remote TOOL [FILTER]` | 列出远端稳定版本；可选 `FILTER` 是字符串前缀，不列预发布版本 |
| `current [TOOL]` | 显示当前目录解析到的原始版本请求和来源；不保证版本已安装，也不是远端解析后的精确版本 |
| `where TOOL[@VERSION]` | 精确版本直接定位；非精确请求不会读取项目当前版本，而是选择已安装列表的最后一项 |
| `reshim` | 为所有已安装的内置 backend 重新生成 shim，并协调 npm/npx 路由 |

因此，`osdk current node` 与 `osdk where node` 回答的是不同问题：前者显示项目
选择，后者在未给精确版本时只选择一个已安装目录。脚本需要确定路径时应写精确版本。

## 临时执行

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

`--tool` 必填且可重复。osdk 确保这些工具已安装，组合精确的 `PATH` 和 backend
环境，然后只启动一次 `COMMAND`；它不读取项目 lock，也不修改项目 pin。

```bash
osdk exec --tool python@3.12 -- python -c "print('ok')"
osdk exec --tool node@20 --tool pnpm@10 -- pnpm install
```

`pnpx` 会改写为受管 `pnpm dlx`，`bunx` 会改写为受管 `bun x`；对应 backend 必须
同时出现在 `--tool` 中。包管理器命令还可能执行 [Registry 预检](./package-managers#registry-预检)。
子进程失败会使 osdk 返回错误，但当前不保证原样透传子进程退出码。

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

CLI 始终在用户全局配置中编辑别名；项目也可手写 `[aliases.<tool>]` 并覆盖同名
全局别名。别名可以链式引用，但循环会被拒绝。名称不能留空、包含空白或 `@`，
也不能使用 `latest`、`current`、`stable`、`system`、`lts`、`lts/*`、
`lts-latest` 以及任何 `lts/`、`lts-` 前缀。工具别名会规范化后保存，例如
`nodejs` 保存为 `node`。

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

下一步可阅读[项目与配置](./projects)，把个人命令变成可共享的项目环境。
