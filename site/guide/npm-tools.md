# npm 开发工具

osdk 通过 `npm:<package>` 命名空间使用 npm Registry 中发布的命令行包。
`osdk use` 会识别上下文：它可以把工具加入真实 Node 项目、安装为用户级全局工具，
也可以在没有 Node 项目时保留原有的 osdk 隔离安装。

::: tip 先区分两个名字
`npm@11.5.2` 安装的是 npm 包管理器；`npm:prettier@3` 选择的是 npm Registry 中的
Prettier 包。若要选择 Registry 中名字就叫 `npm` 的包，请写 `npm:npm`。
:::

## `use` 如何选择作用域

| 命令与上下文 | 安装目标 | 配置与 lock |
| --- | --- | --- |
| 在 `package.json` 下执行 `osdk use npm:prettier@3` | 最近的真实 Node 项目 | `osdk.toml`、`osdk.lock`、原生 package lock，以及本地筛选命令 generation |
| `osdk use --global npm:prettier@3` | osdk 控制的全局前缀 | 用户 `config.toml` 和用户 `osdk.lock`；忽略当前项目 |
| 祖先目录中没有 `package.json` 时执行本地 `use` | 原有 osdk 隔离安装 | 普通项目 pin，以及 osdk 自有合成项目和 shim |

最近的普通 `package.json` 是硬项目边界。较近 manifest 是软链接或格式异常时，osdk
不会跳过它并落到外层项目。显式 `osdk install npm:...` 与
`osdk exec --tool npm:...` 继续使用隔离的受管工具流程；修改真实项目只发生在本地
`use`。

## 将工具加入 Node 项目

可在项目根目录下的任意子目录执行 `use`，然后启用 Shell 激活：

```bash
cd my-app/packages/web/src
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .
```

osdk 会先解析并安装受管 Node，再把 Prettier 加入最近的项目。如果包已经位于
`dependencies`、`devDependencies`、`optionalDependencies` 或 `peerDependencies`，它会
留在原区段；新包默认加入 `devDependencies`。若一个包同时是 peer dependency 和开发
依赖，这两个角色都会保留。项目添加始终禁用 lifecycle scripts。

安装器返回后，osdk 会校验已安装包的名称与精确版本、包声明的 `bin` 目标，以及
`node_modules/.bin` 中对应的 launcher。没有可执行命令、包身份不符或路径逃出包目录时
都会 fail closed。

随后，osdk 只把已配置包声明并通过校验的命令发布到不可变的自有 generation：

```text
.osdk/npm-bin/generations/<sha256>/bin/
```

其中的 launcher 仍执行 `node_modules/<package>` 下包实际声明的目标；包管理器生成的完整
`node_modules/.bin` 只用于安装后校验，osdk 从不把它加入 PATH。继续配置 npm 工具时，
新 generation 只保留配置 spec 仍与项目完全匹配的选择；命令名冲突会 fail closed。

该操作还会写入或更新类似下面的结构化项目选择：

```toml
[tools]
node = "22.17.0"
"npm:prettier" = { version = "3", installer = "aube" }
```

精确受管 Node 版本与具体安装器会一起记录；已有 Node 工具项的其他选项会保留。由于
激活可能暴露项目代码，osdk 会自动信任这次生成的 `osdk.toml` 精确内容；之后编辑文件
会改变其信任身份，需要重新审阅并执行 `osdk trust`。

## 安装器选择

自动选择以 Aube 为先：没有原生 lock，或现有 lock 格式能被内嵌 Aube 读取时，osdk
都会选择 Aube。目前兼容 Aube v9、pnpm v9，以及 npm
`package-lock.json` / `npm-shrinkwrap.json` v2 或 v3。若已知 npm 或 pnpm lock 版本过新
或 Aube 尚不支持，osdk 会只调用一次拥有该 lock 的原生包管理器。

`package.json#packageManager`（其次是 `devEngines.packageManager`）用于识别声明的
owner。声明与已有原生 lock 必须一致；同时存在多个可识别 lockfile 也会因歧义被拒绝。
自动模式只接受声明的 Aube、npm 或 pnpm；其他管理器需要先明确选择支持的安装器。

需要时可显式选择安装器：

```bash
osdk use npm:prettier@3 -o installer=aube
osdk use npm:prettier@3 -o installer=npm
osdk use npm:prettier@3 -o installer=pnpm
```

显式选择可以覆盖 package-manager 声明，但仍须与当前 lock 兼容：Aube 不会读取不支持
的格式，npm 或 pnpm 也不会覆盖另一原生管理器的 lock。安装器在修改项目前就已确定，
且最多只执行一次；Aube/npm/pnpm 失败会直接返回，osdk 不会换一个安装器重放操作。

## 项目激活与信任边界

项目配置含 npm 工具且已经信任时，Shell hook 只会把当前筛选后的
`.osdk/npm-bin/generations/<sha256>/bin` 加到 PATH 前面，绝不会加入项目原始的
`node_modules/.bin`。激活过程只读，且必须同时通过以下检查：

- 已信任的 `osdk.toml` 与最近的普通 `package.json` 属于同一项目根；
- `.osdk/npm-bin/current` 是普通且有效的 JSON 指针，其 generation schema、平台、内容
  派生 ID 与 manifest 一致；
- 所有 `.osdk/npm-bin` 自有目录都是项目内的非软链接目录，generation 只含声明的文件；
- 每个已发布包及其 spec 仍匹配已信任项目配置，且安装名称、精确版本、声明目标和筛选
  launcher 都重新通过校验；
- 筛选后的 generation 不提供 `node`，避免替换当前选中的受管运行时。

指针、generation 或任一目标缺失、过期、被修改或不安全时，hook 会忽略整个筛选目录。
校验通过时，筛选命令位于 osdk shim 与受管运行时路径之前。成功的 `use` 会发布
generation（新内容先通过 staging 构建），并原子替换 `current` 指针；较旧的完整
generation 可能作为可丢弃的本地状态保留。

建议在该 package 根目录的忽略文件中加入精确规则：

```text
/.osdk/npm-bin/
```

若要在仓库根统一覆盖嵌套 workspace package，请使用 `**/.osdk/npm-bin/`。不要直接忽略
整个 `.osdk/`，以便将来仍可有意提交该目录下的其他项目 metadata。

## 安装全局 npm 工具

全局作用域忽略当前项目的 manifest、声明和 lock：

```bash
# Aube 是默认的全局安装器。
osdk use --global npm:prettier@3

# 显式使用受管原生包管理器。
osdk use -g npm:eslint@9 -o installer=npm
osdk use -g 'npm:@antfu/ni@0.21.12' -o installer=pnpm
```

osdk 会安装所选 Node，并在需要时安装 npm 或 pnpm。原生 npm 执行
`install --global --prefix ...`；原生 pnpm 执行 `add --global`，并使用 osdk 控制的
global、bin 与 store 目录。Aube 使用等价的 osdk 自有合成全局项目。这些模式都不会
修改环境中的 Node 安装或当前项目；校验后的命令通过 osdk shim 发布。

所选版本和安装器写入用户配置；基本的 package、scope、Node、installer 和可选原生
lock 身份写入 `$OSDK_CONFIG_DIR/osdk.lock`。npm 全局安装不会生成依赖 lock；pnpm 的
`pnpm-lock.yaml` 与 Aube 的 `aube-lock.yaml` 保留在各自受控安装目录中。

## 包名与版本语法

```text
npm:<package>[@VERSION]
npm:@<scope>/<package>[@VERSION]
```

普通包与 scoped 包都受支持：

```bash
osdk use npm:prettier@3
osdk install 'npm:@antfu/ni@0.21.12'
osdk exec -t 'npm:@antfu/ni@0.21.12' -- ni
```

scoped 包中的第一个 `@` 属于 scope，最后一个 `@` 才分隔版本。Shell 通常不会特殊
处理它，但单引号可以避免命令被其他包装层误解。省略版本会选择最新稳定版；`3`、
`3.6` 等前缀会选择最高的匹配稳定版。

## 其他生命周期命令

```bash
# 隔离安装或单次执行；两者都不修改 package.json。
osdk install npm:prettier@3
osdk exec --tool npm:prettier@3 -- prettier --check .

# 查看当前选择和 osdk 自有的隔离/全局安装。
osdk current npm:prettier
osdk list npm:prettier
osdk where npm:prettier@3.6.2

# 删除 osdk 自有的精确安装，并重建受管 shim。
osdk --yes uninstall npm:prettier@3.6.2
osdk reshim
```

`list`、`where`、`uninstall` 与 `reshim` 操作 osdk 自有的隔离或全局安装。加入真实项目
的包仍由项目及其包管理器所有。激活命令来自筛选 generation，其中的 launcher 指向
`node_modules/<package>` 下已配置包通过校验的声明文件。
`osdk list-remote npm:prettier [FILTER]` 可列出 Registry 中的稳定版本。

## 构建脚本策略

本地项目 `use` 始终禁用 lifecycle scripts，无论使用 Aube 还是原生 npm/pnpm。隔离与
全局安装也默认禁用脚本；已经审阅的包可通过结构化工具项或单次选项放行：

```toml
[tools."npm:@scope/native-tool"]
version = "1.2.3"
installer = "aube"
allow_builds = ["@scope/native-tool", "esbuild"]
```

| 配置 | 对隔离/全局安装的效果 |
| --- | --- |
| 省略或 `false` | 禁止所有依赖的构建脚本 |
| `["pkg-a", "pkg-b"]` | 所选安装器支持 allowlist 时，只允许列出的包 |
| `true` | 允许整个依赖图执行脚本；只应在完整审阅后使用 |

单次形式是 `-o allow_builds=esbuild,sharp`。原生 npm 无法实施包级 allowlist，只接受
false 或 true；Aube 与 pnpm 支持按包放行。

## Source 与共享存储

动态 npm 版本 metadata 使用常规 npm source 选择，在 npmmirror 与 npmjs 之间选择。
它与受管原生 npm 或 pnpm 进程启动前执行的 `[registries.npm]` 预检不同。项目委托保留
文档约定的显式 Registry 优先级；全局委托则在 osdk 控制的前缀中使用隔离配置。

所有 Aube 驱动的 npm 工具会跨项目、全局作用域、包和版本共享以下 osdk 自有路径：

```text
$OSDK_CACHE_DIR/aube/v1/cache
$OSDK_STORE_DIR/aube
```

共享布局避免重复下载相同包内容，而每个真实项目或受控全局安装仍保留自己的原生
lock。

## `osdk.lock` 提供什么保证

项目感知的 `use` 会写入紧凑的 schema 3 `osdk.lock` 条目，包括 package、解析版本、
具体 installer、scope、精确 Node 版本，以及原生 lock 的 kind、format 与 SHA-256。全局
工具的用户 lock 使用相同的 metadata-only 模式；npm 全局安装没有原生 lock 身份。

::: warning 依赖图限制
`osdk.lock` 中的 metadata 本身**不会**捕获或重建 npm 传递依赖图。安装器的原生 lock
仍是依赖图来源：真实项目中的 `aube-lock.yaml`、`package-lock.json`、
`npm-shrinkwrap.json` 或 `pnpm-lock.yaml`，或者受控全局安装目录中保留的 Aube/pnpm
lock。npm 全局安装没有依赖 lock，因此只凭用户 `osdk.lock` 无法复现其传递依赖选择。
:::

项目工作流应同时提交 `package.json`、原生项目 lock、`osdk.toml` 与 `osdk.lock`。
schema 2 graph sidecar 仍可兼容读取，但当前 schema 3 不再创建新 sidecar，也不嵌入它的
payload。

安装器规划、metadata 校验、原生前缀隔离和激活安全检查见
[npm 工具实现](./implementation/npm-tools)。
