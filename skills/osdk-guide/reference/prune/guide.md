# `osdk prune`

以 SDK 安装和模型快照的 manifest 为根，回收 osdk CAS 中没有引用的对象。清理当前项目已弃用的模型快照用 `osdk model sync --prune`。

```bash
osdk prune [--dry-run]
```

`--dry-run` 仅输出演练提示。该命令不等同于：

- `osdk cache clean`：清下载归档和下游包缓存。
- `osdk cache prune`：只清未引用的下游缓存。
- `osdk container prune`：清宿主容器运行时的指定类别。

无专属配置；store 路径可用 `OSDK_STORE_DIR` 覆盖。
