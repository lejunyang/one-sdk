# `osdk cache`

查看或清理共享下载缓存与下游包管理器缓存；不等同于 store GC。

| 子命令 | 语义 |
| --- | --- |
| `dir` | 打印共享缓存目录 |
| `env` | 打印 uv/pip/npm 等下游缓存重定向变量 |
| `clean` | 删除下载归档和 uv/pip 缓存，保留 CAS store 与安装 |
| `prune` | 只删没有环境继续引用的缓存条目 |

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
osdk cache prune
```

`clean` 更彻底，之后可能重新下载；`prune` 更保守且没有 dry-run，因为底层 uv prune
没有可忠实映射的预览模式。回收 osdk store 中不再引用的对象使用一级命令 `osdk prune`。
缓存目录路径可由 `OSDK_CACHE_DIR` 覆盖；下载副本保留策略使用下文的 `[settings.download_cache]`。

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
