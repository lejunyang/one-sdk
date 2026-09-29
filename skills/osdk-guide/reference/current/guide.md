# `osdk current`

显示当前目录实际生效的工具版本。

```bash
osdk current [TOOL]
```

不带参数列全部选择；带参数只查一个工具。结果来自配置分层与版本解析，不等同于
“本机安装了什么”（后者用 `list`）。

```bash
osdk current
osdk current node
```

相关配置：项目/全局 `[tools]`、`.tool-versions` 与 `[alias.tools]`。需要看安装目录和命令
归属时用 `osdk where <tool> --bins`。
