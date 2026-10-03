# `osdk use`

安装（若缺失）并把一个工具设为当前选择。

```bash
osdk use <TOOL[@VERSION]> [-g|--global] [-o KEY=VALUE]...
```

| 参数 | 语义 |
| --- | --- |
| `<TOOL>` | 单个工具请求 |
| `-g, --global` | 写用户全局选择；npm 动态包使用受控全局 prefix |
| `-o, --opt` | backend 选项，可重复 |

```bash
osdk use node@20
osdk use -g python@3.12
osdk use npm:prettier@3 -o installer=pnpm
```

项目级选择写入最近的 `osdk.toml [tools]`，并把实际安装的精确工具及受管 runtime 依赖
写入同目录的当前平台 `osdk.lock`。配置与 lock 作为一个元数据事务发布：lock 更新失败时
恢复原配置。已有结构化工具若被 `when` 排除在当前平台之外，`use TOOL@VERSION` 只更新其
`version`，保留 `when`、`lazy` 与 backend 选项；不在错误平台安装，也不为该平台写 lock。
此路径不能同时修改 `-o/--opt`，backend 选项需在匹配平台上更新并验证。Node 项目里的
`npm:` 工具还会按项目语义更新最近的 `package.json` 和原生 package-manager lock。全局
`use` 不写项目 lock；依赖原生安装图的全局 npm/Go 工具继续使用用户 lock。相关配置见
`reference/configuration/tools.md`；请求语法见 `reference/tool-requests/guide.md`。
