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

## 可配置的声明优先级

```toml
[settings.precedence]
tools = "osdk"
packages = "native"
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "error" # 默认 prefer，按顺序选取
npm = "osdk"           # 可选：覆盖 packages；inherit 清除该生态覆盖
python = "native"
```

`tool_order` 从高到低排列来源类型，再在同类中选最近目录。支持 `osdk`、`global`、
`.tool-versions`、`.nvmrc`、`.node-version`、`idiomatic`、
`package.json#packageManager`、`package.json#devEngines.packageManager`、
`package.json#engines.node`、`package.json#devEngines.runtime`。遗漏项按默认顺序补齐，
不代表禁用；未知值和重复值报错；`[]` 清除继承的自定义顺序。`idiomatic` 保留 backend
声明的其他文件顺序（例如 Rust/Go/Python 的版本文件）。默认顺序是项目 osdk、
.tool-versions、.nvmrc、.node-version、其他 idiomatic、packageManager、
devEngines.packageManager、engines.node、devEngines.runtime、global；`tools="native"`
把项目 osdk 移到原生项之后。自定义顺序可以明确提升 global。

项目 osdk 只使用最近配置及显式 `extends`，不隐式合并父配置。precedence 的用户、
extends、项目层按字段覆盖，数组替换；未写的策略字段继承。工具声明同类中被覆盖的
父层仅作为 shadowed 证据，不参与冲突检查，也不校验已失效的版本值。其他工具的声明以 ignored 记录，不解释其版本或完整性值。原生管理器字段跨管理器名称选择最近身份；自动选择遇到不支持的身份会报错。`osdk current` 输出同一选择结果中的
tool、path、field、category、winner/shadowed/ignored 和 reason；结构化配置字段、JSON 字段和简单
文件行号会在已知时报告，不能确定的路径保留为空，不猜测。

`tool_conflicts="error"` 检查不同来源的有效声明：精确版本满足范围不算冲突，数字前缀、
支持的 semver 比较符及 `||` 按范围检查两两交集；配置 alias 先展开。它不把多个范围
合并成新的版本请求，最终仍选择最高优先级声明。不同的 latest/LTS/channel 或不透明选择器
无法离线证明兼容时单独报“无法验证”，不伪称已确认冲突，也不联网安装来解释。
`prefer` 选择最高项，但仍校验有效候选声明的结构。packageManager 的 URL/hash 仍不支持；
devEngines.packageManager 的多选数组明确拒绝，单对象/单元素数组支持范围。
显式 `tool@version`、指定的 deps provider/installer 及锁重放保持各自权威。

`npm/python/go` 只对该生态选择 osdk/native，不重排管理器内部的认证、scope 或 TLS
来源。uv 遵循同目录 uv.toml 优先、最近项目配置及用户/首个系统配置的候选发现；
复杂配置（命名索引、绑定及其他未建模字段）在 native 下整组保留，osdk 显式索引
实际注入并与其组合时报错；若调用环境已经覆盖对应字段而无需注入，不触发此错误。发现可能的 uv workspace 时整组委托 uv，包含成员资格与排除规则，
不注入 osdk 索引；uv 仍可能校验成员元数据。候选文件不等于逐字段 winner。含原生选项或无法读取的 requirements/constraints、`uv pip sync`/`compile` 的位置输入，以及显式配置/目录（含 `-C`）/offline/dotenv 调用参数（包括子命令之前的参数）委托 uv；`UV_NO_CONFIG/UV_OFFLINE/UV_NO_INDEX`
的 false-like 值不会单凭存在就禁用注入。不会改写原生文件或在执行失败后重装。
