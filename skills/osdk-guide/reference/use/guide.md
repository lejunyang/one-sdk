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

GitHub 工具若已配置 `catalog-url` / `catalog-sha256`，`use` 会继承它们：`latest` 只取
catalog 内的最新版本，改成显式版本也不会查询在线 Releases。要恢复 GitHub 版本发现，
从对应项目或用户配置的工具条目中同时移除这两项，再运行 `use`；清缓存不能解除该限制。

隔离的全局 npm/pnpm 安装会保留 `NODE_EXTRA_CA_CERTS`，并将相对文件路径解析为绝对路径。附加信任根不会关闭 TLS 校验，也不会导入用户 npmrc。其他 TLS 覆盖设置仍受隔离检查约束；registry 探测仍必须通过可信 HTTPS 连接。

全局 `npm:` 工具（npm/pnpm 安装器）匿名安装。用户 npmrc/环境中的认证本身不再阻断公共包，
也不会导入凭据；无需删除用户登录配置。已知公共原生 registry 保留原地址。私有/未知源、
scope 路由、不可读/重定向配置、自定义 TLS/代理策略及受管 Node npmrc 的认证仍拒绝。
需要认证的包不受此修复支持；项目 `use`、`exec` 与 shim 继续遵循原生认证配置。
