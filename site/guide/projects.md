# 项目与配置

osdk 将用户默认值、项目声明、环境变量和 CLI 覆盖组合成当前配置。本页给出
项目发现规则、完整可编辑结构、精确合并粒度和信任边界。

## 配置位置

- 用户配置：`$OSDK_CONFIG_DIR/config.toml`；Linux 默认是 `~/.config/osdk/config.toml`。
- 项目配置：`osdk.toml` 或 `.osdk.toml`。从当前目录向上查找最近一份；同一目录
  同时存在时，`osdk.toml` 优先。
- `.tool-versions`：独立向上查找最近一份，只补充项目/用户 `[tools]` 尚未声明的键。

```text
osdk config path
osdk config list
osdk config get KEY [-g]
osdk config set KEY VALUE [-g]
osdk config unset KEY [-g]
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
`shims.include`、`shims.exclude`、`shims.expose`，以及按工具限定的
`shims.<tool>.{include,exclude,expose}`。列表用逗号分隔。工具固定、source 固定和别名
不在其中，它们分别由 `osdk use`、`osdk source pin` 和 `osdk alias` 管理。

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

::: warning 写入项目配置会触发信任
项目配置里出现 `[settings]` 会使该文件需要信任（见下节）。`config set` 会就地询问是否
信任，`--yes` 时自动确认；非交互环境下写入照常成功，但不授予信任，并提示还需要做什么。
:::

## 项目版本发现

活动版本的来源类型优先级如下；每一类内部再从当前目录向祖先查找：

1. `osdk.toml` / `.osdk.toml` 的 `[tools]`；
2. `.tool-versions`；
3. backend 的生态版本文件；
4. Node 的 `package.json` 元数据；
5. 用户配置的 `[tools]`。

这意味着较远祖先的 `osdk.toml` 也会胜过较近目录的 `.nvmrc`。

| backend | 生态版本文件 |
| --- | --- |
| Node.js | `.nvmrc`、`.node-version`，之后 `package.json#engines.node`、`devEngines.runtime` |
| Python | `.python-version` |
| Java | `.java-version`、`.sdkmanrc` |
| Go | `go.mod` 的 `go` 指令、`.go-version` |
| Rust | `rust-toolchain.toml`、`rust-toolchain` |
| Maven / Gradle / Kotlin | `.mvn-version`、`.gradle-version`、`.kotlin-version` |
| Bun / Deno | `.bun-version`、`.dvmrc` |

普通单值文件读取第一个非空、非注释行，并去掉开头的 `v`。Rust TOML 读取
`[toolchain].channel`。Node 的 npm semver range 会解析到最高匹配稳定版；无效 range
会明确失败。

::: warning 无参数生命周期命令的当前边界
`current`、shim 和 shell hook 会为每个 backend 读取上述生态文件。但无参数
`lock`、`upgrade` 以及没有可用 lock 时的 `install`，目前主要枚举合并后的
`[tools]`/`.tool-versions`，再额外发现 `packageManager` 和 Node。仅存在
`.python-version`、`.java-version`、`go.mod`、`rust-toolchain.toml` 等文件时，
这些非 Node 工具不会自动加入无参数生命周期命令；请在 `[tools]` 中声明或显式传参。
:::

## 最小项目配置

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
rust = "1.91.1"
pnpm = "10.15.0"
"npm:prettier" = "3"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
"go:golang.org/x/tools/gopls" = { version = "0.20", tags = ["netgo"] }

[aliases.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` 会修改最近的项目配置；没有项目配置时在当前目录创建
`osdk.toml`。`osdk use --global node@20` 修改用户配置。
对于 `npm:<package>`，本地 `use` 会先查找最近的 `package.json`；下一节说明其项目感知
行为。
每个 `cargo:<crate-or-https-url>` 条目都必须对应一个精确、显式请求或配置的 `rust`
条目；浮动和本地链接的 Rust toolchain 会被拒绝。详见 [Cargo 开发工具](./cargo-tools)。
每个 `go:<module-or-command-path>` 条目也必须对应一个受管 `go` 选择。osdk 会先解析该
runtime，再把精确版本绑定到工具；详见 [Go 开发工具](./go-tools)。

## 在真实项目中使用 npm 工具

在 Node 项目下的任意目录执行以下命令，会把包加入最近 `package.json` 所在的项目：

```bash
osdk use npm:prettier@3
```

包会保留原有的 `dependencies`、`devDependencies`、`optionalDependencies` 或
`peerDependencies` 区段；新包默认加入 `devDependencies`。osdk 依次参考项目声明的
`packageManager`、现有 lock 的归属安装器，最后使用配置的默认值，也可以用
`-o installer=npm|pnpm` 显式指定。某个安装器失败后不会换另一个安装器重试。祖先目录中没有 `package.json` 时，该命令保留原有的 osdk 隔离安装与 shim
行为。

项目感知的 `use` 会更新原生 `package.json` 与包管理器 lock，再把精确 Node 选择和结构化
npm 条目写入 `osdk.toml`。它还会写一份紧凑的 `osdk.lock`，记录 installer、scope、Node
和原生 lock 身份。该 metadata 不包含传递依赖图；原生包管理器 lock 仍是依赖图来源。
项目中应同时保留这四类文件。安装器与激活细节见 [npm 开发工具](./npm-tools)。

为支持 Shell 激活，`use` 还会在 `.osdk/npm-bin/` 下生成本地派生状态。只有已配置 npm
工具的筛选 launcher 会被激活；整个 `node_modules/.bin` 永远不会加入 PATH。建议把
`/.osdk/npm-bin/` 加入 package 根目录的忽略文件；若仓库根包含嵌套 package，则可在
仓库根使用 `**/.osdk/npm-bin/`，同时继续提交上述四类事实来源文件。

## 控制哪些命令进 PATH

一个安装往往带来比你想要的更多可执行文件：conda prefix 装着整个依赖闭包，Android
NDK 有 172 个可执行文件。`[settings.shims]` 决定其中哪些生成 shim、进入 PATH。

不生成 shim 不等于没装：文件仍在 install 目录里，`osdk exec` 和激活的 shell 里照样
可用。

### 三个列表

| 设置 | 语义 | 作用域 |
| --- | --- | --- |
| `include` | **白名单**：非空时，未列出的名字一律不生成 | 全体工具 |
| `expose` | **增量**：额外放行默认被挡下的名字，不影响其他任何工具 | 全体工具 |
| `exclude` | 排除，最后生效，可修剪前两者的结果 | 全体工具 |

判定顺序：`include` 决定候选集合 → `expose` 追加 → `exclude` 最后剪除。

`expose` 会盖过别处的窄 `include`。两者命中同一个名字时，一边说「要这个」、一边说
「不在名单里」，让 `include` 赢就等于显式要求被静默丢弃。

### `include` 是全局白名单，不是「加回一个」

这是最容易出错的一点：

```bash
# 危险：想取回 make，结果 cargo / go / node 全部失去 shim
osdk config set shims.include "conda:m2-base:make"
```

`include` 一旦非空就成为对**全体工具**生效的白名单，列出 1 个名字等于声明「其余都不
要」。实测这一条命令把 646 个 shim 变成了 0。

要取回被挡下的命令，用 `expose`：

```bash
# 安全：只加不减
osdk config set shims.expose "conda:m2-base:make"
```

`include` 本身没有问题，它表达的是「只要这几个」——当你真的想大幅收窄时才用它，并且
优先用下面的按工具形式。

### 按工具覆盖

三个列表都可以限定到单个工具，键的形式是 `shims.<tool>.<字段>`：

```bash
osdk config set shims.conda:m2-base.expose  "make,sh,bash,tr,awk"
osdk config set shims.android-ndk.include   "clang,clang++,llvm-strip"
osdk config set shims.conda:m2-base.exclude "ls,test"

osdk config get   shims.conda:m2-base.expose
osdk config unset shims.conda:m2-base.expose
```

写进 TOML 的形态：

```toml
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "bash", "tr", "awk"]
```

按工具限定让 `include` 变得安全：作用域收窄之后，`android-ndk` 下的 `include` 只能
影响 NDK 自己的命令，不可能收走 `cargo`。**需要收窄某个工具时，优先用这个形式，而不
是全局 `include`。**

覆盖是**逐字段**的：写了哪个字段就覆盖哪个，没写的继承全局列表。因此「只调 expose」
不会顺手清掉全局的 exclude。`config get` 对未指定的字段显示 `inherit`，以区别于显式
设成空列表。

工具 key 按 backend id **精确匹配**，不支持通配；跨工具的模式请用全局列表。

### 模式语法

三个列表的元素都是模式，规则一致：

| 写法 | 匹配对象 | 例 |
| --- | --- | --- |
| 不含 `:` | 命令名 | `make`、`clang*` |
| 含 `:` | `<backend>:<命令名>` | `conda:m2-base:make`、`android-ndk:*` |

- `*` 匹配任意长度，`?` 匹配单个字符；
- 大小写不敏感（Windows 可执行文件本就如此）；
- 匹配整个名字，不是子串。

在按工具的列表里，两种写法都可用，但既然作用域已经限定，直接写命令名更清楚。

### 元包：默认一个 shim 都没有

`conda:m2-base` 这类元包自身不安装任何命令——prefix 里的东西都属于它拉进来的包，所以
默认不生成任何 shim。这是刻意的：否则 msys 版的 `ls`、`test`、`sort` 会盖住 Windows
同名命令。

需要其中几个时用 `expose` 点名：

```bash
osdk config set shims.conda:m2-base.expose "make,sh,bash,tr,awk,grep,printf"
osdk reshim
```

只暴露真正用到的命令，未列出的不会干扰系统命令。

### 改完要 reshim

`config set` 只改配置，不动磁盘上的 shim。改完执行：

```bash
osdk reshim
```

`osdk where --bins <tool>` 可以核对结果，它分别列出 `published`（生成 shim）与
`withheld`（挡下）两组。

::: tip 项目配置需要信任
把这些设置写进项目 `osdk.toml` 会让该文件需要信任，`config set` 会就地询问；改动后
重新 `osdk trust`。详见[项目配置信任](#项目配置信任)。
:::

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

[containers]
runtime = "auto"              # auto|docker|containerd
builder = "auto"              # auto 或经过验证的 Buildx 构建器名称
platform = "runtime"          # runtime 或 OS/ARCH[/VARIANT]
probe_timeout_ms = 1500

[tools]
node = "20"
python = "3.12"
pnpm = "10.15.0"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
installer = "npm"             # auto|npm|pnpm；隐式默认值为 auto
allow_builds = ["@scope/native-tool", "esbuild"]

[tools."http:https://downloads.example.com/acme-{version}.tar.gz"]
version = "1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
kind = "tar.gz"
strip-components = "1"
bin = "bin/acme"
rename = "acme"

[aliases.node]
default = "20"
```

Registry URL 会去重并补尾部 `/`。只允许带 host 的 HTTP(S) URL；credentials、
query 和 fragment 都会被拒绝。来源的选择语义见[下载源与供应链安全](./sources-security)，
Registry 的选择语义见[JavaScript 包管理器](./package-managers)。
结构化工具对象要求 `version`，其他 option 可以是字符串、布尔值或字符串数组；数组
传给 backend 时会转成逗号分隔值。`installer` 选择 npm 工具安装器。`allow_builds` 控制
隔离与全局安装；项目感知的 `use` 始终禁用 lifecycle scripts。完整安全边界见
[npm 开发工具](./npm-tools#构建脚本策略)。
`http:` 条目要求精确语义化版本，并为严格 HTTPS `{version}` 模板提供 SHA-256；
文件/归档布局与离线重放见[直接 HTTPS 制品](./http-artifacts)。

## 精确的覆盖与合并语义

总体优先级为：

```text
CLI > OSDK_* 环境变量 > 最近项目配置 > 用户配置 > 内置默认值
```

文件之间不是统一的“逐字段覆盖”：

| 配置区域 | 高优先级文件的行为 |
| --- | --- |
| `[settings]` | **整段替换**；只写一个字段时，其他设置回到 `Settings` 内置默认值，而不是继承用户文件 |
| `[sources]` 顶层 | `selection`、`probe_timeout_ms`、`cache_ttl` **整段替换**；遗漏项回到默认值 |
| `[sources.<tool>]` | 按工具键合并；同一工具的 `pin`、`disable`、`custom` 等整项由高优先级层替换 |
| 模型 `env`/`env_force` | 项目配置不能改变；始终保留用户全局值，避免项目静默改写 shell 凭据环境 |
| `[registries]` | 整段替换；项目 `[registries.npm]` 不与用户 URL 列表合并 |
| `[containers]` | 整段替换；省略的运行时、构建器、平台、超时和 Registry 策略字段使用内置默认值 |
| `[tools]` | 按工具键合并；高优先级同名键覆盖 |
| `[aliases.<tool>]` | 按工具和别名键合并；高优先级同名别名覆盖 |
| `.tool-versions` | 只填补合并后 `[tools]` 中缺失的工具 |

例如，用户配置启用了 `verify_signatures = false`，而项目仅写：

```toml
[settings]
jobs = 2
```

项目层会创建一整套默认 `Settings` 再设 `jobs = 2`，因此最终
`verify_signatures` 回到默认 `true`。若希望保留非默认组合，应在高优先级的
`[settings]` 中完整声明。

## 环境变量覆盖

| 环境变量 | 对应配置 |
| --- | --- |
| `OSDK_LINK_MODE` | `settings.link_mode` |
| `OSDK_JOBS` | `settings.jobs` |
| `OSDK_YES` | `settings.yes` |
| `OSDK_VERIFY_SIGNATURES` | `settings.verify_signatures` |
| `OSDK_REQUIRE_CHECKSUMS` | `settings.require_checksums` |
| `OSDK_ATTESTATIONS` | `settings.attestations` |
| `OSDK_OFFLINE` | `settings.offline` |
| `OSDK_PRERELEASE` | `settings.prerelease` |
| `OSDK_PYTHON_CATALOG_URL` | `settings.python.catalog_url` |
| `OSDK_PYTHON_CATALOG_SHA256` | `settings.python.catalog_sha256` |
| `OSDK_JAVA_CATALOG_URL` | `settings.java.catalog_url` |
| `OSDK_SELECTION` | `sources.selection`；未知值当前回退为 `auto` |
| `OSDK_CONTAINER_RUNTIME` | `containers.runtime`；`auto|docker|containerd` |
| `OSDK_CONTAINER_BUILDER` | `containers.builder`；`auto` 或经过验证的 Buildx 构建器名称 |
| `OSDK_CONTAINER_PLATFORM` | `containers.platform`；`runtime` 或 `OS/ARCH[/VARIANT]` |
| `OSDK_LANG` | 输出语言，优先于配置与 locale |

目录变量见[存储、Shell 与扩展](./storage-shell#目录布局与覆盖)。

## 项目配置信任

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` 可为配置文件或目录；目录会从该处向上找最近项目配置。只有顶层 `[tools]` 和
`[aliases]` 的项目文件通常无需信任，但 npm 工具项需要信任，因为 Shell 激活可能暴露
最终执行项目依赖文件的筛选 launcher；原始 `node_modules/.bin` 永远不会被激活。其他
顶层 section（包括 `settings`、`sources`、`registries` 或未知 section）也需要信任。
项目感知的 `osdk use npm:...` 成功后会信任它刚生成的 `osdk.toml` 精确内容；之后编辑
会使该记录失效，并阻止筛选 generation 激活。

```bash
osdk --yes trust                 # 最近项目配置
osdk --yes trust ./osdk.toml     # 指定文件
osdk trust list                  # active / stale 记录
osdk untrust                     # 撤销最近项目配置
```

持久信任身份由配置的规范路径和规范化 TOML 内容的 BLAKE3 共同决定。内容变化或
仓库移动会使记录变为 stale；仅空白或格式变化通常不会。软链接解析到真实目标。
trust store 位于 `$OSDK_CONFIG_DIR/trusted-configs.toml`；`trust list` 只报告 stale，
不会自动删除记录。

CI 可设置 `OSDK_TRUSTED_CONFIG_PATHS`，值是操作系统路径分隔符连接的已审阅文件或
目录。匹配文件或位于匹配目录下的项目配置会在本次进程中视为 trusted，不写本地
trust store。`trust`/`untrust` 自身只加载用户配置，防止未信任项目影响自己的审批。

`osdk config set` 和 `osdk config unset` 同样在信任检查之前执行。否则出口会被它自己
要撤销的那份配置堵死——`unset` 将无法删掉正导致拒绝的那个键。这两个命令只针对指定
文件里的指定键，不会执行未受信任配置的任何内容。`config get` 和 `config list` 仍受
限制，因为它们确实会读出并展示那份配置的合并结果。
