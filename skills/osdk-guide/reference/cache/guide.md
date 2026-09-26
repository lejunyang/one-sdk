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
缓存路径可由 `OSDK_CACHE_DIR` 覆盖，没有 `osdk.toml` 专属段。
