# JavaScript 包管理器

osdk 把 npm、pnpm 和 Yarn 当作可独立锁定的 backend，也直接管理 Bun、Deno。
它同时为可能获取 npm 包的命令做启动前 Registry 选择，但不会重写项目 lockfile。

::: tip npm 包管理器与 npm 工具不是同一个 backend
`npm@11.5.2` 安装 npm CLI；`npm:prettier@3` 安装 npm Registry 中的 Prettier
开发工具。后者的完整生命周期、安全策略和离线行为见 [npm 开发工具](./npm-tools)。
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
| pnpm | `@pnpm/<os>-<arch>` standalone 包，SRI | `pnpm`、`pnpx`；后者路由为 `pnpm dlx` | 是 |
| Yarn 1 | `yarn` 包，SRI | `yarn`、`yarnpkg` | 是 |
| Yarn 2+ | `@yarnpkg/cli-dist` 包，SRI | `yarn`、`yarnpkg` | 是 |
| Bun | `@oven/bun-*` 平台包，SRI | `bun`、`bunx`；后者路由为 `bun x` | 否 |
| Deno | `@deno/*` 平台包，SRI | `deno` | 否 |

npm 和 Yarn 的 launcher 调用 `PATH` 中的 Node。只要一次请求含 `npm`、`pnpm` 或
`yarn` 而没有 Node，osdk 就自动加入 Node：先按项目规则选择版本，找不到时用
`latest`。该规则也适用于显式工具列表；manager bin 排在受管 Node bin 之前，不依赖
用户全局 Node。

## `packageManager` 自动发现

osdk 自动发现 npm、pnpm 或 Yarn 的精确版本，优先级为：

1. 最近祖先项目配置 `[tools]` 中的 `npm`、`pnpm`、`yarn`；
2. 最近祖先 `package.json#packageManager`；
3. 同一 `package.json#devEngines.packageManager`（对象或数组第一项）。

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "pnpm@10.15.0"
}
```

只支持 `npm|pnpm|yarn@精确 semver`。缺少版本、Bun/Deno、URL、路径，以及带
`#`、`+` hash/build suffix 的值都会失败。`packageManager` 优先于
`devEngines.packageManager`。无参数 `install`/`lock`/`upgrade` 会自动加入 manager
和 Node；`current` 也会报告对应 `package.json` 来源。

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

- osdk 使用 `--offline`；
- 命令参数显式指定 Registry；
- 对应 Registry 环境变量已经设置；
- manager 参数显式选择另一 cwd 或配置文件；
- npm/pnpm/Yarn/Bun 使用真正的 `--offline`；Deno 在支持的命令上使用
  `--cached-only`；
- `--help`、`-h`、`--version`、`-v` 等自省调用，或命令不在白名单；
- 原生配置无法安全解析，或包含私有/未知/scope Registry、认证、TLS、原生代理、
  自定义配置路径等策略。

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

| manager | 选中后只注入此变量 |
| --- | --- |
| npm/npx | `npm_config_registry` |
| pnpm/pnpx | `pnpm_config_registry` |
| Yarn 1 | `YARN_REGISTRY` |
| Yarn 2+ | `YARN_NPM_REGISTRY_SERVER` |
| Bun | `BUN_CONFIG_REGISTRY` |
| Deno npm 层 | `NPM_CONFIG_REGISTRY` |

未配置列表时，内置 npmmirror 与 npmjs 会并发匿名探测并选择延迟最低的健康项。
显式项目/用户列表保留顺序，选择第一个健康项；项目 `[registries]` 整段覆盖用户层。
所有候选失败时 manager 启动零次；启动后失败不会切源重跑，以免脚本、lockfile 或
`node_modules` 被执行两次。

osdk 会读取 `.npmrc`、`.yarnrc`、`.yarnrc.yml`、`bunfig.toml`/`.bunfig.toml`
以及相关用户/全局配置，只把识别出的公共 npmjs/npmmirror 纳入候选。它不改项目
lockfile，也不拦截网络；metadata 或 lockfile 中的绝对 artifact URL 仍可能绕过默认
Registry。

## 原生缓存语义

这些路径保留各工具的原生格式，并不是跨 manager 的统一 tarball CAS：

| 工具 | 环境变量 | osdk 路径与含义 |
| --- | --- | --- |
| npm | `npm_config_cache` | `<cache>/pkg/npm`，npm cacache |
| pnpm ≤10 | `PNPM_HOME`、`npm_config_store_dir` | `<cache>/pkg/pnpm` 与 `<cache>/pkg/pnpm-store` |
| pnpm ≥11 | `PNPM_HOME`、`pnpm_config_store_dir` | 同上，仅 store 变量名不同 |
| Yarn 1 | `YARN_CACHE_FOLDER` | `<cache>/pkg/yarn-classic` |
| Yarn 2+ | `YARN_GLOBAL_FOLDER` | `<cache>/pkg/yarn` |
| Bun | `BUN_INSTALL_CACHE_DIR` | `<cache>/pkg/bun` |
| Deno | `DENO_DIR` | `<cache>/pkg/deno`，还含编译产物和部分运行时状态 |

Yarn 2/3 默认仍以项目 `.yarn/cache` 为 active cache，但默认 mirror 会使用
`${YARN_GLOBAL_FOLDER}/cache`；Yarn 4 默认启用 global cache。osdk 不强制改
`enableGlobalCache` 或 `cacheFolder`。

这些变量通过当前 backend 的 shim/`exec` 和 shell hook 注入，并默认保留用户已有值。
`osdk cache clean` 只删除 osdk 下载归档，**不会**删除上述原生缓存。完整存储边界见
[存储、Shell 与扩展](./storage-shell)。
