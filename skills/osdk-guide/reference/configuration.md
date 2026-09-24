# osdk 配置文件编写格式参考

osdk 读两类配置文件，两者都是 TOML，**段与字段完全相同**：

- **项目配置**：`osdk.toml`（或 `.osdk.toml`），从当前目录向上查找。放项目专属声明，
  可提交进仓库。
- **用户全局配置**：`$OSDK_CONFIG_DIR/config.toml`（`osdk config path` 可打印路径）。放
  跨项目的个人默认。

**合并优先级（高者胜）**：CLI 参数 → 环境变量（`OSDK_*`）→ 项目配置 → 用户全局配置 →
内置默认。字段名对齐 `crates/osdk-core/src/config/mod.rs` 的 `ConfigFile` 及子结构。

**顶层段一览**（`ConfigFile` 的字段即合法顶层段）：

| 段 | 用途 | 需要 trust？ |
| --- | --- | --- |
| `[settings]` | osdk 自身行为默认值 | 否 |
| `[tools]` | 项目/全局固定的工具及版本 | 否（仅声明装什么） |
| `[aliases]` | 版本别名 | 否 |
| `[sources]` | 下载源 / 镜像策略 | 改写来源的键需要 |
| `[registries]` | 依赖 Registry 预检候选 | 是（WeakensVerification） |
| `[containers]` | 原生容器运行时设置 | 镜像改写需要 |
| `[syspkg]` | 宿主系统包声明 | 是（ExecutesCode） |
| `[tasks]` / `[task_config]` | 项目任务 | 是（ExecutesCode，运行时） |
| `[deps]` | 应用依赖 provider | 视字段而定 |
| `[models]` | 声明式模型 | 仅 endpoint/自定义来源需要 |

> **信任规则**：除 `tools` 和 `aliases` 外的每个顶层键都被视为「影响执行或下载来源」，
> 需要 `osdk trust`。仅写 `[tools]` / `[aliases]` 不触发信任。被拒时 osdk 逐条列出是哪些键。

> **不要手写 osdk 管控目录（data/config）里的源与代理配置**：`[sources]` 等应通过
> `osdk source` / `osdk config` 命令改，命令会做校验并保持格式一致。

---

## `[settings]` — osdk 自身行为

对应 `Settings` 结构。这些也可用 `osdk config set <key> <value>` 写入（键名见下方「config
键名」列）。

| 字段 | 类型 / 取值 | 默认 | 说明 |
| --- | --- | --- | --- |
| `link_mode` | 见下 | 平台自适应 | store 对象如何物化进安装目录 |
| `jobs` | 正整数 | CPU 数（上限 8） | 最大并发下载 / 安装 |
| `yes` | bool | `false` | 对提示默认「是」 |
| `verify_signatures` | bool | `true` | backend 提供签名时是否校验 |
| `require_checksums` | bool | `false` | 无 checksum 的产物是否拒绝 |
| `attestations` | `off` / `if-available` / `required` | `if-available` | GitHub attestation 策略 |
| `offline` | bool | `false` | 只用缓存，不联网 |
| `lang` | `en` / `zh` | 自动（按 locale） | 输出语言 |
| `prerelease` | `never` / `if-explicit` / `allow` | `if-explicit` | 预发布解析策略 |

子表：

```toml
[settings]
jobs = 4
require_checksums = true
attestations = "if-available"

[settings.node]
corepack = false            # 安装后是否跑 node 自带的 corepack enable

[settings.npm]
default_installer = "npm"   # npm / pnpm；仅在项目自身没有任何声明时作兜底

[settings.python]
catalog_url = "https://.../catalog.json"   # 可选：自定义 python 目录
catalog_sha256 = "..."                     # 与 catalog_url 配套的精确 SHA-256

[settings.java]
catalog_url = "https://.../packages"       # Foojay 兼容端点或静态镜像

[settings.shims]
include = []                # 非空则「只」shim 匹配名（对所有工具的白名单，慎用）
expose = ["make"]           # 在默认判定之外「额外」shim 的名字（加法，安全）
exclude = ["apkanalyzer"]   # 跳过的名字，最后生效、总是胜出
# 每工具覆盖（键是 backend id，精确匹配、无 glob）：
[settings.shims.tools."conda:m2-base"]
expose = ["make"]
```

- **`link_mode` 取值**：`hardlink` / `symlink` / `copy` / `clone`（reflink）/ `auto`
  （具体支持随平台，`osdk doctor` 会报告实际使用的模式）。
- **`shims.include` 是危险开关**：它是对「机器上一切工具」的白名单，不是「补一个命令」。
  想补命令用 `expose`（加法）。

**config 键名**（用于 `osdk config get/set/unset`，与 TOML 路径的对应）：
`jobs`、`offline`、`yes`、`verify_signatures`、`require_checksums`、`attestations`、
`prerelease`、`link_mode`、`lang`、`shims.include`、`shims.expose`、`shims.exclude`。
布尔值接受 `true/1/yes/on` 与 `false/0/no/off`；列表型接受逗号分隔值。

```bash
osdk config set jobs 6
osdk config set shims.exclude "apkanalyzer,lint"
osdk config get attestations -g
```

---

## `[tools]` — 固定工具与版本

键是 backend id（或动态工具全名），值有两种写法：

```toml
[tools]
# 1) 传统字符串：只给版本
node = "20"
python = "3.12"

# 2) 结构化对象：版本 + backend 专属选项 + 平台过滤
rust = { version = "1.98.0", components = "rustfmt,clippy", targets = "x86_64-pc-windows-gnu" }
java = { version = "21", distribution = "zulu", package-type = "jdk" }

# 动态工具（键含命名空间前缀，需引号）
"npm:prettier" = "3"
"go:golang.org/x/tools/gopls" = "0.20.0"
```

结构化对象（`StructuredToolConfig`）：

- `version`（**必填**，字符串）。
- `when`（可选，平台过滤，见文末「平台过滤」）：`when = { os = "windows" }`，
  `when = { os = ["linux","macos"], arch = "arm64" }`。`when` 内用了不支持的维度会**硬报错**。
- 其余键都是**该 backend 的选项**（`-o KEY=VALUE` 的等价物），原样透传给 backend，
  例如 rust 的 `components` / `targets` / `profile`，java 的 `distribution` / `package-type`，
  npm 的 `installer` 等。

> 被平台过滤排除的工具，命令点名它时 osdk 会解释「因 os/arch 被排除」，而不是当成拼错。

---

## `[aliases]` — 版本别名

两层表：工具 → 别名 → 目标版本 spec。可链式展开，禁止环。

```toml
[aliases]
node = { default = "20", lts = "20" }
python = { work = "3.12" }
```
等价命令：`osdk alias set node default 20`。

---

## `[sources]` — 下载源与镜像

顶层字段 + 每工具子表（`per_tool` 以工具名 flatten 进 `[sources]`）。

```toml
[sources]
selection = "auto"          # auto（测速排序）/ 其他选择策略
mode = "auto"               # auto：环境镜像参与测速；env：原样遵循、缺失即报错
probe_timeout_ms = 1500     # 单次测速超时
cache_ttl = "6h"            # 测速结果缓存有效期（人类可读时长）

# 每工具覆盖：键是工具名（或 self / go-modules 这类特殊源名）
[sources.node]
pin = "tuna"                # 优先尝试的源 id（非「只用这一个」，其余仍作回退）
disable = ["some-builtin"]  # 停用的内置源 id
# 自定义源（等价 osdk source add）：
[[sources.node.custom]]
id = "mycorp"
download_url = "https://mirror.corp/node/"
index_url = "https://mirror.corp/node/index.json"   # 可选，与下载地址不同时给
forward_credentials = false                          # 允许该端点收到 provider 凭据

# 模型 provider 的环境导出（配合 osdk model env）：
[sources.huggingface]
env = true                  # 激活时导出该 provider 的 endpoint/cache
env_force = false           # 覆盖用户已设置的同名变量
```

- 特殊源名：`self`（osdk 自身升级源）、`go-modules`（`GOPROXY`，与工具链源 `go` 相互独立）。
- **推荐用命令改**：`osdk source add/pin/unpin/remove`，而不是手写这一段。

---

## `[registries]` — 依赖 Registry 预检（需要 trust）

只影响 osdk 代跑的包管理器命令，与 osdk 自身下载源分开。

```toml
[registries.npm]
urls = ["https://registry.npmmirror.com/"]   # 空 = 用内置公共候选
probe_timeout_ms = 1500

[registries.python]
urls = ["https://pypi.tuna.tsinghua.edu.cn/simple/"]   # 只当作默认 index 的镜像
probe_timeout_ms = 8000
```
> Python 镜像**只会**映射到默认 index，绝不排在私有 index 之上（防依赖混淆）。私有 index
> 交给包管理器自己的配置。

---

## `[containers]` — 原生容器运行时（镜像改写需要 trust）

```toml
[containers]
runtime = "auto"            # auto / docker / containerd
builder = "auto"            # auto 或显式 Buildx builder 名
platform = "runtime"        # runtime 或 OCI 的 OS/ARCH[/VARIANT]，如 linux/amd64
probe_timeout_ms = 1500

# 每 Registry 的镜像策略（键是 registry 主机名）：
[containers.registries."docker.io"]
mirrors = ["https://mirror.example/"]
anonymous_only = true
resolve = "upstream"        # upstream（源解析 tag）/ mirror（镜像解析 tag）
```
> 已信任的显式 policy 会完整覆盖 Docker Hub 的内置候选镜像。

---

## `[syspkg]` — 宿主系统包（需要 trust：ExecutesCode）

声明宿主包管理器要装的包。**声明本身不装任何东西**：`osdk pkg status` 只读，只有
`osdk pkg apply --yes` 才安装。键是 `<manager>:<package>`，version 是「安装时的愿望」而非锁。

```toml
[syspkg]
# 顶层可留空；下面每条按 manager 前缀 + os/arch 过滤

[syspkg.packages]
# 字符串写法与对象写法等价
"apt:build-essential" = { version = "latest", os = "linux" }
"dnf:gcc" = { version = "latest", os = "linux" }
"pacman:gcc" = "latest"
"apk:build-base" = { version = "latest", os = "linux" }
"winget:Git.Git" = { version = "latest", os = "windows" }
```

- 支持的 manager 前缀取决于宿主（如 `apt` / `dnf` / `pacman` / `apk` / `winget` / `brew`）。
  osdk **不做**跨 manager 名称映射，每个包按各自发行版的写法拼。
- `os` / `arch` 各接受单个 token 或列表；无法识别的 token 是**硬错误**（不是「永不匹配」）。

---

## `[tasks]` 与 `[task_config]` — 项目任务（运行时需要 trust）

`[tasks]` 的键是任务名，值有三种：字符串、命令数组、或完整对象（`TaskDef`）。

```toml
[tasks]
# 1) 单行字符串
fmt = "cargo fmt --all"

# 2) 命令数组：依次执行、失败即停
[tasks.ci]
description = "CI 全流程"
run = [
  "cargo fmt --all --check",
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },  # 容错继续
  { tasks = ["test", "doc"] },                                    # 并行子任务并等待
]
depends = ["build"]         # 前置任务，先跑

# 3) 完整对象（部分常用字段）
[tasks.e2e]
run = "pytest tests/e2e"
run_post = "docker compose down"     # run 之后一定跑（含 run 失败时）
run_windows = "..."                  # Windows 专属替换 run
depends = ["build"]                  # 前置（会被拉入并运行）
wait_for = ["migrate"]               # 仅当对方已在计划中才等它
alias = ["integration"]
env = { CI = "1" }
dir = "e2e"                          # 相对配置文件的工作目录
shell = "pwsh -Command"              # 解释器覆盖
hide = false                         # 从列表/补全隐藏
quiet = false
when = { os = "linux" }              # 平台过滤
timeout = "5m"                       # 到期杀整棵进程树
sources = ["src/**/*.rs"]            # 输入 glob；不变则跳过（匹配不到即报错）
outputs = ["target/release/app"]     # 输出 glob
freshness = "mtime"                  # mtime（默认）/ hash / always：如何判定输入是否变化
file = "scripts/build.sh"            # 或改用脚本文件（相对配置文件）
lua = "..."                          # 或嵌入 Lua（scripts feature，默认开）
```

`run` 步骤语义：数组内命令**顺序执行、失败即停**；`{ cmd = "...", ignore_error = true }`
容忍失败继续（打印警告）；`{ tasks = [...] }` 并行执行并等待全部完成。
**不要用 shell 的 `&`**——它在 cmd、PowerShell 7 与 5.1 下含义各异。

`[task_config]`（runner 级默认，整体替换而非字段级合并）：

```toml
[task_config]
shell = "pwsh -Command"     # 全局默认解释器
dir = "."                   # 全局默认工作目录
roots = ["apps/*", "packages/*"]   # monorepo：子项目任务以 //path:name 并入同一任务表
# dirs = [...]              # 覆盖搜索 file task 的目录
```
> 子项目**不能**自己声明 `[task_config]`；runner 默认只由根配置的 `[task_config]` 决定。

---

## `[deps]` — 应用依赖 provider

`[tools]` 装包管理器本身，`[deps.<provider>]` 装项目自己的包。**空表即选中内置 provider**
（`auto` 默认 true，声明 provider 就等于开启自动兑现）。

```toml
[deps]
disable = ["yarn"]                 # 即使上层启用也在此关掉（不卸载，只不跑）
roots = ["apps/*", "packages/*"]   # monorepo：显式声明子项目，绝不盲扫

[deps.pnpm]                        # 键是 provider id：npm/pnpm/yarn/bun/deno/go/cargo/uv/pip-requirements...
# 空表就够用；下面都是可选覆盖
auto = true                        # 裸 install/run/exec 前是否自动兑现（默认 true）
sources = ["package.json", "pnpm-lock.yaml"]  # 新鲜度输入（替换而非追加内置列表）
outputs = ["node_modules/.package-lock.json"] # 跟踪输出（空列表=显式关闭输出跟踪）
run = "pnpm install --frozen-lockfile"        # 完全覆盖默认命令
env = { NODE_ENV = "development" }
dir = "."                          # 相对配置根的工作目录（嵌套项目）
depends = ["proto"]                # 必须先完成的其他已配置 provider
timeout = "5m"
installer = "pnpm"                 # 显式钉安装器，覆盖自动选择

# 下面两个键的存在会让本 provider 需要 trust（WeakensVerification）：
index = "https://mirror/simple/"        # 改写来源
extra_index = "https://extra/simple/"   # 附加 index，绝不排在默认之上

# 下面这个是 ExecutesCode：默认拒绝，必须显式打开并被信任
allow_build_from_source = true     # 允许从源码构建 / 跑生命周期脚本
```

- `deny_unknown_fields`：拼错字段（如把 `allow_build_from_source` 写成 `allow_builds`）会
  **硬报错**，不会被静默忽略。
- 逃生口：单次 `osdk deps --no-deps` 之类由命令侧提供；永久关闭某 provider 用 `auto = false`。

---

## `[models]` — 声明式模型

`osdk model pull <name>` 会读取声明、写进 lock 并立即渲染视图。

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"   # 必填
include = ["*.safetensors", "*.json"]              # pull 时的 glob 白名单
exclude = []
variant = "fp16"                                   # 可选格式/量化标签
when = { os = "linux" }                            # 平台过滤
endpoint = "https://..."                           # 可选：改写来源（这一项需要 trust）

# 消费者视图（重复子表）：仓库前缀 -> 消费者分类
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"
```
- `deny_unknown_fields`：`source`/`endpoint` 拼错会硬报错。
- 只声明「要什么」不需要 trust；只有 `endpoint` / 自定义来源这类**改变字节来源**的键才需要，
  且模型声明不会阻断普通工具命令。

---

## 平台过滤（`when` / `os` / `arch` 通用词汇）

多处（`[tools]` 的 `when`、`[tasks]` 的 `when`、`[syspkg]` 的 `os`/`arch`）共用一套词汇：

```toml
when = { os = "windows" }
when = { os = ["linux", "macos"] }
when = { arch = "arm64" }
when = { os = "linux", arch = ["x86_64", "arm64"] }
```
- `os` 常见 token：`windows` / `macos`（`darwin`）/ `linux`。
- `arch` 常见 token：`x86_64`（`amd64`）/ `arm64`（`aarch64`）。
- 无法识别的维度或 token 会**硬报错**，而不是变成一个永不匹配的过滤器。

---

## 环境变量对照（部分）

配置字段大多有对应的 `OSDK_*` 环境变量或全局参数，优先级高于文件：

| 环境变量 | 对应 |
| --- | --- |
| `OSDK_JOBS` | `settings.jobs` / `--jobs` |
| `OSDK_OFFLINE` | `settings.offline` / `--offline` |
| `OSDK_REQUIRE_CHECKSUMS` | `settings.require_checksums` / `--require-checksums` |
| `OSDK_ATTESTATIONS` | `settings.attestations` / `--attestations` |
| `OSDK_PRERELEASE` | `settings.prerelease` / `--prerelease` |
| `OSDK_SOURCE_MODE` | `sources.mode` / `--source-mode` |
| `OSDK_LANG` | `settings.lang`（会话级） |
| `OSDK_CONFIG_DIR` / `OSDK_DATA_DIR` / `OSDK_CACHE_DIR` / `OSDK_STORE_DIR` / `OSDK_INSTALL_DIR` | 目录覆盖 |

## 最小可用示例（一个典型项目 `osdk.toml`）

```toml
[tools]
node = "20"
python = "3.12"
rust = { version = "1.98.0", components = "rustfmt,clippy" }

[deps.pnpm]

[tasks]
build = "cargo build --release"
[tasks.ci]
run = [{ tasks = ["fmt-check", "test"] }]
[tasks.fmt-check]
run = "cargo fmt --all --check"
[tasks.test]
run = "cargo test --workspace"
```
