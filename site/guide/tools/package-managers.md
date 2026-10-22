# JavaScript 包管理器

osdk 把 npm、pnpm 和 Yarn 当作可独立锁定的 backend，也直接管理 Bun、Deno。
它同时为可能获取 npm 包的命令做启动前 Registry 选择，但不会重写项目 lockfile。

::: tip npm 包管理器与 npm 工具不是同一个 backend
`npm@11.5.2` 安装 npm CLI；`npm:prettier@3` 安装 npm Registry 中的 Prettier
开发工具。后者的完整生命周期、安全策略和离线行为见 [npm 开发工具](./npm)。
:::

## 安装与命令

所有 manager 使用通用生命周期命令：

```text
osdk install MANAGER[@VERSION]... [-o|--opt KEY=VALUE ...]
osdk lock [MANAGER[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk upgrade [MANAGER[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk use|u MANAGER[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool MANAGER[@VERSION])... -- COMMAND [ARG ...]
osdk list|ls [MANAGER]
osdk list-remote|lsr MANAGER [FILTER]
osdk current [MANAGER]
osdk where MANAGER[@VERSION]
osdk uninstall|rm MANAGER@VERSION
```

这些生命周期命令接受通用 `-o/--opt KEY=VALUE`，但本页列出的包管理器 backend
当前没有专用安装选项。

```bash
osdk install npm@11.5.2
osdk install pnpm@10.15.0 yarn@4.9.2
osdk install bun@latest deno@latest
```

| backend | 安装物与校验 | 暴露命令 | 自动补 Node |
| --- | --- | --- | --- |
| npm | npm registry 的 `npm` 包，SRI | `npm`、`npx` | 是 |
| pnpm | 完整 `pnpm` npm package，SRI | `pnpm`、`pnpx`；后者路由为 `pnpm dlx` | 是 |
| Yarn 1 | `yarn` 包，SRI | `yarn`、`yarnpkg` | 是 |
| Yarn 2+ | `@yarnpkg/cli-dist` 包，SRI | `yarn`、`yarnpkg` | 是 |
| Bun | `@oven/bun-*` 平台包，SRI | `bun`、`bunx`；后者路由为 `bun x` | 否 |
| Deno | `@deno/*` 平台包，SRI | `deno` | 否 |

pnpm 从完整 `pnpm` package 解析版本和安装 JavaScript distribution，并使用受管 Node
启动；它不会直接提取只含 SEA 二进制的 `@pnpm/<os>-<arch>` package。pnpm 9 的 `.cjs`
入口和新版本的 `.mjs` 入口都支持。Bun 与 Deno 则直接以当前平台实际安装的 npm
platform package 为准。

npm 和 Yarn 的 launcher 调用 `PATH` 中的 Node。只要一次请求含 `npm`、`pnpm` 或
`yarn` 而没有 Node，osdk 就自动加入 Node：先按项目规则选择版本，找不到时用
`latest`。该规则也适用于显式工具列表；manager bin 排在受管 Node bin 之前，不依赖
用户全局 Node。

Node 的 Corepack 也可能在 Node 安装目录中提供 `npm`/`npx`、`pnpm`/`pnpx` 和
`yarn`/`yarnpkg`。osdk 只把这些当作 Node 的回退路由，不把它们算成 Node 自己拥有的
命令：项目或全局配置选中了独立 manager 时，对应 shim 始终路由到独立 backend；没有
独立选择时，才使用当前 Node 安装中实际存在的 Corepack launcher。

## 包管理器版本发现

osdk 按以下优先级发现 npm、pnpm 或 Yarn：

1. 最近祖先项目配置 `[tools]` 中的 `npm`、`pnpm`、`yarn`；这里使用普通工具版本语义，`9`、`9.15` 分别选择最新稳定的 `9.x`、`9.15.x`，解析出的精确版本写入 `osdk.lock`；
2. 最近祖先 `package.json#packageManager`；
3. 同一 `package.json#devEngines.packageManager`（对象或数组第一项）。

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "pnpm@10.15.0"
}
```

`package.json` 的两种声明只支持 `npm|pnpm|yarn@精确 semver`。缺少版本、Bun/Deno、URL、路径，以及带
`#`、`+` hash/build suffix 的值都会失败；这一精确限制不适用于 `[tools]`。`packageManager` 优先于
`devEngines.packageManager`。无参数 `install`/`lock`/`upgrade` 会自动加入 manager
和 Node；`current` 也会报告对应来源。

## Registry 预检

诊断命令的完整语法：

```text
osdk registry test [MANAGER]
```

`MANAGER` 接受 `npm|npx`、`pnpm|pnpx`、`bun|bunx`、`deno`，以及
`yarn-classic|yarn1|yarn@1` 或 `yarn-berry|yarn2|yarn3|yarn4|yarn@2|yarn@3|yarn@4`。
传 `yarn|yarnpkg` 时，若项目能确定 major 就只测对应策略，否则分别测 Classic 与
Berry。省略 manager 会检查 npm、pnpm、可判断的一种或两种 Yarn、Bun、Deno。
该命令只探测，不运行安装。

```bash
osdk registry test
osdk registry test pnpm
osdk registry test yarn
```

### 自动覆盖的调用

直接 shim、shell activation 和 `osdk exec` 中的下列命令，会在 manager 启动前
评估是否需要 npm-compatible Registry：

| 程序 | 会触发预检的命令 |
| --- | --- |
| `npm` | `install`、`i`、`ci`、`add`、`update`、`up`、`exec` |
| `npx` | 有位置参数的命令，或 `--package/-p`、`--call/-c` 获取形式 |
| `pnpm` | `install`、`i`、`add`、`update`、`up`、`fetch`、`dlx`、`deploy` |
| `pnpx` | 有位置参数的命令 |
| Yarn 1/2+ | 无子命令，或 `install`、`add`、`upgrade`、`up`、`dlx`、`create` |
| `bun` | `install`、`i`、`ci`、`add`、`update`、`x` |
| `bunx` | 有位置参数的命令 |
| `deno` | `add`、`bench`、`cache`、`check`、`ci`、`compile`、`doc`、`eval`、`info`、`install`、`outdated`、`run`、`serve`、`task`、`test`、`update` |

Yarn major 无法判断时，实际执行路径不会猜测，而是直接透传。launcher/script 操作数
之后的子进程参数不会被误判为 manager 参数。

### 跳过预检的情况

以下情况保持 manager 自己的选择，不探测或覆盖：

- osdk `--offline` 禁止探测；显式进程级覆盖仍可生效；
- 命令参数显式指定 Registry；
- 对应 Registry 环境变量已经设置；
- manager 参数显式选择另一 cwd 或配置文件；
- npm/pnpm/Yarn/Bun 使用真正的 `--offline`；Deno 在支持的命令上使用
  `--cached-only`；
- `--help`、`-h`、`--version`、`-v` 等自省调用，或命令不在白名单；
- 默认 `packages = "native"` 下，原生配置声明 registry，或包含私有/scope、认证、TLS、代理或自定义配置策略。`packages = "osdk"` 的显式 `default` 仅覆盖默认源，scope 和认证仍由原生配置负责。

`--prefer-offline`、frozen lockfile 和 immutable cache 不保证断网，因此仍可能预检。
Deno 没有通用 `--offline`；`ci`、`outdated`、`update` 也不支持 `--cached-only`。

### 候选选择与单次执行

```toml
[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500
```

| manager | 选中后注入的变量 |
| --- | --- |
| npm/npx | `npm_config_registry` |
| pnpm/pnpx | `npm_config_registry` + `pnpm_config_registry` (10/11) |
| Yarn 1 | `YARN_REGISTRY` |
| Yarn 2+ | `YARN_NPM_REGISTRY_SERVER` |
| Bun | `BUN_CONFIG_REGISTRY` |
| Deno npm 层 | `NPM_CONFIG_REGISTRY` |

没有原生源策略、显式 default 或候选列表时，内置 npmmirror 与 npmjs 会并发匿名探测并选择延迟最低的健康项。
显式项目/用户列表保留顺序，选择第一个健康项；项目 `[registries]` 整段覆盖用户层。
所有候选失败时 manager 启动零次；启动后失败不会切源重跑，以免脚本、lockfile 或
`node_modules` 被执行两次。

osdk 会读取 `.npmrc`、`.yarnrc`、`.yarnrc.yml`、`bunfig.toml`/`.bunfig.toml`
以及相关用户/全局配置；原生声明即使是公共源也会保留，不参与镜像竞速。它不改项目
lockfile，也不拦截网络；metadata 或 lockfile 中的绝对 artifact URL 仍可能绕过默认
Registry。

## 原生缓存语义

这些路径保留各工具的原生格式，并不是跨 manager 的统一 tarball CAS：

| 工具 | 环境变量 | osdk 路径与含义 |
| --- | --- | --- |
| npm | `npm_config_cache` | `<cache>/pkg/npm`，npm cacache |
| pnpm ≤10 | `npm_config_store_dir` | `<cache>/pkg/pnpm-store`，pnpm content-addressable store |
| pnpm ≥11 | `pnpm_config_store_dir` | 同上，仅 store 变量名不同 |
| Yarn 1 | `YARN_CACHE_FOLDER` | `<cache>/pkg/yarn-classic` |
| Yarn 2+ | — | Berry 的 cache 由项目 `.yarn` 配置决定，osdk 不通过环境变量重定向它 |
| Bun | `BUN_INSTALL_CACHE_DIR` | `<cache>/pkg/bun` |
| Deno | `DENO_DIR` | `<cache>/pkg/deno`，还含编译产物和部分运行时状态 |

`PNPM_HOME`、`YARN_GLOBAL_FOLDER`、`BUN_INSTALL` 与 `DENO_INSTALL_ROOT` 不是缓存变量：
它们定位的是全局安装根，见下面[全局安装自动纳管](#全局安装自动纳管)。

Berry（Yarn 2+）的 cache 完全由项目 `.yarn` 配置决定，osdk 不强制改
`enableGlobalCache` 或 `cacheFolder`。

这些缓存变量通过当前 backend 的 shim/`exec` 和 shell hook 注入，并默认保留用户已有值。
`osdk cache clean` 只删除 osdk 下载归档，**不会**删除上述原生缓存。完整存储边界见
[存储、Shell 与扩展](../config/storage)。

## 全局安装自动纳管

即使不经过 `osdk use --global`，而是在激活的 shell 里直接运行各包管理器自带的全局
安装命令，osdk 也会把全局安装位置重定向到受控目录，并在**命令退出后自动对账**，让
新装或卸载的命令立即生效——不需要再手动 `osdk reshim`：

```bash
npm i -g @anthropic-ai/claude-code
pnpm add -g cowsay
yarn global add typescript     # Yarn 1；Berry 没有 global，用 yarn dlx
bun add -g prettier
deno install -A -n mytool jsr:@scope/mytool
# 命令退出即生效，直接输入命令名即可
```

| 工具 | 重定向变量 | 全局 bin 目录 | 纳管方式 |
| --- | --- | --- | --- |
| npm | `npm_config_prefix` | `data/global/npm`（bin 在其根 / `bin`） | shim |
| pnpm | `PNPM_HOME` | `data/global/pnpm/bin`（兼容散根） | shim |
| Yarn | `YARN_GLOBAL_FOLDER` | `data/global/yarn/node_modules/.bin` | shim |
| Bun | `BUN_INSTALL` | `data/global/bun/bin` | shim |
| Deno | `DENO_INSTALL_ROOT` | `data/global/deno/bin` | shim |

这些工具的全局产物是**需要运行时宿主的 launcher/脚本**（例如 pnpm 生成的 `.CMD`、
Deno 的 shell 包装），osdk 在统一的 shims 目录为每个命令生成自己的 shim；shim 会在
对应包管理器的受控环境下执行它。命令进程退出后，osdk 按该工具全局 bin 目录的内容
做一次对账：出现的新命令补 shim，消失的命令（卸载、手动删除都一样）在确认没有其他
全局来源或受管工具占用后移除。对账只看目录的实际内容，不解析命令行，因此与各工具
不断变化的安装/卸载语法、别名和参数解耦。目录未变化时通过 mtime 缓存快速跳过。

包管理器（如 Bun）可能仍按自己的习惯提示"把全局 bin 目录加入 PATH"；这一步 osdk
已经通过 shim 完成，可以忽略该提示。

如果你此前在系统里独立装过这些工具并跑过它们的 `setup`，残留的持久变量（如
`PNPM_HOME`、`BUN_INSTALL`）可能与受控值冲突；安装时 osdk 会对此告警，按提示清除
变量，或设置对应工具的 `[settings.<tool>] override_env = true`（顶层
`override_tools_env = true` 则对所有工具生效）让 osdk 强制覆盖。

### 自包含原生工具：Go 与 Cargo

`go install` 与 `cargo install` 的产物是自包含的原生二进制，不需要运行时宿主，因此
osdk 不为它们生成 shim，而是把输出目录直接放到 PATH：

| 工具 | 重定向变量 | 输出目录 |
| --- | --- | --- |
| Go | `GOBIN` | `data/global/go/bin` |
| Cargo | —（`CARGO_HOME` 已受控） | `data/cargo/bin` |

详见 [Go 工具](./go) 与 [Cargo 工具](./cargo)。

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
