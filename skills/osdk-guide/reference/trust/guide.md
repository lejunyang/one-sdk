# `osdk trust`

按配置文件规范路径与受管内容摘要记录信任。门是按命令作用域开的：每个命令只会
被它实际能碰到的受管键挡住。

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

作用域：安装类命令看校验设置/`sources`/`registries`，`run` 看 `[task]`，
`pkg apply` 只看在本机适用的 `[sys.pkg]` 条目；`models` 不要求信任。受管内容变化后
旧摘要不再授权新内容。不可访问路径不会被 prune，因为它可能只是未挂载磁盘。
