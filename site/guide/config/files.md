# 配置文件

osdk 将用户默认值、项目声明、环境变量和 CLI 覆盖组合成当前配置。本页说明配置文件的位置与 `osdk config` 命令、可编辑字段的完整结构，以及多层配置之间精确的覆盖粒度。环境变量见[环境变量](./environment)，项目配置的审批见[项目配置信任](./trust)。

## 配置位置

- 用户配置：`$OSDK_CONFIG_DIR/config.toml`；Linux 默认是 `~/.config/osdk/config.toml`。
- 项目配置：`osdk.toml` 或 `.osdk.toml`。从当前目录向上查找最近一份；同一目录
  同时存在时，`osdk.toml` 优先。
- `.tool-versions`：独立向上查找，其声明参与共用的[工具版本优先级](#可配置的声明优先级)；默认顺序中优先于用户全局工具默认值。

```text
osdk config path
osdk config list
osdk config get KEY [-g]
osdk config set KEY VALUE [-g]
osdk config unset KEY [-g]
osdk config migrate [--dry-run] [-g]
```

`config path` 显示配置目录、用户配置文件和当前发现的项目配置。`config list` 显示
部分解析结果而非原始 TOML；它不会列出 `yes`、`lang`、所有 source 细节等全部字段。

`get` / `set` / `unset` 默认作用于**项目配置**，`-g` 才是用户配置——与 `git config`、
`npm config` 一致。`get` 默认返回合并后的生效值（含默认值与环境变量覆盖），`-g` 只读
用户那一层。

```bash
osdk config set jobs 8                       # 写入 ./osdk.toml
osdk config set -g jobs 8                     # 写入用户配置
osdk config get jobs                          # 生效值
osdk config unset jobs                        # 恢复默认
```

可写的是下列标量与列表设置：`jobs`、`offline`、`yes`、`verify_signatures`、
`require_checksums`、`attestations`、`prerelease`、`link_mode`、`lang`、
`sources.probe_timeout_ms`、`shims.include`、`shims.exclude`、`shims.expose`，以及按
工具限定的 `shims.<tool>.{include,exclude,expose}`。列表用逗号分隔。工具固定、source
固定和别名不在其中，它们分别由 `osdk use`、`osdk source pin` 和 `osdk alias` 管理。

`sources.probe_timeout_ms` 写进顶层 `[sources]` 表，单位毫秒、必须 ≥ 1，默认 1500。
它是**每个源一次探测**的总预算：模型源探测要在这一份预算内同时发 metadata 请求和一个
有界的 Range 拉取，所以慢网络下 1500 ms 可能把完全可用的本地镜像误判为不可达。此时调
大它，而不是去手改配置文件：

```bash
osdk config set -g sources.probe_timeout_ms 4000   # 只放宽源探测，不影响下载本身
osdk config get sources.probe_timeout_ms
```

Python 与 npm 各自的探测超时是 `[registries.python]` / `[registries.npm]` 下独立的
`probe_timeout_ms`，当前不在 `config set` 清单里（那两组列表已在清单中）。

枚举取值与各自类型一致，别名会被规范化后写入（`attestations=auto` 存为
`if-available`）：

| 设置 | 取值 |
| --- | --- |
| `attestations` | `off`、`if-available`、`required` |
| `prerelease` | `never`、`if-explicit`、`allow` |
| `link_mode` | `auto`、`hardlink`、`reflink`、`copy`、`symlink` |
| `lang` | `en`、`zh` |

取值会先解析校验再落盘，非法值不会留下改了一半的文件；`unset` 会顺带清掉被清空的表头，
不会留下一个空的 `[settings]`。

::: warning 写入受管设置会触发信任
`config set` 写入的键若属于[受管键](./trust#哪些键需要信任)（例如 `verify_signatures`），会使该
文件需要信任，命令会就地询问，`--yes` 时自动确认；非交互环境下写入照常成功，但不授予
信任，并提示还需要做什么。写入 `jobs`、`lang` 这类安全键不会触发询问。
:::

### 配置段命名与旧格式迁移

复数顶层段表示具名条目的集合：`[tools]`、`[tasks]`、`[models]`、`[skills]`、
`[sources]`、`[registries]`。单数顶层段表示一个子系统的设置或命名空间：`[task]`
保存共享 runner 设置，`[container]` 保存容器设置，`[alias.tools]` 是别名域下的工具版本
别名，`[sys.pkg]` 是系统域下的包管理器设置。于是 `osdk task` 操作 `[tasks]` 中的一个
任务，而不是要求集合表也改成单数。

旧版四组写法暂时仍可读取，并可一次迁移：

```bash
osdk config migrate --dry-run   # 预览当前项目
osdk config migrate             # 改写当前项目
osdk config migrate --global    # 改写用户 config.toml
```

迁移关系是 `[aliases]` → `[alias.tools]`、`[containers]` → `[container]`、
`[task_config]` → `[task]`、`[syspkg]` → `[sys.pkg]`，以及把临时的
`[sys.pkg.packages]` 条目扁平到 `[sys.pkg]`。命令保留注释及其他段；如果一组
新旧写法同时存在，它会拒绝猜测合并顺序并保持文件不变。osdk 的写命令只生成新布局。

## 最小项目配置

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
rust = "1.91.1"
pnpm = "10"
"npm:prettier" = "3"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
"go:golang.org/x/tools/gopls" = { version = "0.20", tags = ["netgo"] }
"cargo:cargo-release" = { version = "0.25", lazy = true }

[alias.tools.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` 会修改最近的项目配置；没有项目配置时在当前目录创建
`osdk.toml`，并把实际安装结果及注入的 runtime 依赖写入同目录 `osdk.lock`。两份文件作为
一个元数据事务更新，lock 写入失败会恢复配置。当前平台尚无 lock 区段时，无参数
`install`/`lock` 会继承其他平台一致且仍满足声明的版本，但重新解析当前平台的 artifact 与
checksum。`osdk use --global node@20` 修改用户配置，
不触碰当前项目 lock。
对于 `npm:<package>`，本地 `use` 会先查找最近的 `package.json`；下一节说明其项目感知
行为。
每个 `cargo:<crate-or-https-url>` 条目都必须对应一个精确、显式请求或配置的 `rust`
条目；浮动和本地链接的 Rust toolchain 会被拒绝。详见 [Cargo 开发工具](../tools/cargo)。
每个 `go:<module-or-command-path>` 条目也必须对应一个受管 `go` 选择。osdk 会先解析该
runtime，再把精确版本绑定到工具；详见 [Go 开发工具](../tools/go)。

## 完整配置参考

以下示例覆盖当前可编辑 schema。`[tools]` 的值既可以是版本字符串，也可以是带
backend 选项的结构化对象；包含 `:`、`@` 或 `/` 的 key 要用引号。省略的字段使用
该文件层反序列化时的默认值；下一节解释这为何不等于继承低优先级层。

```toml
[settings]
link_mode = "auto"          # auto|hardlink|reflink|copy|symlink
jobs = 8                    # 默认 min(可用并行数, 8)，无法检测时 4
yes = false
verify_signatures = true
require_checksums = false
attestations = "off"        # off|if-available|required
offline = false
lang = "zh"                 # 可省略；en|zh
prerelease = "if-explicit"  # never|if-explicit|allow

[settings.precedence]
tools = "osdk"            # osdk|native；工具版本的默认顺序
packages = "native"       # osdk|native；默认包源策略
tool_order = []            # 所有工具共用的来源类型顺序；[] 使用默认顺序
tool_conflicts = "prefer"  # prefer|error
npm = "inherit"           # osdk|native|inherit；只覆盖包源策略
python = "inherit"
go = "inherit"

[settings.shims]
include = []                # 全局白名单；非空时未列出者一律不生成 shim
exclude = []                # 排除；最后生效
expose = []                 # 增量放行；只加不减

# 按工具覆盖，key 是 backend id。未写的字段继承上面的全局列表。
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "tr"]

[settings.node]
corepack = false

[settings.python]
catalog_url = "/approved/python-catalog.json" # HTTP(S) 或本地路径，可省略
catalog_sha256 = "0123456789abcdef..."         # 使用 catalog_url 时必填

[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages" # 可省略

[sources]
selection = "auto"          # auto|pinned|ordered
probe_timeout_ms = 1500
cache_ttl = "6h"            # s/sec/secs, m/min/mins, h/hr/hrs, d/day/days

[sources.node]
pin = "official"            # 可省略
disable = ["tuna"]          # 可省略
env = false                 # 仅模型 provider 的全局 shell adapter 使用
env_force = false

[[sources.node.custom]]
id = "corp"
kind = "custom"             # official|mirror|custom
download_url = "https://mirror.example/sdk/"
index_url = "https://mirror.example/sdk/index.json" # 可省略
headers = [["Header-Name", "value"]]              # 可省略
forward_credentials = false
priority = 0
enabled = true

[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500

[container]
runtime = "auto"              # auto|docker|containerd
builder = "auto"              # auto 或经过验证的 Buildx 构建器名称
platform = "runtime"          # runtime 或 OS/ARCH[/VARIANT]
probe_timeout_ms = 1500

[tools]
node = "20"
python = "3.12"
pnpm = "10"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
lazy = true                    # 裸 install 默认跳过，除非带 --include-lazy
installer = "npm"             # auto|npm|pnpm；隐式默认值为 auto
allow_builds = ["@scope/native-tool", "esbuild"]

[tools."npm:only-on-windows-arm"]
version = "1.0.0"
when = { os = "windows", arch = "arm64" }   # 单值或数组；两个维度都要命中

[tools.node]
version = "20"
arch = "arm64"                # backend 选项：下载哪个产物（语义不变）
when = { os = "windows" }     # 过滤：这条在哪生效；两者可共存

[tools."http:https://downloads.example.com/acme-{version}.tar.gz"]
version = "1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
kind = "tar.gz"
strip-components = "1"
bin = "bin/acme"
rename = "acme"

[alias.tools.node]
default = "20"
```

Registry URL 会去重并补尾部 `/`。只允许带 host 的 HTTP(S) URL；credentials、
query 和 fragment 都会被拒绝。来源的选择语义见[下载源与供应链安全](../tools/github-releases)，
Registry 的选择语义见[JavaScript 包管理器](../tools/package-managers)。
结构化工具对象要求 `version`，其他 option 可以是字符串、布尔值或字符串数组；数组
传给 backend 时会转成逗号分隔值。`lazy` 是 osdk 元数据而非 backend option：裸
`install` 会跳过它，除非带 `--include-lazy`；显式点名仍会安装。`installer` 选择 npm 工具安装器。`allow_builds` 控制
隔离与全局安装；项目感知的 `use` 始终禁用 lifecycle scripts。完整安全边界见
[npm 开发工具](../tools/npm#构建脚本策略)。
`http:` 条目要求精确语义化版本，并为严格 HTTPS `{version}` 模板提供 SHA-256；
文件/归档布局与离线重放见[直接 HTTPS 制品](../tools/http-artifacts)。

`when` 是**平台过滤**，不是 backend option：它在进入 backend 之前就被摘除，不匹配的
条目在合并后的配置里**根本不存在**，因此求解、lock、shim、激活都不会看到它。维度内是
「任一命中」，两个维度之间是「都要命中」。

::: warning 为什么嵌套在 `when` 下，而不是直接写 `os` / `arch`
`os`、`arch`、`libc` 在 `github:` 和 `node` 上**已经是 backend 选项**，语义是「下载哪个
平台的产物」——跨架构锁定正依赖它。早先一版把平铺的 `arch` 读作过滤，结果
`[tools.node] arch = "arm64"` 在 x64 主机上让 node 整条消失，且没有任何提示。嵌套把
两套词汇分开，也给将来的 `libc` 留了位置。
:::

`when` 内只接受已实现的维度，写入未实现的（如 `libc`）会明确报错，而不是被忽略后把
过滤悄悄放宽成「任意 libc」。无法识别的取值同样直接报错，而不是默默永不匹配。显式
点名一个被过滤的工具会报错并说明限制，`osdk current` 也会把它连同原因一起列出——
一个写在配置里却不出现的工具，否则看起来像配置写错了。

字符串简写形式（`fd = "npm:fd@10"`）不支持过滤，需要时改写成上面的表形式。

## 精确的覆盖与合并语义

普通配置字段的总体优先级为（工具版本选择使用下文单独的策略）：

```text
CLI > OSDK_* 环境变量 > 最近项目配置 > 用户配置 > 内置默认值
```

文件之间不是统一的“逐字段覆盖”：

| 配置区域 | 高优先级文件的行为 |
| --- | --- |
| `[settings]` | 除 `precedence` 外**整段替换**；其他遗漏设置回到 `Settings` 内置默认值，而不是继承用户文件 |
| `[settings.precedence]` | 用户、显式 `extends`、项目层按字段合并；遗漏字段继承，数组替换；`tool_order = []` 清除自定义顺序，生态字段 `"inherit"` 清除对应包源策略覆盖 |
| `[sources]` 顶层 | `selection`、`probe_timeout_ms`、`cache_ttl` **整段替换**；遗漏项回到默认值 |
| `[sources.<tool>]` | 按工具键合并；同一工具的 `pin`、`disable`、`custom` 等整项由高优先级层替换 |
| 模型 `env`/`env_force` | 项目配置不能改变；始终保留用户全局值，避免项目静默改写 shell 凭据环境 |
| `[registries]` | 整段替换；项目 `[registries.npm]` 不与用户 URL 列表合并 |
| `[container]` | 整段替换；省略的运行时、构建器、平台、超时和 Registry 策略字段使用内置默认值 |
| `[tools]` | 按工具键合并；高优先级同名键覆盖 |
| `[alias.tools.<tool>]` | 按工具和别名键合并；高优先级同名别名覆盖 |
| 原生工具声明（含 `.tool-versions`） | 按 `settings.precedence` 与项目/全局 `[tools]` 一起排序；来源类型顺序见下文 |

例如，用户配置启用了 `verify_signatures = false`，而项目仅写：

```toml
[settings]
jobs = 2
```

项目层会创建一整套默认 `Settings` 再设 `jobs = 2`，因此最终
`verify_signatures` 回到默认 `true`。若希望保留非默认组合，应在高优先级的
`[settings]` 中完整声明。

`[settings.download_cache]` 使用 `max_size_bytes`（默认 2147483648；0 不保留；`"unlimited"` 不限）与 `max_age_secs`（默认 604800；0 不过期）。`OSDK_DOWNLOAD_CACHE_MAX_SIZE` 覆盖容量。详见[缓存与清理](../maintenance/cache)。

显式继承：子项目 `osdk.toml` 可设置 `extends = "../../osdk.toml"`。目录嵌套不会自动继承。优先级为 CLI/环境变量、子文件字段、显式继承的祖先字段、用户配置、默认值。映射按键合并（包括 env），标量与列表替换，`false`、`[]` 都是有效覆盖。继承的依赖 sources/outputs/dir 与任务 dir/file 路径属于声明它的文件。发现 roots/projects 留在根目录，已发现的子配置不能重定义整个目录。继承子项目以根项目声明为默认，显式 provider 字段优先；独立子配置自行声明 provider。根/子目录调用依赖使用同一目录与有效 provider/工具选项。继承链中每个可执行或改变来源的配置都单独检查信任，信任子文件不代表信任父文件。自身循环、非祖先引用及越过仓库边界均报错。`[task].roots` 仍单独负责聚合；继承的 runner 默认值转换到具体子任务。

## 可配置的声明优先级

```toml
[settings.precedence]
tools = "osdk"
packages = "native"
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "error" # 默认 prefer，按顺序选取
npm = "osdk"           # 可选：覆盖 packages；inherit 清除该生态覆盖
python = "native"
```

`tool_order` 是所有工具共用的一份来源类型顺序，从高到低排列，不是按工具名分别配置的表。仅适用于当前工具的来源参与选择；每个文件名取最近声明，`idiomatic` 还保留下述 backend 固定文件顺序。支持 `system`（需主动启用）、`osdk`、`global`、
`.tool-versions`、`.nvmrc`、`.node-version`、`idiomatic`、
`package.json#packageManager`、`package.json#devEngines.packageManager`、
`package.json#engines.node`、`package.json#devEngines.runtime`。遗漏的声明来源按默认顺序补齐；`system` 是例外，仅在数组中明确列出时启用。
未知值和重复值报错；`[]` 清除继承的自定义顺序并禁用继承的 `system`。`idiomatic` 将其他原生文件归为一类：Python 使用 `.python-version`；Go 先 `go.mod` 后 `.go-version`，Rust 先 `rust-toolchain.toml` 后 `rust-toolchain`，Java 先 `.java-version` 后 `.sdkmanrc`。这些文件名不能独立写入 `tool_order`；不同文件名之间先按 backend 顺序，再考虑目录距离。默认顺序是项目 osdk、
.tool-versions、.nvmrc、.node-version、其他 idiomatic、packageManager、
devEngines.packageManager、engines.node、devEngines.runtime、global；`tools="native"`
把项目 osdk 移到原生项之后。自定义顺序可以明确提升 global。

项目 osdk 只使用最近配置及显式 `extends`，不隐式合并父配置。precedence 的用户、
extends、项目层按字段覆盖，数组替换；未写的策略字段继承。工具声明同类中被覆盖的
父层仅作为 shadowed 证据，不参与冲突检查，也不校验已失效的版本值。其他工具的声明以 ignored 记录，不解释其版本或完整性值。原生管理器字段跨管理器名称选择最近身份；自动选择遇到不支持的身份会报错。`osdk current` 输出同一选择结果中的
tool、path、field、category、winner/shadowed/ignored 和 reason；结构化配置字段、JSON 字段和简单
文件行号会在已知时报告，不能确定的路径保留为空，不猜测。

`tool_conflicts="error"` 检查不同来源的有效声明：精确版本满足范围不算冲突，数字前缀、
支持的 semver 比较符及 `||` 按范围检查两两交集；配置 alias 先展开。它不把多个范围
合并成新的版本请求，最终仍选择最高优先级声明。不同的 latest/LTS/channel 或不透明选择器
无法离线证明兼容时单独报“无法验证”，不伪称已确认冲突，也不联网安装来解释。
`prefer` 选择最高项，但仍校验有效候选声明的结构。packageManager 要求精确 semver，不支持 URL、hash 或 build suffix；
devEngines.packageManager 的多选数组明确拒绝，单对象/单元素数组支持范围。
显式 `tool@version`、指定的 deps provider/installer 及锁重放保持各自权威。

`[settings.precedence]` 下的 `npm`、`python`、`go` 只覆盖对应生态的**包源策略**，不控制工具版本顺序。可取 `osdk`、`native`、`inherit`（恢复使用 `packages`）；例如 `python="native"` 保留 Python 包索引策略，不会提升 `.python-version`。这些字段不重排管理器内部的认证、scope 或 TLS 来源。uv 遵循同目录 uv.toml 优先、最近项目配置及用户/首个系统配置的候选发现；
复杂配置（命名索引、绑定及其他未建模字段）在 native 下整组保留，osdk 显式索引
实际注入并与其组合时报错；若调用环境已经覆盖对应字段而无需注入，不触发此错误。发现可能的 uv workspace 时整组委托 uv，包含成员资格与排除规则，
不注入 osdk 索引；uv 仍可能校验成员元数据。候选文件不等于逐字段 winner。含原生选项或无法读取的 requirements/constraints、`uv pip sync`/`compile` 的位置输入，以及显式配置/目录（含 `-C`）/offline/dotenv 调用参数（包括子命令之前的参数）委托 uv；`UV_NO_CONFIG/UV_OFFLINE/UV_NO_INDEX`
的 false-like 值不会单凭存在就禁用注入。不会改写原生文件或在执行失败后重装。

### tool_order 合法值与工具适用范围 {#tool-order-reference}

`tool_order` 是由以下 **11 个精确字符串**组成的数组，排列的是声明来源及需主动启用的外部执行来源，不是工具名、命令名、下载镜像或 registry。同一数组适用于所有工具；合法类型若没有当前工具的候选声明，就不会影响该工具。没有 `[settings.precedence.node]` 或按工具独立设置的 `tool_order`。

| 合法值 | 候选来源 | 适用的内置工具 |
| --- | --- | --- |
| `system` | 适配器 home 或 PATH 中已有的可执行文件；仅明确列出时启用 | Node、Python、Go、Java、npm、pnpm、Yarn、Bun、Deno、Zig；见[系统工具](#system-tools) |
| `osdk` | 最近项目 `osdk.toml` / `.osdk.toml` 中的 `[tools]`，包括显式 `extends` | 所有具有匹配 backend 键的工具 |
| `global` | osdk 用户配置 `$OSDK_CONFIG_DIR/config.toml` 中的 `[tools]`；用 `osdk config path` 查看实际位置 | 所有具有匹配 backend 键的工具 |
| `.tool-versions` | 最近适用 `.tool-versions` 中匹配该工具的行 | 所有具有匹配 backend 键的工具 |
| `.nvmrc` | `.nvmrc` 版本声明 | Node |
| `.node-version` | `.node-version` 版本声明 | Node |
| `idiomatic` | backend 声明的文件，使用下表固定顺序 | 下表对应的 backend；不包含 Node 单独分类的两个文件 |
| `package.json#packageManager` | `packageManager` 中的管理器精确版本 | npm、pnpm、Yarn |
| `package.json#devEngines.packageManager` | `devEngines.packageManager` 中的版本/范围 | npm、pnpm、Yarn |
| `package.json#engines.node` | `engines.node` 中的 Node 版本/范围 | Node |
| `package.json#devEngines.runtime` | `name="node"` 条目中的运行时版本/范围 | Node |

`global` 指 **osdk 用户级的工具默认配置**，不是 `PATH` 中已安装的版本，也不是 nvm/npm 的原生全局配置。例如，用户 `config.toml` 的 `[tools]` 声明 `node="20"`，项目 `osdk.toml` 的 `[tools]` 声明 `node="22"`，项目使用以下策略时选中请求 `20`：

```toml
[settings.precedence]
tool_order = ["global", "osdk"]
tool_conflicts = "prefer"
```

把 `tool_conflicts` 改为 `"error"` 会拒绝不兼容的 `20` 与 `22` 声明，不再选取其中一个。不配置自定义顺序时，默认项目优先，选中 `22`。显式 CLI 版本与锁重放仍保持权威。

没有 `env` 来源类型，也没有通用 `OSDK_<TOOL>_VERSION` 覆盖：`OSDK_NODE_VERSION`、`OSDK_PYTHON_VERSION` 均未实现。`CLI > OSDK_* > 项目` 只适用于已实现的设置。机器上已有的 PATH/home 工具通过单独、需主动启用的 [`system` 执行来源](#system-tools) 选择；未启用时，shim 未选中版本仍会报错。

下表每一行还支持 `system`（需主动启用）、`osdk`、`global`、`.tool-versions`，按该工具的 backend 键匹配。存在版本文件不等于自动请求安装该工具；本表说明请求或激活该工具时如何选择版本。

| 工具 / backend 键 | 额外适用的 `tool_order` 值 | `idiomatic` 内的文件，按优先级排列 |
| --- | --- | --- |
| `node` | `system`, `.nvmrc`、`.node-version`、`package.json#engines.node`、`package.json#devEngines.runtime` | 无：`.nvmrc` 和 `.node-version` 各有独立类型；移动 `idiomatic` 不会移动它们 |
| `python` | `system`, `idiomatic` | `.python-version` |
| `go` | `system`, `idiomatic` | `go.mod`（`go` 指令），然后 `.go-version` |
| `rust` | `idiomatic` | `rust-toolchain.toml`，然后 `rust-toolchain` |
| `java` | `system`, `idiomatic` | `.java-version`，然后 `.sdkmanrc` |
| `bun` | `system`, `idiomatic` | `.bun-version` |
| `deno` | `system`, `idiomatic` | `.dvmrc` |
| `zig` | `system`, `idiomatic` | `.zig-version` |
| `maven` | `idiomatic` | `.mvn-version` |
| `gradle` | `idiomatic` | `.gradle-version` |
| `kotlin` | `idiomatic` | `.kotlin-version` |
| `npm`、`pnpm`、`yarn` | `package.json#packageManager`、`package.json#devEngines.packageManager` | 无 |
| 其他内置及动态 backend | 无 | 无 |
| 声明式插件 backend | 定义声明了文件时支持 `idiomatic` | 其他文件按定义的 `idiomatic_files` 顺序；保留名 `.nvmrc` / `.node-version` 仍使用独立类型；不增加枚举值 |

普通版本文件接受一行版本值（可含注释），不表示支持任意原生管理器配置语法。特别是已登记的 `.sdkmanrc` 路径目前使用普通版本读取器，不解释 SDKMAN 的多键 `java=...` 格式。Rust toolchain 文件和 Go 的 `go` 指令使用专门解析。

未知字符串和重复项会报错：`"node"`、`".python-version"`、`"go.mod"` 都不是合法的 `tool_order` 值。遗漏的声明类型会按 `tools` 决定的默认相对顺序追加，不会被禁用。`system` 不会自动追加，遗漏即禁用。`[]` 清除继承的自定义顺序并禁用继承的 `system`。`idiomatic` 内先按固定文件名顺序，再看目录距离，因此祖先目录的 `go.mod` 声明可以优先于更近的 `.go-version`；`tool_order` 不能反转这两个文件。下方提供[三个可运行示例](#示例-node、python-与-javascript-管理器)。

### 示例：Node、Python 与 JavaScript 管理器

以下是分别使用的项目 `osdk.toml` 示例。这些策略字段直接编辑 TOML，再用 `osdk current node`、`osdk current python` 或 `osdk current pnpm` 查看 winner 与 shadowed/ignored 证据。已有锁重放或显式版本请求仍保持权威。

**Node：让 `.node-version` 优先于 `.nvmrc` 和 `[tools]`：**

```toml
[tools]
node = "20"

[settings.precedence]
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "prefer"
```

若 `.node-version` 内容为 `22`、`.nvmrc` 为 `20`，选中的请求是 `22`。把 `tool_conflicts` 改为 `"error"` 会拒绝这些不兼容声明。该排序也适用于其他工具，但两个 Node 专用文件类型不会影响 Python 或 pnpm。

**Python：优先使用 `.python-version`，同时保留原生包索引：**

```toml
[tools]
python = "3.12"

[settings.precedence]
tool_order = ["idiomatic", "osdk"]
packages = "osdk"
python = "native"
```

若 `.python-version` 内容为 `3.13`，Python 选中的请求为 `3.13`。`idiomatic` 也会提升 Go/Rust/Java 适用的原生文件；没有只针对 Python 的 `tool_order` 配置。单独的 `python="native"` 让 Python 包索引保留原生策略，而其他生态使用 `packages="osdk"`。子层写 `python="inherit"` 可恢复共用包源策略；写 `tool_order=[]` 可清除继承的自定义版本顺序。

**JavaScript 管理器：让 devEngines 范围优先于 packageManager 精确版本：**

```toml
[settings.precedence]
tool_order = ["package.json#devEngines.packageManager", "package.json#packageManager", "osdk"]
tool_conflicts = "error"
npm = "native"
```

配合[包管理器页的 package.json 示例](../tools/package-managers#包管理器版本发现)，pnpm 选择 `>=10 <11`；精确声明 `10.15.0` 与其兼容，但不会将胜出范围固定到该版本。`npm="native"` 控制 npm 生态（含 pnpm/Yarn）的包 registry，不控制管理器可执行文件版本。需要精确选择时，让 `packageManager` 排在前面，或显式指定版本，例如 `osdk exec -t pnpm@10.15.0 -- pnpm --version`。

### 机器上已有的系统工具 {#system-tools}

要使用机器上已有的可执行文件，而不将它安装进 osdk：

```toml
[settings.precedence]
tool_order = ["system", "osdk"]
tool_conflicts = "prefer"
```

这是一份对所有已支持工具生效的策略。`osdk current node` 显示绝对执行路径及来源；`osdk exec -t node -- node --version` 验证并执行它。`osdk reshim` 为可用的伴随命令生成 shim，activation 使用同一选择。`prefer` 下发现/current/activation 不运行版本探针，执行前版本诚实标记为未知。`error` 下选择时探测外部版本，与有效声明（含范围）检查兼容性；`lts` 等不透明选择器无法证明兼容，会报错。JS 管理器探针使用选中的 Node；严格检查若选中托管 Node，该 Node 必须已经安装。

`system` **默认关闭**，遗漏时不会自动追加；用 `[]` 替换继承顺序会禁用它。其他遗漏的声明类型仍按默认顺序追加。例如 `["osdk", "system"]` 优先项目声明，其次系统，再处理遗漏的原生/global 来源。要让系统成为最后回退项，先列出全部声明来源：

```toml
[settings.precedence]
tool_order = [
  "osdk", ".tool-versions", ".nvmrc", ".node-version", "idiomatic",
  "package.json#packageManager", "package.json#devEngines.packageManager",
  "package.json#engines.node", "package.json#devEngines.runtime", "global", "system",
]
```

| 工具 | PATH 之前搜索 | 可执行文件 / 验证 |
| --- | --- | --- |
| Node | 无 | `node`；`vMAJOR.MINOR.PATCH` |
| Python | `VIRTUAL_ENV/bin`（Windows 为 `Scripts`） | 先 `python3` 后 `python`；稳定版 `Python MAJOR.MINOR.PATCH`；显式 venv 须与 `sys.prefix` 一致 |
| Go | `GOROOT/bin` | `go`；稳定版 `go version goMAJOR.MINOR[.PATCH]` |
| Java | `JAVA_HOME/bin` | `java`；数字 Java 版本，必须是带 `javac` 的 JDK；伴随命令来自同一 JDK，不走另一条 alternatives 链接 |
| npm、pnpm、Yarn | 无 | 同名命令，semver 输出；Node 运行时由同一策略选择 |
| Bun、Deno、Zig | 无 | 同名命令，稳定 semver 输出（识别 Deno 原生 banner） |

只有这些适配器参与。Rust/rustup、Maven、Gradle、Kotlin、uv、动态包工具和插件 backend 没有 system 适配器。`PNPM_HOME`、`NVM_DIR`、缓存变量及其他 home 不会成为选择器。现有的无关环境/缓存变量策略不变；`override_env` 不是选择开关。 外部执行前，osdk 也将 activation 注入的强制位置变量（如 `PNPM_HOME`、`npm_config_prefix`、`BUN_INSTALL`）恢复为保存的用户原值或原本未设置状态；这不会让它们变成选择来源。Python venv 或 JDK 优先时，设置普通 `VIRTUAL_ENV` 或 `JAVA_HOME`，再用 `tool_order=["system"]`，无需新增 osdk 版本变量。要混用托管 pnpm 与外部 Node，可用 `osdk exec -t pnpm@10.15.0 -- pnpm --version`：显式管理器保持托管，隐式 Node 按策略选择。

PATH 搜索接受绝对目录（允许空格），忽略空项/相对目录；排除 osdk data/install/store、canonical 别名、已知 shim 文件身份/副本及生成的包装脚本。home 必须是绝对路径；显式设置的无效 home 会报错，不悄悄改选 PATH。osdk activation 注入的 locator/PATH 在发现前恢复成保存的原始环境值。Windows 仅支持 `.exe`，名称不区分大小写；不选择 `.cmd`、`.bat`、PowerShell 或任意 PATHEXT 包装器，因此常见 `pnpm.cmd` / `npm.cmd` 安装应改用托管管理器或真实外部可执行文件。用于构造该执行环境的路径/home 必须是 Unicode。

没有合格候选时，较低优先级的声明可胜出。一旦选中外部候选，版本探针失败、输出无法识别或超长都会停止；探针限三秒、每个输出流 8 KiB。探针失败不会自动安装替代品，执行开始后不会重试安装。不承诺支持未识别的预发布 banner、Java 8 下划线版本或自定义启动器。

显式 CLI 版本及托管锁重放保持权威。`install`/`lock` 对选中 system 的无版本请求报错；要创建可复现安装，请给出显式托管版本。外部路径不写成托管 receipt 或可移植锁条目，uninstall/cache 清理不拥有这些文件。非 frozen deps 可使用未固定版本的外部安装器/运行时；已固定安装器和 frozen 计划仍走托管。使用外部工具的 deps 计划不写可移植工具/新鲜度记录；启用 system 策略时 deps 每次重新验证。`deps --dry-run --explain` 显示不执行探针的发现证据，并注明延后验证。system 执行不与动态包工具 receipt 混用，后者仍保留记录的托管运行时契约。
