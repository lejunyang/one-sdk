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
```

`config path` 显示配置目录、用户配置文件和当前发现的项目配置。`config list` 显示
部分解析结果而非原始 TOML；它不会列出 `yes`、`lang`、所有 source 细节等全部字段。

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
pnpm = "10.15.0"

[aliases.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` 会修改最近的项目配置；没有项目配置时在当前目录创建
`osdk.toml`。`osdk use --global node@20` 修改用户配置。

## 完整配置参考

以下示例覆盖当前可编辑 schema。省略的字段使用该文件层反序列化时的默认值；
下一节解释这为何不等于继承低优先级层。

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

[tools]
node = "20"
python = "3.12"
pnpm = "10.15.0"

[aliases.node]
default = "20"
```

Registry URL 会去重并补尾部 `/`。只允许带 host 的 HTTP(S) URL；credentials、
query 和 fragment 都会被拒绝。来源的选择语义见[下载源与供应链安全](./sources-security)，
Registry 的选择语义见[JavaScript 包管理器](./package-managers)。

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
| `OSDK_LANG` | 输出语言，优先于配置与 locale |

目录变量见[存储、Shell 与扩展](./storage-shell#目录布局与覆盖)。

## 项目配置信任

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` 可为配置文件或目录；目录会从该处向上找最近项目配置。只有顶层
`[tools]` 和 `[aliases]` 的项目文件无需信任。只要出现其他顶层 section（包括
`settings`、`sources`、`registries` 或未知 section），普通命令就会要求先信任。

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
