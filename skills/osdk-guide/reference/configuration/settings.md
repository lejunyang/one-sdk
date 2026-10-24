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

`[settings.precedence]` 的 `tools` 控制默认工具来源顺序，`tool_order` 是全工具共享的来源类型数组，
`tool_conflicts=prefer|error` 控制排序选择或兼容性检查；`packages` 和 `npm`/`python`/`go` 则是项目包来源策略，
不能改变单个工具的版本顺序。逐字段继承，数组替换；`tool_order=[]` 清除继承的自定义顺序并禁用 system，
生态 `"inherit"` 清除包来源覆盖。未知/重复来源报错；其他遗漏来源自动补齐，system 只显式启用。
完整合法值、默认顺序、适用矩阵和配置示例统一维护在[工具选择](https://lejunyang.github.io/one-sdk/guide/config/tool-selection)。
`global` 是 osdk 用户 config.toml 的工具配置，不是 PATH；`osdk` 是最近项目配置及显式 extends。

### system 执行来源

`tool_order=["system","osdk"]` 启用受支持的已有工具，默认不启用。
支持范围、home 选择器、托管变量与缓存开关集中见[工具环境变量](https://lejunyang.github.io/one-sdk/guide/config/tool-environment)。
`override_env` 只控制对应缓存覆盖，不启用 system。显式版本、托管锁与 frozen 保持权威；
外部路径不写安装 receipt 或可移植锁，也不由 uninstall/cache 删除。没有通用 OSDK_TOOL_VERSION。

用 [current TOOL --sources](../current/guide.md#完整来源诊断) 查看项目适用/存在/缺失来源和有效排序；
默认不运行工具，严格 system 兼容性会标为待验证。`--probe` 显式允许探测，不安装缺失运行时。
