# `osdk list-remote`

查询一个工具的远端可安装版本。

```bash
osdk list-remote <TOOL> [FILTER]
```

- 别名：`osdk lsr`。
- `FILTER` 是可选版本前缀，例如 `20`。
- 受全局 `--source`、`--refresh-sources`、`--offline` 和 `--prerelease` 影响。

```bash
osdk list-remote node
osdk list-remote node 20
osdk list-remote cargo:ripgrep 14
```

只查询，不安装、不写配置或 lock。离线模式下只能使用已有元数据缓存。
