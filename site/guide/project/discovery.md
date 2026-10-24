# 项目工具链

osdk 从当前目录向上查找版本声明，并兼容各生态原生的版本文件。本页说明活动版本的来源与优先级，以及在 Node 项目中使用 npm 工具的方式。配置文件的完整结构见[配置文件](../config/files)。

如果不想在项目目录里写 `osdk.toml`，可以把目录绑定为**外置项目**：配置与锁
存在 osdk 配置目录下，发现优先级与本节一致。详见[外置项目配置](./external-config)。

## 项目版本发现

活动版本的来源类型优先级如下；每一类内部再从当前目录向祖先查找：

1. `osdk.toml` / `.osdk.toml` 的 `[tools]`；
2. `.tool-versions`；
3. backend 的生态版本文件；
4. Node 的 `package.json` 元数据；
5. 用户配置的 `[tools]`。

这意味着较远祖先的 `osdk.toml` 也会胜过较近目录的 `.nvmrc`。

| backend | 生态版本文件 |
| --- | --- |
| Node.js | `.nvmrc`、`.node-version`，之后 `package.json#engines.node`、`devEngines.runtime` |
| Python | `.python-version` |
| Java | `.java-version`、`.sdkmanrc` |
| Go | `go.mod` 的 `go` 指令、`.go-version` |
| Rust | `rust-toolchain.toml`、`rust-toolchain` |
| Maven / Gradle / Kotlin | `.mvn-version`、`.gradle-version`、`.kotlin-version` |
| Bun / Deno | `.bun-version`、`.dvmrc` |

普通单值文件读取第一个非空、非注释行，并去掉开头的 `v`。Rust TOML 读取
`[toolchain].channel`。Node 的 npm semver range 会解析到最高匹配稳定版；无效 range
会明确失败。

::: warning 无参数生命周期命令的当前边界
`current`、shim 和 shell hook 会为每个 backend 读取上述生态文件。但无参数
`lock`、`upgrade` 以及没有可用 lock 时的 `install`，目前主要枚举合并后的
`[tools]`/`.tool-versions`，再额外发现 `packageManager` 和 Node。仅存在
`.python-version`、`.java-version`、`go.mod`、`rust-toolchain.toml` 等文件时，
这些非 Node 工具不会自动加入无参数生命周期命令；请在 `[tools]` 中声明或显式传参。
:::

## 在真实项目中使用 npm 工具

在 Node 项目下的任意目录执行以下命令，会把包加入最近 `package.json` 所在的项目：

```bash
osdk use npm:prettier@3
```

包会保留原有的 `dependencies`、`devDependencies`、`optionalDependencies` 或
`peerDependencies` 区段；新包默认加入 `devDependencies`。osdk 依次参考项目声明的
`packageManager`、现有 lock 的归属安装器，最后使用配置的默认值，也可以用
`-o installer=npm|pnpm` 显式指定。某个安装器失败后不会换另一个安装器重试。祖先目录中没有 `package.json` 时，该命令保留原有的 osdk 隔离安装与 shim
行为。

项目感知的 `use` 会更新原生 `package.json` 与包管理器 lock，再把精确 Node 选择和结构化
npm 条目写入 `osdk.toml`。它还会写一份紧凑的 `osdk.lock`，记录 installer、scope、Node
和原生 lock 身份。该 metadata 不包含传递依赖图；原生包管理器 lock 仍是依赖图来源。
项目中应同时保留这四类文件。安装器与激活细节见 [npm 开发工具](../tools/npm)。

为支持 Shell 激活，`use` 还会在 `.osdk/npm-bin/` 下生成本地派生状态。只有已配置 npm
工具的筛选 launcher 会被激活；整个 `node_modules/.bin` 永远不会加入 PATH。建议把
`/.osdk/npm-bin/` 加入 package 根目录的忽略文件；若仓库根包含嵌套 package，则可在
仓库根使用 `**/.osdk/npm-bin/`，同时继续提交上述四类事实来源文件。
