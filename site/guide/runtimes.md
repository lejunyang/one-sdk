# 运行时与生态工具

本页覆盖 Node.js、Python、Java/JRE、Go、Rust，以及 Maven、Gradle、Kotlin。
JavaScript 包管理器另见[专页](./package-managers)，任意 GitHub Release 工具见
[下载源与供应链安全](./sources-security#任意-github-release-工具)。

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

活动版本的完整来源优先级，以及无参数生命周期命令对生态文件的当前限制，见
[项目版本发现](./projects#项目版本发现)。

## Node.js

```bash
osdk install node@20
osdk install node@20.19.0 -o corepack=true
osdk lock node@20 -o arch=arm64
```

| 选项 | 值 | 作用 |
| --- | --- | --- |
| `arch` | `x64`、`arm64`、`x86`、`arm` | 选择目标 Node artifact；默认 host 架构 |
| `corepack` | `true|1|yes|on` 或 `false|0|no|off` | 是否用目标 Node 自带的 Corepack 执行 `enable --install-directory <node-bin>` |

`corepack` 未显式传入时取 `[settings.node].corepack`，默认 `false`。启用失败会删除
本次安装，不留下完成标记。Node 的 shim 只负责 `node` 和 `corepack`；npm/npx 由
独立 npm backend 或相应 Node 安装的路由 shim 协调。执行环境还会在用户未设置时
提供共享 `npm_config_cache`。

`arch` 可用于生成其他架构的 lock 区段，但 osdk 没有只下载模式；实际安装会拒绝
不能在当前 host 执行的 Node artifact。

Node 也提供全局包迁移：

```text
osdk node migrate-packages --from VERSION --to VERSION [--apply]
```

```bash
# 只生成计划
osdk node migrate-packages --from 20.19.0 --to 22.17.0

# 执行迁移
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

源、目标都必须是已安装且含 npm 的受管 Node。osdk 通过源版本的
`npm ls -g --depth=0 --json --long` 枚举，跳过 npm 自身、目标已存在的包，以及声明
`hasInstallScript=true` 或 `gypfile=true` 的原生/安装脚本包。`--apply` 按精确版本安装；
失败时恢复目标原有的全局包集合。

## Python

`python@3.14` 是默认 CPython 的简写。完整请求为
`python@IMPLEMENTATION-VERSION+VARIANT`：

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@cpython-3.14+freethreaded+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

| 实现 | 支持的变体 | identity 示例 |
| --- | --- | --- |
| `cpython` | `default`、`freethreaded`、`debug`、`freethreaded+debug` | `3.14.7`、`cpython-3.14.7+debug` |
| `pypy` | `default` | `pypy-3.11.x` |
| `graalpy` | `default` | `graalpy-3.12.x` |
| `pyodide` | `default` | `pyodide-3.14.x` |

| 选项 | 值 | 作用 |
| --- | --- | --- |
| `variant` | 上表中的变体 | 与请求中的 `+VARIANT` 等价；显式选项优先 |
| `tag` | python-build-standalone 发布日期，如 `20240224` | 为经典 CPython 下载固定历史 PBS release |

实现、精确 Python 版本、变体和 catalog artifact 都会进入 lock，不同 identity 可
并存。普通 CPython 使用内置 python-build-standalone 版本索引；多实现、变体和预发布
使用内置的已校验 catalog。自定义 catalog 必须同时指定内容摘要：

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

`catalog_url` 可为 HTTP(S) 或本地路径。只有 SHA-256、schema 与每个 artifact
checksum 全部有效才更新 last-good；刷新失败会尝试 last-good，再回退内置 catalog。
预发布策略见[预发布版本](./sources-security#预发布版本)。

查找解释器：

```text
osdk python find [REQUEST]
```

```bash
osdk python find
osdk python find pypy-3.11
osdk python find 3.14+freethreaded
```

managed 结果按可选请求筛选；随后仍扫描 `PATH` 与系统候选并去重，按
`managed`、`PATH`、`system` 标记输出。完全找不到时返回错误。

## Java JDK 与 JRE

```bash
osdk install java@21
osdk install java@zulu-17.0.1
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install java@21 -o package-type=jre
```

| 选项 | 值 | 默认值 |
| --- | --- | --- |
| `distribution` | Foojay distribution ID，如 `temurin`、`zulu` | `temurin` |
| `package-type` | `jdk` 或 `jre` | `jdk` |

发行版也可写进版本请求，例如 `java@temurin-21`。Foojay 查询按发行版、操作系统、
架构、archive 类型、JDK/JRE 和 Linux libc 过滤。JRE identity 为
`jre-<resolved-version>`，可与相同版本的 JDK 共存；执行 JDK/JRE 时导出 `JAVA_HOME`。

Temurin 版本带 build 号（如 `21.0.12+8`），PSU 还会有第四段（如 `21.0.12.1+1`）。
版本请求可以省略 build 号：`java@21.0.12` 会匹配同一核心版本的 `21.0.12+8`；只有当
同一核心版本不存在时，才回退匹配四段式 PSU。

内置 Temurin LTS catalog 包含 8、11、17、21、25，可在空 metadata 缓存下解析；
已锁定的 artifact 在 Foojay 不可用时也可安装。可设置兼容 Foojay `/packages` 的
端点或静态镜像：

```toml
[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages"
```

## JVM 工具

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

当前 JVM 工具 catalog 是固定集合：Maven 仅 `3.9.16`（SHA-512），Gradle 仅
`9.7.0`（SHA-256），Kotlin 仅 `2.4.10`（SHA-256）。请求其他版本会失败。它们拥有
独立安装目录和 shim；Kotlin 的 GitHub 下载还提供代理候选。

## Go

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go 没有 backend 专用 `-o`。osdk 从 go.dev JSON index 选择当前 OS/架构的
`archive` 并验证索引中的 SHA-256；下载候选包括 go.dev、Aliyun 和
golang.google.cn。执行时导出 `GOROOT`，并提供 `go`、`gofmt`。

## Rust

Rust backend 委托给安装在 osdk 隔离目录中的 rustup：

```bash
osdk install rust@stable
osdk install rust@nightly -o profile=minimal \
  -o components=clippy,rustfmt \
  -o targets=wasm32-unknown-unknown,x86_64-pc-windows-gnu
```

| 选项 | 值 | 默认值 |
| --- | --- | --- |
| `profile` | rustup 接受的 profile | `default` |
| `components` | 逗号分隔的 rustup component | 空 |
| `targets` | 逗号分隔的 rustup target | 空 |

`latest`、`lts` 和 `system` 都映射为 `stable`；`stable`、`beta`、`nightly` 与精确
工具链基本原样交给隔离 rustup。rustup bootstrap 自身固定使用 `minimal` 且不装默认
toolchain。运行时导出 `RUSTUP_HOME=<data>/rustup` 和 `CARGO_HOME=<data>/cargo`。
Lock 也会原样保存这些浮动 channel，因此以后重装 `stable`、`beta` 或 `nightly` 可能
得到更新 toolchain；需要不可变结果时请写明确版本或带日期的 toolchain。

### 暴露的命令与 `cargo install`

安装和 `osdk reshim` 时，osdk 会为活动工具链 `bin` 与隔离 `CARGO_HOME/bin` 里的**全部**
可执行文件生成 shim，而不只是 `rustc`、`cargo`、`rustup`、`rustfmt`、`clippy-driver`
五个核心命令：`rustdoc`、`rust-analyzer`、`cargo-miri` 等 rustup 代理，以及之后用受管
`cargo` 安装的 CLI 都会被暴露。这些 shim 运行时同样注入隔离的 `RUSTUP_HOME`、`CARGO_HOME`，
不会误连到系统 rustup。

用受管 `cargo install` 安装的第三方工具进入 `<data>/cargo/bin`（而不是系统 `~/.cargo`），
装完执行一次 `osdk reshim` 即可让新命令在 shell 中直接可用；`cargo <子命令>` 形式
（如 `cargo tauri`）无需 reshim，cargo 会直接在隔离 `CARGO_HOME/bin` 找到对应程序。

### 与系统 rustup 的管理边界

osdk 只驱动数据目录下的隔离 rustup，不接管已经安装在系统 `PATH` 上的 rustup：

- 未执行过 `osdk install rust` 时，隔离 rustup 不存在，`osdk rust *` 会明确报错并
  提示先安装；它不会转而调用系统 rustup。
- `osdk source pin rust <源>` 只改变 osdk 安装、更新受管工具链时注入的
  `RUSTUP_DIST_SERVER`，不会修改外部 rustup 的环境变量或配置；未安装受管 Rust 时
  命令会额外打印这条作用域提示。要让系统 rustup 走镜像，请自行配置
  `RUSTUP_DIST_SERVER`、`RUSTUP_UPDATE_ROOT` 等环境变量。

### Component 与 target

```text
osdk rust component add NAME [--toolchain TOOLCHAIN]
osdk rust component remove NAME [--toolchain TOOLCHAIN]
osdk rust component list [--toolchain TOOLCHAIN]
osdk rust target add NAME [--toolchain TOOLCHAIN]
osdk rust target remove NAME [--toolchain TOOLCHAIN]
osdk rust target list [--toolchain TOOLCHAIN]
```

`--toolchain` 默认 `stable`，命令直接作用于隔离 rustup。

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust target add wasm32-unknown-unknown --toolchain stable
```

### 状态、override 与本地工具链

```text
osdk rust check [--repair]
osdk rust override import [PATH]
osdk rust override export [PATH]
osdk rust toolchain link NAME PATH
```

- `check` 先执行隔离 `rustup check`；`--repair` 再按真实 toolchain 目录补建缺失 marker，
  并移除没有对应 toolchain 的 marker。
- `override import` 从可选目录（默认当前目录）的隔离 rustup override 读取 toolchain，
  写入最近项目 `osdk.toml`。
- `override export` 把该目录的活动 osdk Rust pin 写成隔离 rustup 的目录 override。
- `toolchain link` 要求 `PATH` 可规范化且包含 `bin/`；`NAME` 不能有空白、斜杠，
  也不能是 `.` 或 `..`。

linked toolchain 可被 shim 和 shell 使用，但它是本机路径，`osdk lock` 会拒绝把它
写成可复现 artifact。
