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
- `osdk current` 显示工具选择来源及策略；`osdk deps --dry-run --explain` 显示包源决策、发现的原生候选文件和工具选择。原生链最终赢家由包管理器解析，不能确定的来源不会被声称已解析。原生文件、源相关环境及配置参与不透明 freshness 哈希；凭据不写入显示命令或 osdk.lock。自定义命令的参数和环境在显示中整体隐藏；原生子进程自己的输出由原生工具负责脱敏。

### 迁移

删除旧 `[deps.<provider>].index` / `extra_index`，不提供兼容别名。默认源迁移到
`registries.npm.default` 或 `registries.python.default`；只有确实需要额外 Python 索引时才设置 `registries.python.indexes`。
项目局部例外可写成 `deps.uv.registries.python.default = "https://example.org/simple/"`。
未知字段正常报错，不能静默接受 Cargo/Deno 的虚构通用源字段。

## 包来源优先级

工具版本的 tool_order 参考见 [settings](./settings.md#可配置的声明优先级)。

`[settings.precedence]` 下的 `npm`、`python`、`go` 只覆盖对应生态的**包源策略**，不控制工具版本顺序。可取 `osdk`、`native`、`inherit`（恢复使用 `packages`）；例如 `python="native"` 保留 Python 包索引策略，不会提升 `.python-version`。这些字段不重排管理器内部的认证、scope 或 TLS 来源。uv 遵循同目录 uv.toml 优先、最近项目配置及用户/首个系统配置的候选发现；
复杂配置（命名索引、绑定及其他未建模字段）在 native 下整组保留，osdk 显式索引
实际注入并与其组合时报错；若调用环境已经覆盖对应字段而无需注入，不触发此错误。发现可能的 uv workspace 时整组委托 uv，包含成员资格与排除规则，
不注入 osdk 索引；uv 仍可能校验成员元数据。候选文件不等于逐字段 winner。含原生选项或无法读取的 requirements/constraints、`uv pip sync`/`compile` 的位置输入，以及显式配置/目录（含 `-C`）/offline/dotenv 调用参数（包括子命令之前的参数）委托 uv；`UV_NO_CONFIG/UV_OFFLINE/UV_NO_INDEX`
的 false-like 值不会单凭存在就禁用注入。不会改写原生文件或在执行失败后重装。


### system 执行来源

可选的外部工具选择与包源配置不同；参见 [settings 的 system 边界](./settings.md#system-执行来源)和 [current 来源诊断](../current/guide.md#完整来源诊断)。
