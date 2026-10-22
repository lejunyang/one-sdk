# `[registries]`

## 包源与原生配置优先级

`tools` 声明工具和版本，`sources` 控制工具归档下载，`registries` 是项目包源的统一入口。
exec、shim 和 deps 共用同一组适配器，配置不会改写 `.npmrc`、`pyproject.toml` 等原生文件。

```toml
[settings.precedence]
tools = "osdk"       # 或 "native"；默认 osdk
packages = "native"  # 或 "osdk"；默认保留原生显式配置

[registries.npm]
default = "https://registry.npmjs.org/"

[registries.python]
default = "https://pypi.org/simple/"
indexes = ["https://packages.example.org/simple/"]

[registries.go]
proxy = "https://proxy.golang.org,direct"
```

- 本次命令行源参数和进程中显式源变量优先；`deps.env` 是本次子进程环境。`packages` 只决定 osdk 显式配置和原生文件之间的顺序，不对认证排序。
- `packages = "native"` 保留原生源、scope、认证、代理和 TLS 配置链；`"osdk"` 仅覆盖声明的默认源/附加索引，scope 与认证保持原生。缺少覆盖时委托原生配置；自动镜像不得覆盖显式原生配置。uv 的复杂原生表采用保守整组保留，不尝试重写命名索引或包绑定。
- npm、pnpm、Yarn Classic、Yarn Berry、Bun 使用各自变量：`npm_config_registry`、`npm_config_registry` + `pnpm_config_registry`（pnpm 10/11）、`YARN_REGISTRY`、`YARN_NPM_REGISTRY_SERVER`、`BUN_CONFIG_REGISTRY`。Yarn major 必须已知；不会把 Node 版本当作 Yarn major。pnpm 工作区配置委托 pnpm 解析。
- Python 两种 deps provider 都运行 uv。`default` 是最低优先级默认索引；`indexes` 是按声明顺序排列的显式附加索引，保留 `first-index`，不是请求失败后的镜像 fallback。命名索引、包绑定和认证插值继续放在原生配置。
- `urls` 仍是只用于默认源的匿名镜像候选；全部不可用时在启动包管理器前失败，不在安装失败后重跑生命周期脚本。离线不探测，原生安装由其离线参数约束。
- `default = ""` 清除继承的 osdk 默认覆盖并关闭该项自动镜像；`indexes = []` 清除 osdk 附加索引。两者不删除原生配置。`urls = []` 保留原来的内置候选含义。
- 用户层到项目层的 `registries` 整段替换；显式 `extends` 按字段合并、数组替换。`[deps.<provider>.registries]` 使用相同 schema，作为该 provider 的整段局部替换。Cargo、Deno 和自定义 deps 不接受此局部字段；使用原生配置和 `env`。Go 保留 GOPROXY 的逗号/竖线语义，不统一成 URL 重试列表。
- `tools` 策略比较 osdk 项目声明与原生版本声明；用户全局版本仍是后备。`.nvmrc` 等选择运行时，`package.json.packageManager` 选择管理器，两者独立。显式 `tool@version` 和现有锁重放优先；版本范围仍按范围解析，无效原生文件报错。没有 `conflict-error` 模式。
- `osdk current` 显示工具选择来源及策略；`osdk deps --dry-run --explain` 显示包源决策、发现的原生候选文件和工具选择。原生链最终赢家由包管理器解析，不能确定的来源不会被声称已解析。原生文件、源相关环境及配置参与不透明 freshness 哈希；凭据不写入显示命令或 osdk.lock。自定义命令的参数和环境在显示中整体隐藏；原生子进程自己的输出由原生工具负责脱敏。

### 迁移

删除旧 `[deps.<provider>].index` / `extra_index`，不提供兼容别名。默认源迁移到
`registries.npm.default` 或 `registries.python.default`；只有确实需要额外 Python 索引时才设置 `registries.python.indexes`。
项目局部例外可写成 `deps.uv.registries.python.default = "https://example.org/simple/"`。
未知字段正常报错，不能静默接受 Cargo/Deno 的虚构通用源字段。
