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

```toml
[settings]
jobs = 4
require_checksums = true
attestations = "if-available"

[settings.node]
corepack = false

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
