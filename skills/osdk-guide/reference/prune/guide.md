# `osdk prune`

回收 osdk CAS store 中不再被任何安装引用的对象。

```bash
osdk prune [--dry-run]
```

`--dry-run` 只显示可回收对象，不删除。该命令不等同于：

- `osdk cache clean` 清理下载归档与受管 uv、pip 缓存。
- `osdk cache prune` 委托受管 uv 回收未使用的 uv 缓存。用法见 [cache](../cache/guide.md)。
- `osdk container prune`：清宿主容器运行时的指定类别。

无专属配置；store 路径可用 `OSDK_STORE_DIR` 覆盖。
