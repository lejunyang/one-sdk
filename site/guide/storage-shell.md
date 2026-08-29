# 存储、Shell 与扩展

本页说明 osdk 的目录、CAS 和缓存边界，以及 shim、shell hook、临时执行、补全、
诊断和数据型声明式 backend。

## 目录布局与覆盖

| 用途 | Linux 默认位置 | 环境变量 |
| --- | --- | --- |
| 数据根 | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| 内容存储 | `<data>/store` | `OSDK_STORE_DIR` |
| SDK 安装 | `<data>/installs` | `OSDK_INSTALL_DIR` |
| 缓存根 | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| 配置目录 | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

派生目录：

```text
<data>/models                模型快照
<data>/shims                 命令 shim
<data>/rustup                隔离 RUSTUP_HOME
<data>/cargo                 Rust backend 的隔离 CARGO_HOME；受控 cargo-binstall 位置
<data>/plugins               声明式 backend
<cache>/downloads            SDK/模型下载文件
<cache>/tmp                  临时解压
<cache>/remote               metadata 与证明缓存
<cache>/sources              来源测速缓存
<cache>/pkg                  下游工具原生缓存
```

`OSDK_BIN_DIR` 属于 osdk 二进制安装脚本，不是 SDK 状态目录；不要把它与
`OSDK_INSTALL_DIR` 混用。

## CAS 与物化

验证、解压后的 SDK 文件和模型文件按 BLAKE3 内容哈希写入 CAS，路径形如
`store/<前2位>/<第3-4位>/<完整hash>`。安装/快照 manifest 记录相对路径、hash、Unix
mode 和 symlink 目标，再把对象物化到版本目录。

`settings.link_mode` 或 `OSDK_LINK_MODE` 接受：

| 模式 | 行为 |
| --- | --- |
| `auto` | 同文件系统优先 hardlink，再 reflink、copy；跨文件系统尝试 reflink 后 copy |
| `hardlink` / `hard` | 优先硬链接，失败回退 copy |
| `reflink` / `clone` / `cow` | 优先写时复制克隆，失败回退 copy |
| `copy` | 普通字节复制 |
| `symlink` / `sym` | 显式符号链接；失败不回退，且 `auto` 永不选择它 |

SDK/模型 CAS 不包含不同 package manager 的原生缓存；后者格式互不兼容，见
[原生缓存语义](./package-managers#原生缓存语义)。

## 缓存层与命令

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| 命令 | 当前行为 |
| --- | --- |
| `cache dir` | 打印缓存根、downloads、CAS 和 downstream 根目录 |
| `cache env` | 打印所有受支持的下游原生缓存环境映射 |
| `cache clean` | 只删除并重建 `<cache>/downloads` |
| `prune --dry-run` | 当前只输出演练提示，不计算或列出候选对象 |
| `prune` | 以 SDK installs 与模型 snapshots 的 manifest 为根，删除无引用 CAS 对象 |

`cache clean` 不删除 CAS、安装、模型、remote/source metadata 或 `<cache>/pkg`。
GC 遇到损坏 manifest 会拒绝继续，防止误删仍在使用的对象。

`osdk container cache status` 是另一条路径：它通过受支持的原生聚合接口查询 Docker
Engine 或 Buildx 构建器自有的存储。详见[容器运行时、Registry 与原生操作](./containers)。

通用 shell hook 在用户未设置时还映射
`npm_config_cache`、`PIP_CACHE_DIR`、`GOMODCACHE`、`GOCACHE`、`CARGO_HOME` 和
`GRADLE_USER_HOME`。这里没有 Maven `M2_HOME`/`maven.repo.local` 重定向。Rust 的
直接 shim/`exec` 会以 `<data>/cargo` 覆盖通用 `<cache>/pkg/cargo` 映射。
Cargo 开发工具 provider 不会把两者当作构建 cache：每次安装都有 stage 私有的 `HOME`、
`CARGO_HOME`、target 与安装根。只有符合条件的受控 `cargo-binstall` 会从
`<data>/cargo/bin` 发现；临时 source/build workspace 会在发布前删除。

## 删除与确认

```text
osdk uninstall|rm TOOL@VERSION
osdk cache clean
osdk prune [--dry-run]
osdk model remove NAME
```

`uninstall`、`cache clean` 和非演练 `prune` 需要确认。交互终端显示提示；非交互
环境必须使用 `--yes`、`OSDK_YES=true` 或 `settings.yes=true`，否则失败。
`--quiet` 只关闭进度，不代表同意。`model remove` 当前不要求确认。
对 osdk 自有动态工具，uninstall 会派生完整配置身份，只删除对应指纹化根；相同 backend 和
version 的其他身份仍然保留。

## Shim 与 Shell 激活

安装和 `use` 会生成 shim；如果安装目录变化，可重建：

```text
osdk reshim
```

对动态工具，`reshim` 只为配置精确匹配的 `.osdk-install.json` schema 1 身份发布 launcher，
不会选择同版本的其他根，旧 `.osdk-tool.json` 状态也永远不可执行。

shim 在每次执行时按当前目录解析版本，因此 IDE、CI 和未安装 prompt hook 的进程也
能使用项目 pin。它会避免递归调用自身；Windows `.cmd`/`.bat` 目标通过
`%ComSpec% /D /S /C call` 执行以保留参数、stdin/stdout 和状态。

Shell 激活命令：

```text
osdk activate bash|zsh|fish|powershell|pwsh
osdk deactivate bash|zsh|fish|powershell|pwsh
```

常见接入方式：

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

hook 在 prompt/目录变化时重新计算环境。PATH 顺序为：

```text
已生成 shims > 独立 npm/pnpm/yarn > Node > 其他 backend > 原 PATH
```

它还导出 backend 环境、下游缓存和已启用的模型 adapter。osdk 保存所有被管理变量
的原值；`deactivate` 移除 hook 并恢复环境。PowerShell hook 带重入保护。

## 临时执行

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

`-t/--tool` 至少一个且可重复。osdk 先安装指定版本，再把 backend bin 和环境加入
单个子进程；不修改项目 pin，也不读取 lock。

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool node@20 --tool pnpm@10 -- pnpm test
osdk exec -t bun@latest -- bunx vite
```

`pnpx` 路由到受管 `pnpm dlx`，`bunx` 路由到受管 `bun x`；未声明对应 backend
会报错。包管理器调用可能在启动前执行 Registry 预检。子命令失败会使 osdk 返回
错误，但当前不保证原样透传其数值退出码。

## 补全

```text
osdk completions bash|elvish|fish|powershell|zsh
```

命令把补全脚本写到 stdout；请按 shell 的惯例保存或 source。例如：

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions zsh > ~/.zfunc/_osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```

## 诊断

```text
osdk doctor
osdk config path
osdk config list
osdk source list TOOL
osdk registry test [MANAGER]
```

| 命令 | 输出 |
| --- | --- |
| `doctor` | 平台、data/store/install 目录、store 与 install 是否同文件系统、shim 路径及是否在 PATH、backend ID |
| `config path` | 配置目录、用户配置文件、当前项目配置 |
| `config list` | 部分最终设置与目录、registry、模型环境、tools、aliases |
| `source list` | 某 backend/provider 的来源与 pin；`doctor` 不列镜像 |
| `registry test` | npm-compatible Registry 的匿名探测与选择计划 |
| `container doctor` | Docker/containerd 只读选择，以及独立的 Buildx 报告 |

顶层 `doctor` 当前不直接打印 `link_mode`；使用 `config list` 查看。它与诊断原生容器
控制面的 `container doctor` 不同。

## 声明式 Backend

osdk 启动时自动加载以下目录直接子级的 `*.toml`，无需单独安装命令：

```text
<config>/plugins/*.toml
<data>/plugins/*.toml
```

config 目录先加载，再加载 data 目录。任何非法定义或与内置/其他 backend 的 ID/别名
冲突都会使 registry 加载失败，不允许 shadow。只加载普通文件，不递归；每个文件
最大 1 MiB。

### Schema 1 示例

```toml
schema = 1
id = "acme"
bin_paths = ["bin"]
bin_names = ["acme", "acmectl"]
idiomatic_files = [".acme-version"]

[versions]
values = ["1.0.0", "1.2.3"]
# 或 url = "https://example.test/{os}/{arch}/versions.txt"

[archive]
url = "https://example.test/{version}/{file}"
file = "acme-{version}-{os}-{arch}.tar.gz"
kind = "tar.gz"          # tar.gz|tar.xz|tar.zst|zip
strip_root = true

[archive.checksum]
algorithm = "sha256"     # sha256|sha512|blake3
url = "{archive_url}.sha256"
# 或 value = "<固定十六进制摘要>"
```

### 验证与安全边界

- `versions.values` 与 `versions.url` 必须且只能设置一个，最多 10,000 个版本；
- ID 首字符为小写 ASCII 字母或数字，其余仅小写字母、数字、`-`、`_`，并保留
  `github` namespace；
- `bin_paths`、`bin_names` 不能为空，所有路径/名称必须安全且留在安装根内；
- archive URL 或文件名至少一处必须随 `{version}` 变化；
- archive 类型仅为 `tar.gz|tar.xz|tar.zst|zip`；
- checksum 的 `value` 与 `url` 必须且只能设置一个；长度必须符合算法；
- versions/archive URL 只接受 HTTP(S)，checksum URL 额外可基于 `{archive_url}`；
- 允许的模板变量按位置为 `{id}`、`{version}`、`{os}`、`{arch}`、`{libc}`、
  `{file}`、`{archive_url}`；不支持的变量会失败；
- schema 使用严格未知字段拒绝，因此不能加入 hook 或 install script。

声明式 backend 只描述数据，不能执行自定义代码；安装仍经过统一下载、checksum、
受限解压和 CAS 物化管线。`osdk lock` 记录 artifact receipt 后，后续不带参数的
`osdk install` 会先使用其中锁定的 URL、文件名、checksum 与可选子目录，再考虑当前
插件模板。因此即使版本/checksum 端点不可用，或本地插件定义后来发生变化，已缓存产物
仍能离线重装。
