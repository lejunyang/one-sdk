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


显式配置 `tool_order=["system"]` 可使用受支持的已有 PATH/home 工具；默认不启用。显式版本及托管锁/frozen 仍保持权威。参见[system 配置、适配器和限制](../configuration/settings.md#system-执行来源)。

## 完整来源诊断

```sh
osdk current node --sources
osdk current python --sources --project ./apps/api --json
osdk current pnpm --sources --probe
```

`--sources` 必须指定 TOOL；`--json`、`--project DIR`、`--probe` 必须与 `--sources` 一起用。
默认只读检查，不执行系统工具。报告分开显示：全部合法类型对该工具是否适用、继承后的
自定义顺序和补齐后的有效顺序、文件是否存在和声明是否存在、真实选择证据。
`missing` 不代表文件一定缺失，文件可能没有对应字段；`inapplicable` 与未启用的
`system`（`disabled`）不同。类型的合法值/适用范围见[集中参考](https://lejunyang.github.io/one-sdk/guide/config/tool-selection#tool-order-reference)。

严格策略需要 system 验证时，`resolution_status=verification-required` 和 `candidate`
表示临时排序结果。`--probe` 才允许有界原生版本探测（JS 管理器可能探测选中的 Node），
经过配置 trust 门禁，不安装缺失运行时。现有不带 `--sources` 的 current 行为保持不变。

JSON 为单个 `schema_version=1` 对象，包含 tool/project/configured_order/effective_order/
tool_conflicts/inventory_complete/sources/evidence/selected/resolution_status/error/notes。
已知解析/探测失败输出报告并非零退出；无效配置或未知 backend 可在报告建立前失败，只有
stderr。清单不完整时 unknown 不能当作 missing，退出码零也不保证严格 system 验证完成。
不透明选择器脱敏，不输出环境变量值。显式 CLI 请求、动态包 receipt 和锁回放不由此诊断模拟。
`config get precedence.tool_order` 尚不支持；直接编辑 TOML。
