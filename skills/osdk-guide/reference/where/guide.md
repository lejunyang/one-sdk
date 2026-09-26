# `osdk where`

打印一个工具请求解析到的安装目录。

```bash
osdk where <TOOL[@VERSION]> [-g|--global] [--bins]
```

| 参数 | 语义 |
| --- | --- |
| `<TOOL>` | 工具名或带版本请求 |
| `-g, --global` | 从用户全局 npm 动态包作用域解析；其他 backend 不支持 |
| `--bins` | 同时列出发布的命令及因冲突/策略被保留的命令 |

```bash
osdk where node
osdk where node@20.11.1 --bins
osdk where -g npm:prettier --bins
```

只读查询。若要判断当前选择用 `current`；若要重新生成入口用 `reshim`。
