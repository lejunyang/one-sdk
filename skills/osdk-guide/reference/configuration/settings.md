# `[settings]`

配置 osdk 自身默认行为。CLI 和环境变量可覆盖这些值。

| 字段 | 类型/取值 | 默认 | config key / 环境变量 |
| --- | --- | --- | --- |
| `link_mode` | `auto\|hardlink\|symlink\|copy\|clone` | `auto` | `link_mode` |
| `jobs` | 正整数 | CPU 数，上限 8 | `jobs` / `OSDK_JOBS` |
| `yes` | bool | false | `yes` |
| `verify_signatures` | bool | true | `verify_signatures` |
| `require_checksums` | bool | false | `require_checksums` / `OSDK_REQUIRE_CHECKSUMS` |
| `attestations` | `off\|if-available\|required` | `off` | `attestations` / `OSDK_ATTESTATIONS` |
| `offline` | bool | false | `offline` / `OSDK_OFFLINE` |
| `lang` | `en\|zh` | 按 locale | `lang` / `OSDK_LANG` |
| `prerelease` | `never\|if-explicit\|allow` | `if-explicit` | `prerelease` / `OSDK_PRERELEASE` |
| `override_tools_env` | bool | false | `override_tools_env` / `OSDK_OVERRIDE_TOOLS_ENV` |

```toml
[settings]
jobs = 4
require_checksums = true
attestations = "if-available"
# override_tools_env = true        # 总开关：对所有工具强制覆盖其可重定向环境变量

[settings.node]
override_env = true                # 仅对 node 生效；与总开关取 OR
corepack = false

[settings.pnpm]
override_env = true

[settings.npm]
default_installer = "npm"       # npm / pnpm

[settings.python]
catalog_url = "https://example/catalog.json"
catalog_sha256 = "<64 hex>"

[settings.java]
catalog_url = "https://example/packages"

[settings.shims]
include = []                     # 非空即全局白名单，慎用
expose = ["make"]               # 在默认判断外额外暴露
exclude = ["apkanalyzer"]       # 最后应用，总是胜出

[settings.shims.tools."conda:m2-base"]
expose = ["make"]
```

`shims.include/expose/exclude` 支持 `*`、`?`，大小写不敏感；也可写
`backend:pattern`。每工具表按 backend id 精确匹配，不使用 glob。

## 每工具段 `[settings.<tool>]`

每个工具段都有一个通用字段 `override_env`，其余 key 是该工具专属字段；没有专属字段的工具只接受 `override_env`，写别的 key 会报错。

| 通用字段 | 类型 | 默认 | 含义 |
| --- | --- | --- | --- |
| `override_env` | bool | false | 强制以 osdk 受管值覆盖该工具已存在的可重定向环境变量；与顶层 `override_tools_env` 取 OR |

默认（false）时，osdk 尊重你已经设置的工具缓存/数据目录变量（如 `npm_config_cache`、`PIP_CACHE_DIR`、`GOMODCACHE`、pnpm 的 store 变量等）；打开后这些变量也被强制指向 osdk 受管目录。工具的定位类变量（如 `npm_config_prefix`、`PNPM_HOME`、`JAVA_HOME`、`CARGO_HOME`）始终由 osdk 无条件设置，不受此开关影响。

## `[settings.download_cache]`

`max_size_bytes` 默认 2147483648；0 在成功提交后不保留副本，`"unlimited"` 不限容量；支持整数 GiB/MiB/KiB 和 GB/MB/KB。`max_age_secs` 默认 604800，0 禁用过期。对应 config key 是 `download_cache.max_size_bytes` 和 `download_cache.max_age_secs`；容量可用 `OSDK_DOWNLOAD_CACHE_MAX_SIZE` / `--download-cache-max-size` 覆盖。预算只管登记的下载副本，不能阻止大模型下载，不清 CAS/安装/原生 store。失败 partial 前 10 分钟受保护。没有后台清理定时器。详见 [cache](../cache/guide.md)。

`[settings.precedence]`：`tools = "osdk" | "native"`（默认 osdk），`packages = "osdk" | "native"`（默认 native）。本次 CLI 和锁重放不参与此排序。详见 `registries.md`。

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

`tool_order` 是所有工具共用的一份来源类型顺序，从高到低排列，不是按工具名分别配置的表。仅适用于当前工具的来源参与选择；每个文件名取最近声明，`idiomatic` 还保留下述 backend 固定文件顺序。支持 `osdk`、`global`、
`.tool-versions`、`.nvmrc`、`.node-version`、`idiomatic`、
`package.json#packageManager`、`package.json#devEngines.packageManager`、
`package.json#engines.node`、`package.json#devEngines.runtime`。遗漏项按默认顺序补齐，
不代表禁用；未知值和重复值报错；`[]` 清除继承的自定义顺序。`idiomatic` 将其他原生文件归为一类：Python 使用 `.python-version`；Go 先 `go.mod` 后 `.go-version`，Rust 先 `rust-toolchain.toml` 后 `rust-toolchain`，Java 先 `.java-version` 后 `.sdkmanrc`。这些文件名不能独立写入 `tool_order`；不同文件名之间先按 backend 顺序，再考虑目录距离。默认顺序是项目 osdk、
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
