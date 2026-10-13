# 环境变量

osdk 读取的 `OSDK_*` 变量都列在本页，按用途分组。总体优先级为
`命令行参数 > OSDK_* 环境变量 > 项目配置 > 用户配置 > 内置默认值`，配置文件之间的精确合并规则见
[配置文件](./files#精确的覆盖与合并语义)。

布尔变量接受 `1`、`true`、`yes`、`on`（不区分大小写）为真，其余任何值都视为假。枚举变量的值
无法识别时会被忽略，继续使用配置中的值，`OSDK_SELECTION` 例外，见下表。

## 目录

| 变量 | 作用 | Linux 默认值 |
| --- | --- | --- |
| `OSDK_DATA_DIR` | 数据根：安装、CAS、shim、模型 | `~/.local/share/osdk` |
| `OSDK_CACHE_DIR` | 缓存根：下载、metadata、原生缓存 | `~/.cache/osdk` |
| `OSDK_CONFIG_DIR` | 配置目录：`config.toml`、信任记录 | `~/.config/osdk` |
| `OSDK_STORE_DIR` | 内容寻址存储 | `<data>/store` |
| `OSDK_INSTALL_DIR` | SDK 安装目录 | `<data>/installs` |

安装脚本会询问前三项并写进 shell 启动文件，通常不必手工设置。派生目录与 CAS 见
[目录与存储](./storage)。

## 行为设置

这些变量覆盖 `[settings]` / `[sources]` / `[container]` 中的同名字段。

| 变量 | 对应配置 | 取值 |
| --- | --- | --- |
| `OSDK_JOBS` | `settings.jobs` | 正整数；同时是 `--jobs` 的环境变量 |
| `OSDK_MODEL_JOBS` | `sources.model_jobs` | 正整数；`--model-jobs` 的环境变量 |
| `OSDK_YES` | `settings.yes` | 布尔；对所有确认提示回答「是」 |
| `OSDK_OFFLINE` | `settings.offline` | 布尔；禁止网络，只用缓存 |
| `OSDK_LINK_MODE` | `settings.link_mode` | `auto`、`hardlink`、`reflink`、`copy`、`symlink` |
| `OSDK_LANG` | 输出语言 | `en`、`zh`；优先于配置与系统 locale |
| `OSDK_PRERELEASE` | `settings.prerelease` | `never`、`if-explicit`、`allow` |
| `OSDK_NPM_DEFAULT_INSTALLER` | `settings.npm.default-installer` | `npm`、`pnpm` |
| `OSDK_OVERRIDE_TOOLS_ENV` | `settings.override_tools_env` | 布尔；只能开启，不能用它关闭配置里已开启的值 |
| `OSDK_PYTHON_CATALOG_URL` | `settings.python.catalog_url` | HTTP(S) URL 或本地路径 |
| `OSDK_PYTHON_CATALOG_SHA256` | `settings.python.catalog_sha256` | 十六进制 SHA-256 |
| `OSDK_JAVA_CATALOG_URL` | `settings.java.catalog_url` | URL |
| `OSDK_CONTAINER_RUNTIME` | `container.runtime` | `auto`、`docker`、`containerd` |
| `OSDK_CONTAINER_BUILDER` | `container.builder` | `auto` 或经过验证的 Buildx 构建器名称 |
| `OSDK_CONTAINER_PLATFORM` | `container.platform` | `runtime` 或 `OS/ARCH[/VARIANT]` |

## 下载源与安全

| 变量 | 对应配置 | 取值 |
| --- | --- | --- |
| `OSDK_SELECTION` | `sources.selection` | `auto`、`pinned`、`ordered`；**未知值回退为 `auto`** |
| `OSDK_SOURCE_MODE` | `sources.mode` | `auto`、`env`；`--source-mode` 的环境变量 |
| `OSDK_VERIFY_SIGNATURES` | `settings.verify_signatures` | 布尔 |
| `OSDK_REQUIRE_CHECKSUMS` | `settings.require_checksums` | 布尔 |
| `OSDK_ATTESTATIONS` | `settings.attestations` | `off`、`if-available`、`required` |
| `OSDK_TRUSTED_CONFIG_PATHS` | 无 | 用系统路径分隔符连接的已审阅文件或目录；本次进程把它们下面的项目配置视为已信任，详见[项目配置信任](./trust) |

`OSDK_SOURCE_MODE` 决定 `RUSTUP_DIST_SERVER`、`GOPROXY`、`npm_config_registry` 这类生态
原生镜像变量的用法，见[下载源与镜像](../sources/mirrors#环境变量里的镜像)。

## 凭据

左侧的变量优先。osdk 只把凭据发给对应服务的官方端点，跨源跳转时移除。

| 服务 | 变量 |
| --- | --- |
| GitHub API | `OSDK_GITHUB_TOKEN`、`GITHUB_TOKEN`、`GH_TOKEN` |
| Hugging Face | `OSDK_HF_TOKEN`、`HF_TOKEN`、`HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`、`MODELSCOPE_API_TOKEN` |
| Civitai | `OSDK_CIVITAI_TOKEN`、`CIVITAI_API_TOKEN`、`CIVITAI_TOKEN` |

模型端点还可以用 `HF_ENDPOINT`、`MODELSCOPE_ENDPOINT` / `MODELSCOPE_DOMAIN`、`CIVITAI_ENDPOINT`
覆盖，优先级见[模型快照](../ai/models#endpoint-与凭据)。

## 日志

| 变量 | 作用 |
| --- | --- |
| `OSDK_LOG` | `tracing` 过滤表达式，覆盖 `-v` 系列的默认值；例如 `OSDK_LOG=debug` 打开所有 crate 的调试日志 |

## 安装脚本

`install.sh` / `install.ps1` 只在安装 osdk 本身时读取这些变量，osdk 运行时不读：

| 变量 | 作用 |
| --- | --- |
| `OSDK_VERSION` | 要安装的版本，默认 `latest` |
| `OSDK_BIN_DIR` | 二进制安装目录；与 SDK 状态目录 `OSDK_INSTALL_DIR` 无关 |
| `OSDK_REPOSITORY` | GitHub 的 `owner/repo` |
| `OSDK_DOWNLOAD_BASE_URL` | GitHub 或下载镜像的根地址 |
| `OSDK_TARGET` | 覆盖自动识别的平台目标 |
| `OSDK_SKIP_VERIFY` | 设为 `1` 时跳过 SHA-256 校验，不推荐 |
| `OSDK_SETUP_SHELLS` | 要配置的 shell：`all`、`none` 或逗号分隔列表 |
| `OSDK_ACCEPT_DEFAULTS` | 设为 `1` 时不询问，全部接受默认值 |

完整参数见[安装](../start/installation#自定义安装)。

## osdk 自己写入的变量

以下变量由 osdk 设置给子进程或 shell，用于识别状态，不应手工设置：

| 变量 | 含义 |
| --- | --- |
| `OSDK_TASK`、`OSDK_TASK_NAME` | 当前进程是 `osdk run` 启动的任务，以及任务名；shell 钩子见到 `OSDK_TASK` 会跳过重新激活 |
| `OSDK_SHIM_ACTIVE` | 正在经由 shim 运行的工具名，用来拒绝 shim 递归调用自身 |
| `OSDK_MANAGED_ENV`、`OSDK_ORIG_<NAME>`、`OSDK_ORIGINAL_PATH` | 激活时记录被接管的变量及其原值，`deactivate` 据此恢复 |

`[settings.download_cache]` 使用 `max_size_bytes`（默认 2147483648；0 不保留；`"unlimited"` 不限）与 `max_age_secs`（默认 604800；0 不过期）。`OSDK_DOWNLOAD_CACHE_MAX_SIZE` 覆盖容量。详见[缓存与清理](../maintenance/cache)。
