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
