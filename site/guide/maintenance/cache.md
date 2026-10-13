# 缓存与清理

本页说明 osdk 的缓存分层、相关命令，以及删除操作的确认规则。目录位置见[目录与存储](../config/storage)。

## 缓存层与命令

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| 命令 | 当前行为 |
| --- | --- |
| `cache status [--json]` | 查看下载副本占用、策略、重试和可回收候选 |
| `cache trim [--dry-run] [--json]` | 按策略回收已登记的下载副本；不清 CAS 或原生缓存 |
| `cache prune` | 委托 uv 回收其未使用缓存；无预览 |
| `cache dir` | 打印缓存根、downloads、CAS 和 downstream 根目录 |
| `cache env` | 打印所有受支持的下游原生缓存环境映射 |
| `cache clean` | 删除下载副本及 uv/pip 原生缓存，保留 CAS 与安装 |
| `prune --dry-run` | 当前只输出演练提示，不计算或列出候选对象 |
| `prune` | 以 SDK installs 与模型 snapshots 的 manifest 为根，删除无引用 CAS 对象 |

`cache clean` 不删除 CAS、安装、模型或 remote/source metadata；在 `<cache>/pkg` 中只清 uv/pip，不清其他 manager store。
GC 遇到损坏 manifest 会拒绝继续，防止误删仍在使用的对象。

`osdk container cache status` 是另一条路径：它通过受支持的原生聚合接口查询 Docker
Engine 或 Buildx 构建器自有的存储。详见[容器运行时、Registry 与原生操作](../system/containers)。

通用 shell hook 在用户未设置时还映射
`npm_config_cache`、`PIP_CACHE_DIR`、`UV_CACHE_DIR`、`GOMODCACHE`、`GOCACHE`、
`CARGO_HOME` 和 `GRADLE_USER_HOME`。uv 不读 `pip.conf`，也不认 `PIP_*` 设置，
因此它需要自己的 `UV_CACHE_DIR`（`<cache>/pkg/uv`）；`PIP_CACHE_DIR` 覆盖不到它。
这里没有 Maven `M2_HOME`/`maven.repo.local` 重定向。Rust 的
直接 shim/`exec` 会以 `<data>/cargo` 覆盖通用 `<cache>/pkg/cargo` 映射。
Cargo 开发工具 provider 不会把两者当作构建 cache：每次安装都有 stage 私有的 `HOME`、
`CARGO_HOME`、target 与安装根。只有符合条件的受控 `cargo-binstall` 会从
`<data>/cargo/bin` 发现；临时 source/build workspace 会在发布前删除。
Go 开发工具 provider 则把 module/build cache 固定到 `<cache>/pkg/go-mod` 和
`<cache>/pkg/go-build`；其 `HOME`、`GOPATH`、`GOBIN` 与临时目录属于 stage 私有状态，
发布前会删除。

## 下载副本保留策略

默认仅为 osdk 自有的 SDK 归档和模型下载副本保留最多 **2 GiB**，超过 **7 天**未用于安装/拉取的副本会在维护时回收。容量限制不阻止下载或安装更大的文件：正在使用的临时文件可以超出预算，校验、内容存储提交及安装后处理全部成功后，超预算单文件立即删除下载副本。没有后台定时器；维护在安装/拉取前后及显式 `cache trim` 时触发。

```toml
[settings.download_cache]
max_size_bytes = 2147483648  # 默认 2 GiB；0 = 成功后不保留；"unlimited" = 不限容量
max_age_secs = 604800       # 默认 7 天；0 = 禁用年龄回收
```

容量也接受 `"2GiB"`、`"2GB"` 等整数单位（GiB 为二进制，GB 为十进制）。

```bash
osdk config set --global download_cache.max_size_bytes 0
osdk config set --global download_cache.max_age_secs 0
osdk --download-cache-max-size 2GiB model sync qwen
osdk cache status --json
osdk cache trim --dry-run --json
osdk cache trim
```

`OSDK_DOWNLOAD_CACHE_MAX_SIZE` 与 `--download-cache-max-size` 可覆盖容量；CLI 优先。`config get` 显示规范的字节数或 `unlimited`。超过总预算时按最近安装/拉取使用时间回收，不使用文件系统 atime。容量 0 不禁用临时下载；安装失败或中断的文件及 `.partial`/验证器暂存保留供重试，最近使用/partial 写入后的 10 分钟受保护，之后可因容量/年龄策略回收。`status` 分别报告完成副本、重试文件、受保护的重试字节和未登记的旧缓存。

所有本版本的安装/拉取持跨进程共享锁直到发布/回滚结束；任何事务在进行时清理整体延后，报告 `busy` 和实际超限，每次成功的事务结束都会再尝试维护；忙碌/中断导致延期时由后续安装/拉取或 `cache trim` 再触发。不会强删在用文件。离线不执行安装前或失败后的自动回收，但成功消费的下载副本仍遵守容量预算（包括 0）。已存在且尚未由安装/拉取登记的旧下载只报告为 `unmanaged_bytes`，不自动迁移或删除。

CAS、已安装 SDK/模型、索引 metadata、下游原生包缓存均不计入该预算。`allocated_bytes` 在 Unix 上按分配块估算；回收上界排除多硬链接 inode，reflink 共享等仍可能降低实际释放量。不能把路径的逻辑大小当作实际可释放磁盘空间。已知 HTTP 长度会另行检查可用空间；未知长度、并发写入、解压和 CAS 复制仍可能耗尽磁盘。

模型快照的两个 manifest 与 CAS blob 完整时，离线拉取可验证并重建所记录的文件集，即使下载副本和远端 metadata 已删。离线 alias 使用最近记录的匹配快照，不能判断服务器是否已更新，也不能补出记录之外的文件。缺失/损坏 blob 或 manifest 明确失败，不联网。修复只恢复缺失文件；遇到额外或已修改的数据会保留原状并报错，不擅自覆盖。发布或 `current` 记录失败会回滚旧快照。`model remove` 或 CAS GC 已删除唯一内容时无法冷恢复。SDK 归档重装仍需保留归档或联网；原生管理器的离线依赖仍由其自身协议决定。

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
