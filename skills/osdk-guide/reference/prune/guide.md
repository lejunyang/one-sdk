# `osdk prune`

回收 osdk CAS store 中不再被任何安装引用的对象。

```bash
osdk prune [--dry-run]
```

`--dry-run` 只显示可回收对象，不删除。该命令不等同于：

- `osdk cache clean`：清下载归档和下游包缓存。
- `osdk cache prune`：只清未引用的下游缓存。
- `osdk container prune`：清宿主容器运行时的指定类别。

无专属配置；store 路径可用 `OSDK_STORE_DIR` 覆盖。
