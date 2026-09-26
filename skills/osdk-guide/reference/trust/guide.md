# `osdk trust`

按配置文件规范路径与内容摘要记录信任，允许项目配置影响执行或字节来源。

```bash
osdk trust [PATH]
osdk trust list
osdk trust prune [--dry-run]
```

| 形式 | 语义 |
| --- | --- |
| `trust [PATH]` | 信任文件或目录；省略时使用最近项目配置 |
| `trust list` | 列出内容绑定的信任记录 |
| `trust prune --dry-run` | 预览文件已不存在且父目录可读的陈旧记录 |
| `trust prune` | 删除上述陈旧记录 |

配置内容变化后旧摘要不再授权新内容，需要重新审阅并 trust。不可访问路径不会被 prune，
因为它可能只是未挂载磁盘。仅 `[tools]`、`[aliases]` 等安全声明不需信任；具体原因见
`reference/configuration/overview.md`。
