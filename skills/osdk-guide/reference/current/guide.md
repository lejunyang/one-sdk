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

`current` 使用与执行相同的 `[settings.precedence]` 工具选择结果，输出 tool、path、field、category、status（winner/shadowed/ignored）和 reason。其他工具的声明只记录为 ignored，不解释其版本；已被近层覆盖的原生版本值不再校验。`tool_order`、冲突策略和范围兼容性详见 [settings](../configuration/settings.md)。
