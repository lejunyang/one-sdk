# mise dev-tools 后端与 `osdk` 对等性研究

日期：2026-08-28

上游快照：[`jdx/mise@6a13eb5d3f760eed673f34e182acb418c21dca6a`](https://github.com/jdx/mise/commit/6a13eb5d3f760eed673f34e182acb418c21dca6a)，提交于 2026-08-27

`osdk` 基线快照：`one-sdk@bd00af2`，包含声明式
锁定制品重放。当前状态说明还涵盖了随本报告一同交付的动态
选项身份实现。

## 2026-08-15 审计的状态

[`sdk-manager-audit-2026-08-15.md`](./sdk-manager-audit-2026-08-15.md) 是一份有用的**整改前历史快照**，并非当前仓库的描述。尤其是，
在 2026-08-16 至 2026-08-28 期间的工作落地后，其中的后端清单、功能差距表、测试数量、缺失测试的论断以及 Sigstore/Rekor 限制均已过时。

旧报告应继续保留，作为实现历史记录。当前规划和对等性声明应改用本报告、已完成的 [`remaining-gaps-roadmap-2026-08-16.md`](./remaining-gaps-roadmap-2026-08-16.md) 以及当前源码树。

## 执行摘要

当前 mise 带来的主要启示，并不只是它支持更多具名工具。它的优势源于在一套工具语法背后组合了四层能力：

1. 形如 `backend:package[options]@selector` 的规范化动态身份；
2. 覆盖解析、安装、激活、执行、卸载和锁定的共享生命周期；
3. 从 HTTP 元数据到原生包管理器及 Lua 插件的后端专用适配器；以及
4. 感知选项的缓存和锁，避免将两个实质不同的安装视为同一制品。

在所固定的修订版本中，mise 可识别 19 种固定后端类型：`core`、`npm`、`pipx`、`cargo`、`gem`、`go`、`dotnet`、`spm`、`aqua`、`github`、`gitlab`、`forgejo`、`http`、`s3`、`conda`、`pkgx`、`asdf`、`vfox`，以及已弃用的 `ubi`。
它还支持动态命名的 vfox 后端插件。`Unknown` 在源码中作为解析器哨兵存在，并非可用后端。不存在原生 `composer:` 后端；Composer 必须通过其他后端或插件获取。

`osdk` 已具备扎实的生命周期深度：install/use/uninstall、本地和远程列表、current/where、outdated/upgrade、一次性执行、激活、shim、别名、信任、跨平台锁、离线重放、源探测与故障切换、原生包管理器缓存、BLAKE3 内容寻址存储，以及异常严格的制品验证。
其 13 个固定后端覆盖主要运行时和包管理器，而动态 `npm:<package>` 与 `github:<owner>/<repo>` 支持也已相当完善。已交付的 Phase 0 身份工作为这两个命名空间提供了规范 `b3-v2:` 身份、`.osdk-install.json` schema 1 记录、指纹化根、同版本身份共存，以及精确且失败关闭的 lifecycle 选择。

最大的战略差距是**由完整稳定动态身份支撑的生态广度**，而非基础下载机制。
`osdk` 目前还无法表达 mise 的大多数包命名空间，其声明式插件也有意比
mise 的 Lua 工具/后端插件窄得多。现有 npm 和 GitHub 动态安装现在会在逐安装记录中
验证规范身份并使用指纹化物理根，但统一的规范解析器，以及通用的缓存、锁指纹、
秘密和物理共存规则仍未完成。凡是会改变所选制品、安装布局、依赖图或
可执行文件集合的后端选项，最终都必须在所有这些边界上一致地参与身份判定。

因此，推荐的实现顺序是：

1. 完成规范化动态身份和选项指纹（Phase 0 已针对 `npm:` 和 `github:`
   完成部分实现）；
2. 内联通用 `http:` 后端；
3. `cargo:` 和 Go module `go:` 开发工具后端；
4. 优先使用 `uv` 的 `pipx:`；
5. 原生消费 Aqua registry 并进行验证；
6. 扩展现有 GitHub 后端；
7. 安全且带版本的插件边界；以及
8. 在所有动态后端间实现命令、锁、缓存和并发对等。

这一顺序可复用 `osdk` 最强的基础能力，同时避免形成一批身份和重放行为互不兼容的一次性后端。

## 范围与证据模型

本报告涵盖 mise 的 **dev-tools 后端子系统**，以及正确使用它所需的相邻语义：工具规范、配置作用域、install/use/exec 行为、锁文件、缓存、registry、URL 重写和身份认证。报告不把 mise 环境管理器或任务运行器的所有功能都视为 SDK 管理器的强制范围，但如果这些相邻能力会造成实际行为差异，则会明确指出。

以下上游参考资料分为两类：

- 官方文档 URL 描述受支持的用户可见行为，预计会持续演进。
- 固定的 GitHub 提交是用于判定分类和测试数量问题的可复现实现快照。特别是，
  [`BackendType`](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs#L16-L38) 和 [`BackendType::guess`](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs#L58-L82) 是固定前缀列表的权威来源。

当前 `osdk` 评估包含提交 `bd00af2`。从锁恢复的
声明式工具现在会先使用锁定的 URL、文件名、校验和与归档形态，
然后才查询当前插件模板。

本研究无需进行真实的公开安装。测试数量是 Rust `#[test]` 和 `#[tokio::test]` 注解的结构性计数，并非声称在这项仅文档任务中执行了这些测试套件。

## 1. 当前 mise 后端模型

### 1.1 分类

[后端概览](https://mise.jdx.dev/dev-tools/backends/)和[后端架构](https://mise.jdx.dev/dev-tools/backend_architecture.html)从概念上对系统进行了分组，而固定的枚举给出了完整的固定集合。

| 类别 | 后端 | 角色 | 重要边界 |
| --- | --- | --- | --- |
| 原生核心运行时 | `core:<tool>` | 面向 Node、Python、Go、Java、Ruby、Rust、Swift、Zig、Bun、Deno、Erlang、Elixir 和 .NET 等高频运行时的 Rust 实现 | 尽管后端索引将核心工具单独记录，`core` 仍是一个后端类型。行为和锁定保真度因工具而异。 |
| 语言包生态 | `npm:`、`pipx:`、`cargo:`、`gem:`、`go:`、`dotnet:`、`spm:` | 将包/模块转换为隔离的可执行工具 | 这些后端通常把安装委托给语言原生命令，因此版本锁定并不总是意味着制品 URL/校验和也被锁定。 |
| Registry/通用二进制系统 | `aqua:`、`pkgx:`、已弃用的 `ubi:` | 使用精选 registry 元数据或发布约定来安装二进制文件 | Aqua 是首选的精选路径；`pkgx` 尚处实验阶段；`ubi` 仍可运行，但已弃用并推荐改用 `github:`。 |
| 发布和直接来源后端 | `github:`、`gitlab:`、`forgejo:`、`http:`、`s3:`、`conda:` | 解析并安装发布制品、直接 URL/对象或包归档 | 这些后端最有机会在 `mise.lock` 中提供平台 URL、大小、校验和与来源信息。 |
| 插件系统 | `asdf:`、`vfox:`、`<plugin-name>:` | 运行旧式 shell 插件、现代 Lua 工具插件或多工具后端插件 | asdf 属于旧式方案，通常不支持 Windows。私有/自定义插件优先使用 vfox。动态后端前缀由插件定义。 |

将该列表与 `osdk` 比较时，有三个区别尤其重要：

- `node` 这样的裸 mise 工具通常通过简写 registry 解析为 `core:node`；用户不一定能看到前缀。
- `go:github.com/...` 这样的命名空间后端表示“安装一个 Go 命令”，而裸 `go` 表示 Go 运行时。`osdk` 当前只实现了后一种含义。
- 运行时能力与 registry 准入策略是两回事。尽管 mise 不鼓励甚至拒绝这些路径的新公开 registry 条目，显式 asdf、vfox 和 ubi 引用仍然可用。

官方后端索引不够完备，不能作为机器可读的分类：它遗漏了 `core`，而自动生成的 `mise backends ls` 示例也曾滞后于较新的枚举变体。应以固定的源码枚举作为兼容性判定标准。

### 1.2 工具身份与选择器语法

通用的显式形式为：

```text
backend:package[option=value,...]@selector
```

示例包括：

```text
npm:prettier@3
github:BurntSushi/ripgrep[matching=musl]@14.1.1
http:my-tool[url=https://example.test/tool-{{version}}.tar.gz]@1.2.3
cargo:https://github.com/acme/demo@rev:0123456789abcdef
```

后端/包部分是持久身份，而不只是解析装饰。Registry 简写和 `[tool_alias]` 可以把友好键映射到该身份，动态后端插件则提供自己的前缀。内置兼容性规范化包括 `nodejs -> node`、`golang -> go` 和 `dotnet-core -> dotnet`；它们并不能取代通用别名 registry。

选择器分为以下类别：

| 选择器 | 语义 |
| --- | --- |
| 精确值 | 具体发布版本，例如 `20.11.1`。精确发布版本优先于隐式前缀解释。 |
| 模糊值 | `20` 或 `20.11` 等值会选择匹配版本线中的最新版本。`mise use` 默认保存模糊意图；`--pin` 保存解析后的值。 |
| `latest` 与别名 | 选择后端最新的稳定版本，或 `lts` 之类的工具/后端别名。 |
| `prefix:<value>` | 即使存在拼写相同的精确发布版本，也强制进行递归前缀匹配。 |
| `ref:<ref>` | 在支持的位置选择 VCS ref。 |
| `path:<path>` | 使用现有外部安装，而不下载。 |
| `sub-<partial>:<selector>` | 解析内部选择器，减去数值版本组件，然后解析所得前缀。 |
| `tag:`、`branch:`、`rev:` | 后端专用 VCS 选择器，特别用于 Cargo 和 SPM。完整提交 ID 是可复现形式。 |
| `system` | 在支持的位置选择 `PATH` 上不受管理的可执行文件。 |

选项具有语义意义。`github:owner/repo[matching=client]@1` 和同一发布版本的 `matching=server` 可能产生不同的二进制文件。
Cargo feature、pipx extra、Aqua var、HTTP 提取规则、平台覆盖和包管理器选择，也可能在版本字符串不变的情况下改变字节或布局。因此，正确的实现至少需要在以下位置纳入选项指纹：

- 安装身份，或经过验证的逐安装清单；
- 当选项会影响解析时，远程版本和派生元数据缓存键；
- 锁条目；以及
- 复用检查。

这是安全地将 `osdk` 扩展到当前两个动态命名空间之外的架构前提。

### 1.3 后端选择

Mise 通常通过其内置 registry 解析简短工具名称，并选择第一个启用的后端。用户可以强制指定显式前缀、定义 `[tool_alias]`、禁用后端类别，或使用 `MISE_BACKENDS_<TOOL>` 覆盖。尽管较旧的架构说明暗示显式前缀优先，但当前解析器把环境覆盖放在最高优先级。

默认情况下，registry 快照随发布版本一同内置，因此已安装的 mise 二进制会针对稳定映射接受测试。`registry_floating=true` 可选择使用当前远程 mise/Aqua registry。快照与浮动之间的选择对 `osdk` 很重要：集中维护的简写 registry 应默认版本化且可复现，而不应在每次解析时隐式获取。

## 2. 命令、配置、锁、缓存与来源语义

### 2.1 命令边界

| 命令 | mise 契约 | 对实现对等性的影响 |
| --- | --- | --- |
| [`mise install`](https://mise.jdx.dev/cli/install.html) | 安装显式请求或所有已配置工具。它不会选择/激活工具，也不会重写配置。显式的一次性版本不会更新项目锁状态；配置驱动的安装会维护已启用或现有的锁。默认并行安装。 | 保持安装、选择和锁变更相互分离。 |
| [`mise use`](https://mise.jdx.dev/cli/use.html) | 批量安装一个或多个工具，并写入期望的选择器。目标可以是项目默认配置、`--global`、`--env` 或 `--path`；`--pin` 和 `--fuzzy` 控制持久化精度。 | 仅支持单工具的接口是实质性差距，尤其会影响原子化项目设置。 |
| [`mise exec`](https://mise.jdx.dev/cli/exec.html) | 使用完整解析后的项目环境及可选的临时工具覆盖来运行命令。它不会改变配置或父 shell。当前参数可对环境、网络、读取和写入进行沙箱隔离。 | `exec` 应在未显式指定工具时也能工作，保留子进程退出状态，并把沙箱视为执行问题而非后端问题。 |
| [`mise lock`](https://mise.jdx.dev/dev-tools/mise-lock.html) | 只解析并写入，不执行安装。它支持选定工具、跨平台目标、全局/本地作用域、旧格式升级、dry-run、选择器升级和 JSON 变更输出。 | 锁生成必须能独立自动化，且不能有隐藏的安装副作用。 |

相关区别同样有用：`uninstall` 删除已安装版本而不编辑配置；`unuse` 编辑配置，并可能清理不再被引用的安装；`where` 返回安装根目录；`which` 返回具体的活动可执行文件；`prune` 删除未引用版本；`reshim` 重建启动器。

Mise 没有一种统一命名为“lazy install”的模式。按需安装通过 `exec`、任务和 command-not-found 集成提供。仅仅进入某个目录不一定会安装所有缺失工具。

### 2.2 全局与项目配置

Mise 会按层级合并配置，而不是只选择一个项目文件。用户全局配置提供默认值，沿祖先目录向上遍历时累积配置，距离更近的文件覆盖更远的文件。系统配置可提供组织级默认值。在同一目录中，本地变体和环境专用变体按文档规定的优先级参与合并。

重要的作用域规则如下：

- `mise use` 通常写入最高优先级目录中优先级最低的普通配置（一般为 `mise.toml`），即使 `mise.local.toml` 也存在。
- `--global`、`--path` 和 `--env` 显式选择其他写入目标。
- `MISE_ENV=test` 会激活 `mise.test.toml` 等文件；本地变体拥有各自的锁文件。
- `[tools]`、`[env]` 和 `[settings]` 以加法方式合并，较近位置的冲突项胜出。较近位置的任务定义会替换同名的父级任务。
- 工具版本和选项可以插值使用源自配置变量和环境/来源指令的值。
- `.tool-versions` 和可选的惯用版本文件属于兼容性输入，并不构成完整配置模型。

当前官方[配置参考](https://mise.jdx.dev/configuration.html)和[环境专用配置参考](https://mise.jdx.dev/configuration/environments.html)是权威来源。

### 2.3 锁文件

Mise 的锁文件在创建时需要选择启用，但一旦存在便会持续维护：

- `lockfile=true` 允许配置驱动的 `use`/`install`/`upgrade` 创建和维护锁。
- 如果未设置该选项，现有锁文件仍会被维护，但不会隐式创建新锁文件。
- 普通 `mise lock` 以活动项目根目录为目标。除非使用 `mise lock --global`，否则不包括全局配置。本地和环境专用配置会映射到相应的锁文件，如 `mise.local.lock` 和 `mise.test.lock`。
- `mise lock --platform linux-x64,macos-arm64,windows-x64` 可以填充其他平台条目，而不安装它们。
- `mise lock --bump` 在不安装、不更改配置的情况下推进模糊选择器。`--dry-run --json` 面向依赖更新自动化而设计。
- 版本 1 记录精确版本、规范后端、原始说明符、影响制品的选项，以及在支持时记录平台专用 URL/校验和/大小。
- 严格模式仅要求能够产生 URL 的后端记录 URL。外部安装器后端会被跳过，因此“已锁定”并不代表所有后端都具有相同的制品可复现性。

命令状态矩阵很简洁：

| 操作 | 安装 | 写入配置 | 写入锁 |
| --- | ---: | ---: | ---: |
| `use` | 是 | 是 | 锁适用时是 |
| 配置驱动的 `install` | 是 | 否 | 锁适用时是 |
| 显式一次性 `install tool@version` | 是 | 否 | 否 |
| `upgrade` | 是 | 使用 `--bump`/不兼容显式目标时有时会写入 | 是 |
| `lock` / `lock --bump` | 否 | 通常不写入 | 是 |

中央锁文档的后端支持摘要滞后于一些较新的实现。例如，固定源码显示 Conda 和 pkgx 记录的包/制品信息比摘要表所述更丰富。发生分歧时，应以专用后端文档和固定源码为准。

### 2.4 缓存与来源

Mise 与 `osdk` 解决的是不同的来源选择问题。Mise 通常从一个逻辑上游 URL 出发，然后叠加 registry、共享元数据服务、URL 替换和后端原生镜像。`osdk` 则显式建模多个候选来源并对其测速。这两种方式可以共存，不应混为一谈。

当前 mise 行为包括：

- **mise-versions：**公开 GitHub 版本列表、发布元数据和证明通常通过 [`mise-versions.jdx.dev`](https://mise-versions.jdx.dev) 提供，以减少未经身份认证的 API 调用。
  GitHub token 对回退、私有仓库和企业主机仍然重要。参见 [GitHub token](https://mise.jdx.dev/dev-tools/github-tokens.html)。
- **每日远程缓存：**工具版本列表、别名、惯用文件名、发现的 bin 路径和安装后执行环境会缓存在 `MISE_CACHE_DIR` 下；远程版本默认每天刷新。缓存键包含后端/工具/选项；若行为依赖解析后的配置，则还包含选项/环境上下文，而不是只用一个全局列表。
- **Aqua registry：**mise 内嵌经过测试的 Aqua registry 快照。有序的自定义 registry URL 可以置于其前；可选的浮动模式会检查当前上游数据。远程自定义 registry 数据有单独的 TTL（默认为一周），编译缓存名称包含 registry 内容哈希。
- **URL 替换：**[`url_replacements`](https://mise.jdx.dev/url-replacements.html) 对传出 URL 执行首次匹配的有序字符串或正则表达式重写，其中包括 Conda 元数据/制品和插件 HTTP。这是路由层，而非镜像速度选择。
- **凭据注意事项：**为原始 URL 生成的认证请求头会在 URL 替换后保留。因此，替换到不受信任的主机可能泄露 GitHub/GitLab/Forgejo 或其他凭据。必须使用锚定模式和受信任的代理端点。重写后会应用目标主机的 `.netrc` 凭据，并覆盖默认 authorization 请求头。
- **缓存生命周期：**[`mise cache clear`](https://mise.jdx.dev/cli/cache/clear.html) 清除所有或选定的工具缓存；`mise cache prune` 删除旧条目。[缓存行为参考](https://mise.jdx.dev/cache-behavior.html)指出，许多条目大约一天后就会陈旧，并且在 CI 中缓存整个目录往往是一种浪费。
- **HTTP 例外：**普通 `http:` 安装使用 `MISE_DATA_DIR/http-tarballs` 下的内容寻址提取存储，而非 `MISE_CACHE_DIR`，因为实际安装中的符号链接指向它。`mise cache clear` 有意不破坏这些安装。

## 3. 逐后端矩阵

“锁”列会区分可复现的制品元数据与仅记录精确解析版本的情况。

| 后端 | 身份与安装 | 依赖/来源行为 | 锁与安全态势 | 当前最接近的 `osdk` 覆盖 |
| --- | --- | --- | --- | --- |
| `core` | `core:<tool>` 或 registry 简写；热门运行时的原生 Rust 实现 | 工具专用的官方元数据和归档 | 因工具而异：有些记录 URL/校验和/来源；委托的 .NET/Rust/Swift 路径不受严格 URL 强制约束 | 强大的专用 Node、Go、Python、Java、Rust、Deno、Bun、npm、pnpm、Yarn 后端；固定的 Maven/Gradle/Kotlin 后端 |
| `npm` | `npm:<package>`；默认使用直接 HTTP 元数据和内嵌 Aube；也可选择 Aube CLI、npm、pnpm 或 Bun | 默认元数据/安装不需要 Node；安装后的工具或脚本可能需要。遵循 npm registry/认证配置 | 精确版本锁，而非可移植的制品 URL；Aube 默认拒绝依赖构建 | 最强的动态对等实现：Aube/原生模式、三种作用域、紧凑锁元数据、Node 绑定、经过验证的 bin |
| `pipx` | 来自 PyPI、GitHub/Git 或 HTTP 的 Python CLI 包；优先使用 `uv tool install`，回退到 pipx | 需要 uv 或 pipx；支持 extra、registry 覆盖、安装环境和 Git ref | 仅版本锁；已安装环境取决于 Python 解析器和索引状态 | 缺少动态后端 |
| `cargo` | crates.io crate 或 Git URL；优先使用 cargo-binstall | **仅**当 cargo-binstall 退出码为 94 时回退到 `cargo install`；feature/default-feature 可强制从源码构建；Git 支持 tag/branch/rev | 仅版本锁；源码构建依赖 Cargo 生态状态 | 缺少动态后端；已有受管理的 Rust/Cargo 运行时 |
| `gem` | 通过 `gem install` 安装 `gem:<name>` | 需要 Ruby/Gem；Ruby 变更后可能需要重新安装 | 仅版本，无可强制执行的制品 URL | 缺失 |
| `go` | 通过 `go install` 安装 `go:<module/path>` | 需要 Go；强制使用隔离的 `GOBIN`；支持安装环境和 build tag | 仅版本；支持伪版本 | 缺少包后端；不要与强大的裸 Go 运行时后端混淆 |
| `dotnet` | 通过 `dotnet tool install` 安装 `dotnet:<NuGet-tool>` | 需要 .NET 运行时/SDK；预发布版本需选择启用 | 没有文档所述的完整制品锁 | 缺少包后端；当前也没有专用 .NET 运行时 |
| `spm` | GitHub/GitLab 简写或 Git URL | 优先使用匹配的 Swift artifact bundle，否则通过 SwiftPM 构建；`rev:`/`ref:` 强制从源码构建 | 锁定保真度会因使用发布 bundle 还是源码构建而异 | 缺失 |
| `aqua` | 通过内置/自定义 registry 元数据解析 `aqua:<owner>/<repo>` | 无需 Aqua CLI；使用预构建制品；支持 registry 模板/变量；任意环境设置能力有限 | 强校验和，加上可选 GitHub 证明、Cosign、SLSA 和 Minisign；解析后提供完整锁元数据 | 缺失；现有 pipeline 和 Sigstore/Rekor 基础能力可复用 |
| `github` | `github:<owner>/<repo>` 发布制品 | 统一平台匹配器；显式 pattern、收窄 filter、版本前缀、多个补充制品、rename/bin/bin-path、企业 API | 完整 URL/校验和/大小锁；GitHub 证明和来源 | 基础实现良好，但选项范围、provider/认证行为、补充制品与缓存身份仍较窄 |
| `gitlab` | `gitlab:<namespace>/<repo>` 发布制品 | 共享发布匹配器；GitLab/自托管 token 和 API 配置 | 完整 URL/校验和/大小；无 GitHub 风格证明路径 | 缺失 |
| `forgejo` | `forgejo:<owner>/<repo>`，默认使用 Codeberg | 共享发布匹配器；自托管 API 和凭据 | 在可用时提供完整制品身份；无来源证明 | 缺失 |
| `http` | `http:<logical-name>[url=...]@version` | 内联 URL/平台模板；可选远程版本列表，可按纯文本、正则表达式、JSON path 或表达式解析；支持归档和裸文件 | 完整 URL/校验和/大小；可选校验和 URL/表达式；内容寻址提取缓存 | 声明式 TOML 插件覆盖安全子集，但没有内联动态 `http:` 身份，也没有可比的元数据/提取选项范围 |
| `s3` | `s3:<logical-name>[url=s3://...]@version` | AWS 凭据链、自定义 endpoint/region、manifest 或对象列表发现 | URL/校验和/大小选项；实现可以记录制品 | 缺失 |
| `conda` | `conda:<package>[channel=...]` | 直接使用 Anaconda API/包提取；无需 conda 可执行文件；隔离的 `CONDA_PREFIX`；仅限单包 CLI 用途 | 当前源码记录所选包以及依赖身份/制品元数据，超出中央摘要所述范围 | 缺失 |
| `pkgx` | `pkgx:<pantry-project>` | 实验性原生 pantry 解析器、bottle 下载器、运行时 wrapper、npm 风格 range | 主 bottle 和传递依赖 bottle 的 URL/校验和均可锁定 | 缺失 |
| `asdf` | `asdf:<plugin>` / `asdf:<owner>/<plugin>` | 执行旧式 shell hook；通常仅限 Unix；可运行任意代码并依赖外部组件 | 可以记录精确版本，但没有强制的制品 URL/来源契约 | 声明式插件更安全但表达能力弱得多；没有 asdf 兼容运行时 |
| `vfox` 工具插件 | `vfox:<owner>/<plugin>` | 内嵌 Lua hook 系统，带 HTTP/JSON/archive/semver 模块和跨平台执行 | 工具插件可报告 URL、滚动校验和与证明 | 没有对等的可执行/沙箱化插件 API |
| 动态后端插件 | `<installed-plugin-name>:<tool>` | 一个 Lua 插件通过 list/install/exec-env hook 管理多个工具 | 后端插件目前无法报告工具插件可提供的全部 URL/来源数据 | 没有对等实现；registry 仅动态构造 `npm:` 和 `github:` |
| `ubi` | `ubi:<owner>/<repo>` 或直接 URL | 内嵌发布制品启发式规则 | 已弃用；部分校验和/大小锁；应迁移到 `github:` | 现有 `github:` 才是目标类比对象，不应以此为理由添加 `ubi:` |

固定的 `BackendType` 中**不存在原生 `composer:` 后端**。Composer 可执行文件可通过 HTTP/GitHub/Aqua/插件路径交付，PHP 工具也可使用插件或其他生态专用路径，但调用方不得假定 `composer:<package>` 存在。

### 后端专用官方参考资料

- [npm](https://mise.jdx.dev/dev-tools/backends/npm.html)、[pipx](https://mise.jdx.dev/dev-tools/backends/pipx.html)、[Cargo](https://mise.jdx.dev/dev-tools/backends/cargo.html)、[Gem](https://mise.jdx.dev/dev-tools/backends/gem.html)、[Go](https://mise.jdx.dev/dev-tools/backends/go.html)、[.NET](https://mise.jdx.dev/dev-tools/backends/dotnet.html)、[SPM](https://mise.jdx.dev/dev-tools/backends/spm.html)
- [Aqua](https://mise.jdx.dev/dev-tools/backends/aqua.html)、[GitHub](https://mise.jdx.dev/dev-tools/backends/github.html)、[GitLab](https://mise.jdx.dev/dev-tools/backends/gitlab.html)、[Forgejo](https://mise.jdx.dev/dev-tools/backends/forgejo.html)、[HTTP](https://mise.jdx.dev/dev-tools/backends/http.html)、[S3](https://mise.jdx.dev/dev-tools/backends/s3.html)、[Conda](https://mise.jdx.dev/dev-tools/backends/conda.html)、[pkgx](https://mise.jdx.dev/dev-tools/backends/pkgx.html)
- [asdf](https://mise.jdx.dev/dev-tools/backends/asdf.html)、[vfox](https://mise.jdx.dev/dev-tools/backends/vfox.html)、[后端插件开发](https://mise.jdx.dev/backend-plugin-development.html)、[已弃用的 ubi](https://mise.jdx.dev/dev-tools/backends/ubi.html)

## 4. 当前 `osdk` 的对等程度与差距

### 4.1 已经具备的优势

`crates/osdk-cli/src/cli.rs` 中当前的命令界面和 `commands.rs` 中的处理器覆盖了基本工具生命周期：

- `install`，包括无参数的配置/锁安装；
- `use`，支持项目级和全局持久化；
- `uninstall`、`list`、`list-remote`、`current`、`where` 和 `reshim`；
- `outdated` 和 `upgrade`；
- `exec --tool ... -- command`；
- shell 激活/停用和补全；
- alias、trust、source、registry、cache、prune 和 doctor 命令；以及
- 专用的 Node、Python、Rust 和模型工作流。

`crates/osdk-core/src/backend/mod.rs` 中的核心后端 trait 规模恰当：规范 ID 与别名、默认来源/探测 URL、版本列举/解析、安装/卸载、已安装项清单、bin 路径/名称、运行时环境和惯用文件。这为动态包后端提供了良好基础。

以下当前差异化优势值得保留：

- **来源选择：**多个显式的官方/镜像/自定义候选项、并发吞吐量/TTFB 探测、TTL 缓存、确定性回退、逐工具固定，以及不暴露秘密的候选项指纹（`source/mod.rs`、`source/select.rs`）。
- **离线和制品重放：**以 URL 为键的元数据缓存、可续传/可故障切换下载、持久化校验和，以及从锁恢复的制品计划。
- **存储：**逐文件 BLAKE3 CAS，通过硬链接/reflink/复制进行物化，包含 manifest、receipt 和垃圾回收（`store/mod.rs`、`store/link.rs`）。
- **验证：**校验和/SRI、签名校验和 manifest、可选 GitHub Sigstore 证据，以及 Rekor SET/checkpoint/inclusion-proof 验证。
- **信任：**能够影响执行或来源的项目配置会与内容绑定，且必须受到信任。
- **动态 npm 安全性：**Aube 路径默认拒绝构建脚本，保留选项，验证 bin/manifest，并以 journal 记录全局发布/回滚。

### 4.2 当前后端清单

`crates/osdk-core/src/backend/registry.rs` 中的 `Registry::new()` 注册 13 个固定 ID。`Registry::load()` 添加声明式 TOML 定义。`Registry::get()` 仅动态创建 `github:` 和 `npm:` 后端。

| `osdk` 后端 | 当前行为 | 相比 mise 的主要限制 |
| --- | --- | --- |
| `node` / `nodejs` | 多来源 Node 下载、LTS 元数据、架构选项、校验和验证、可选 Corepack、项目元数据和惯用文件 | 跨架构解析可以锁定，但安装会拒绝非主机架构；核心工具元数据约定更窄 |
| `go` / `golang` | Go SDK 发布版本、镜像、SHA-256、`GOROOT`、惯用文件 | 没有独立的 `go:<module>` 动态命令后端 |
| `python` / `py` / `cpython` | 内嵌且可选择刷新的 PBS catalog；CPython、PyPy、GraalPy、Pyodide、variant、预发布版本、校验和 | 没有 `pipx:`/PyPI 工具隔离或通用 virtualenv 工作流 |
| `java` / `jdk` / `openjdk` | Foojay 兼容 catalog、发行版和 JDK/JRE 选项、校验和、`JAVA_HOME` | 默认只有一个配置的 catalog endpoint/source；未完整解析 `.sdkmanrc` |
| `maven`、`gradle`、`kotlin` | 经过验证的归档安装 | 每个工具只有一个硬编码版本；没有真实远程索引 |
| `rust` / `rustup` | 隔离的 rustup/Cargo home、component、target、检查/修复、override、链接的 toolchain | 委托路径位于归档 CAS 之外；stable/beta/nightly 仍会浮动；未实现通用 `system` 语义 |
| `npm` | 独立管理的 npm CLI、SRI、受管理的 Node launcher 和缓存 | 运行时仍需要受管理的 Node |
| `pnpm` | 独立平台包、SRI、感知版本的 store 变量 | 显式的受支持平台矩阵 |
| `yarn` | Classic 和 Berry 元数据/安装、SRI、生成的 Node launcher | 需要受管理的 Node；没有更广泛的 Corepack 生态 |
| `deno` | npm packument/平台包、SRI、`DENO_DIR` | 没有 musl Linux 包路径 |
| `bun` | npm packument/平台包、SRI、glibc/musl 选择 | 仅专用运行时 |
| `npm:<package>` | 内嵌 Aube 和原生 npm/pnpm 模式；隔离/项目/全局作用域；依赖图元数据；带 `b3-v2:` 身份与 bin 校验的 `.osdk-install.json` schema 1 | 唯一完整开发的语言包命名空间；lock schema 3 存储公开选项和紧凑的原生锁身份，而非完整依赖图；osdk 自有同版本身份使用不同指纹化根，项目管理 npm 保持独立 |
| `github:<owner>/<repo>` | API、Atom 和公开页面回退；平台评分；静态 catalog；归档/二进制安装；校验和/minisign/证明；`.osdk-install.json` 绑定 asset/layout/material 身份 | 相比 mise，可移植 asset 控制和 credential/provider 路径更少；没有 GitLab/Forgejo 同类后端；同版本身份变体可在指纹化根中共存 |
| 声明式 TOML | 安全的静态/逐行列表归档定义，支持校验和、模板、bin 和惯用文件 | 没有内联命名空间、裸二进制文件、JSON/正则表达式/表达式版本解析、env、依赖、转换、hook 或插件生命周期 |

提交 `bd00af2` 增加了在渲染当前模板之前执行锁定制品重放的能力，
并包含一个离线回归测试，弥补了这一特定的声明式后端可复现性差距。

### 4.3 命令差异

| 领域 | 当前 `osdk` | 实质性的 mise 差距 |
| --- | --- | --- |
| `install` | 显式安装或解析配置；无参数/无选项路径会使用匹配的平台锁 | 显式选项会绕过锁；没有自动锁文件模式 |
| `use` | 每次调用一个工具；支持项目级或 `--global`；npm 拥有丰富的项目/全局事务 | 不支持批量 use、`--env` 或任意 `--path` 目标 |
| `exec` | 要求提供一个或多个 `--tool`，安装这些工具，组合 PATH/env，再运行命令 | 无法仅使用配置中的项目环境运行；没有 read/write/network/env 沙箱参数；子进程的非零状态会变成通用错误，而非原样保留 |
| `lock` | 精确且感知平台的 `osdk.lock`；包含制品和 npm 元数据 | 通用命令没有 `--platform`、`--bump`、`--json`、`--dry-run`、`--global` 或配置环境锁选择。锁定动态 npm 工具可能会安装受管理的 Node 并准备原生依赖图，因此不像 `mise lock` 那样是普遍的仅解析操作。 |
| 配置 | `config path/list`；通过专用命令修改 | 没有通用 settings/config set/unset，也无法检查生效的文件顺序 |
| 缓存 | 显示 env 并清理下载；CAS prune 单独存在 | 没有按 category/key/age 清理、统计或完整的缓存生命周期 UI |
| 产品相邻能力 | 专用 SDK/模型工作流 | 没有任务运行器、watch 模式、通用 `[env]`、dotenv/模板、hook、secret 或任务作用域工具 |

### 4.4 全局与项目解析差异

`osdk` 当前依次应用 CLI 参数、环境变量、最近的 `osdk.toml`/`.osdk.toml`、用户配置和默认值。活动解析还可回退到 `.tool-versions`、后端惯用文件、Node 项目元数据和全局工具 pin。

与 mise 相比，它缺少：

- 合并祖先项目配置；
- 系统配置层；
- `local` 和具名环境 profile；
- include 文件和通用模板求值；
- 为一个工具选择多个版本；
- 通用 `[env]`、任务、hook 以及路径/来源指令；以及
- 可用的通用 `path:`/`system` 选择模型。

仅采用最近项目的行为在 `find_project_config` 中清晰可见：向上搜索会在首次匹配时返回。一些嵌套设置也会被替换，而非深度合并。这足以满足当前产品需求，但在采用 mise 兼容配置语法前应明确说明。

### 4.5 锁差异

`osdk.lock` 的 lock schema 3 在多个方面很强：按平台（包括 musl）划分工具，存储精确请求/版本/选项，可附加制品 URL/名称/校验和/子目录/证据，存储紧凑的 npm 安装器/作用域/原生锁身份，并保留模型 manifest。写入时会验证大小/schema/路径安全，通过临时文件发布、执行同步并原子替换。

Lock schema 3 与 `.osdk-install.json` schema 1 彼此独立。锁持久化公开选项和后端重放
metadata；安装记录在嵌套 `identity` 中持久化 `tool`、`version`、`platform`、`scope`、
`material_options`、`dependencies`、`materials` 与规范 `b3-v2:` `install_id`。该身份选择物理
根，并约束本地复用和所有 lifecycle 操作。`.osdk-tool.json` schema 1 或 2 只用于遗留识别，
绝不能授权复用或执行。下文提到的 schema-2 npm graph sidecar 指较旧的锁兼容格式，不是动态
安装身份格式。

剩余差距是语义上的，而不仅是序列化问题：

- 通用锁创建/维护不是一种可配置策略；
- 显式 install 参数或 `-o` 会绕过无参数锁路径；
- 锁定动态 npm 工具可能安装受管理的 Node 并物化依赖图，而不能保持无副作用；
- 单次 lock 命令无法填充任意平台矩阵；
- 没有仅升级选择器或 JSON diff 工作流；
- 模型会被记录，但无参数工具安装不会恢复它们；
- Rust channel 不是不可变发布身份；
- 完整的现有安装会在不重新哈希所有已安装内容的情况下复用；
- 项目锁的 read-modify-write 会原子发布，但并未在竞争进程之间串行化；以及
- 紧凑 npm 元数据依赖单独持久化的原生锁/缓存，才能从完全冷态离线重建。

### 4.6 来源与缓存差异

在可用性和延迟方面，`osdk` 的来源模型比 mise 的 URL 重写模型更强：它理解候选项身份，探测多个来源，基于不暴露秘密的来源列表指纹缓存排序，并将失败的候选项保留为后续下载回退。固定是优先性的，而非严格的，因为回退仍然启用。

当前的重要限制包括：

- 来源候选项指纹覆盖来源配置；npm/GitHub
  安装复用现在有 manifest 约束的选项身份契约，但工具
  选项仍未在各后端间形成通用的远程元数据/缓存身份；
- `--refresh-sources` 并未被 lock、outdated 或 list-remote 统一使用；
- registry 探测是独立的 npm 专用子系统，而非通用包 registry 抽象；
- 元数据、来源探测、制品、原生管理器、模型和 CAS 存储使用不同的生命周期控制；以及
- `cache clean` 仅清理下载，而 CLI 不提供按类别检查、大小报告或按时间清理。

采用 mise 风格的 `url_replacements` 应与来源排序分开考虑。如果添加，`osdk` 应默认剥离跨来源重写中的原始凭据，并要求显式的可信转发策略。直接照搬 mise 保留请求头的行为会削弱 `osdk` 现有的 `forward_credentials` 边界。

## 5. 测试审计

### 5.1 当前 `osdk` 覆盖

在已提交的 `HEAD` 上，仓库整个 workspace 包含 539 个 Rust 测试注解：

| 区域 | 注解数量 | 所证明的能力 |
| --- | ---: | --- |
| `osdk-core` | 340 | 版本/配置解析、来源选择、HTTP 失败、pipeline、提取、CAS、验证、后端、模型、信任、inventory |
| `osdk-cli` 单元测试 | 103 | 锁 schema/验证、配置编辑、命令规划、全局 npm 事务内部逻辑 |
| `osdk-cli/tests/isolated_cli.rs` | 67 | 隔离 home/state 下的真实子进程行为，包括锁、信任、exec、registry 路由、npm 项目/全局流程和回滚 |
| `osdk-shim` 单元测试 | 3 | shim 本地辅助逻辑 |
| `osdk-shim/tests/shim_contract.rs` | 26 | 参数/stdin/stdout/stderr/status、递归、动态 inventory、registry 路由、别名和跨平台 wrapper 行为 |

这些数字直接取代了旧审计中“62 个内联测试”和“没有 CLI 集成/后端契约/shim 运行时测试”的快照。当前高价值覆盖包括：

- 使用临时 `HOME` 和 `OSDK_*` 状态的真实 CLI 子进程套件；
- 离线锁解析、跨架构锁选择、锁消费、制品篡改拒绝和证据重新验证；
- 动态 npm 项目/全局安装、原生锁格式、回滚、崩溃恢复、bin 所有权、registry 选择，以及离线/已认证情况下修改前失败的用例；
- HTTP 403/429/5xx/timeout/malformed/stale/offline 行为和来源候选缓存失效；
- 中断下载续传和失败安装清理；
- 真实注册后端安装/卸载锁定的 fixture 归档；
- 隔离的 Rust 生命周期覆盖；
- Unix shim 子进程契约；以及
- 原生 Windows CI，加上 Wine 下完整的 Windows GNU workspace 套件。

共享契约有一点需要说明。`all_builtin_backend_ids_satisfy_the_lifecycle_contract` 使用合成后端遍历各个 ID。第二个测试会调用真实注册后端，针对锁定 fixture 执行安装、bin 发现、marker/receipt 创建和卸载。
它并未通过普通的远程 list/resolve/source 行为运行每一个真实后端。动态 npm 和声明式插件也尚未纳入统一的端到端公共矩阵。

### 5.2 固定 mise 后端测试注解

以下数字是 `6a13eb5d` 上各 `src/backend/<name>.rs` 文件中 `#[test]` 和 `#[tokio::test]` 注解的直接计数。它们是导航信号，而非质量评分：辅助逻辑、集成套件、snapshot 和共享 matcher 测试可能位于其他位置，一个注解也可能覆盖许多用例。

| 后端文件 | 测试注解 | 后端文件 | 测试注解 |
| --- | ---: | --- | ---: |
| `npm.rs` | 70 | `aqua.rs` | 91 |
| `github.rs` | 34 | `pipx.rs` | 27 |
| `spm.rs` | 26 | `http.rs` | 21 |
| `cargo.rs` | 19 | `go.rs` | 18 |
| `asdf.rs` | 6 | `vfox.rs` | 5 |
| `gem.rs` | 3 | `dotnet.rs` | 3 |
| `ubi.rs` | 1 |  |  |

其他固定文件还包含 11 个 Conda、8 个 S3 和 5 个 pkgx 注解。规模大得多的 npm/Aqua 套件证明，实现后端对等性需要细致的策略和失败测试，而不仅是实现一个 trait。

### 5.3 `osdk` 剩余测试差距

1. **真实后端契约广度。** 每个已注册后端都应针对本地 fixture 元数据运行其真实的 list/resolve/locked-install/bin/execute/uninstall 路径。动态 `github:`、动态 `npm:` 和声明式插件需要显式矩阵行。
2. **动态身份广度。** 聚焦的 npm/GitHub 覆盖现已证明规范 `b3-v2:` 身份、指纹化根共存
   与精确 lifecycle 选择，但仍需通用矩阵证明每个未来命名空间都有等价的 cache/lock 行为。
3. **来源命令集成。** `source add/list/test/pin/unpin/remove` 缺少完整的子进程往返、重启持久化、别名规范化、无效写入回滚和双进程变更测试。
4. **缓存保证。** `cache clean` 测试了下载删除，但没有测试逐字节保留 CAS、元数据、探测缓存、原生管理器缓存、模型快照和安装。
5. **跨进程写入者。** 锁和全局 npm 代码具备原子写入与强大的进程内事务测试，但项目 `lock/use/upgrade` 竞态需要独立生成的竞争进程和更新丢失检测。
6. **其余 shim 行为。** Unix 信号转发没有聚焦的回归用例。
7. **声明式重放。** 聚焦的锁重放测试已随 `bd00af2` 落地；
   声明式后端仍应加入共享生命周期契约。

## 6. 推荐的分阶段路线图

以下每个阶段都可独立交付，并应作为一个聚焦的实现系列完成。后续后端依赖阶段 0 建立的身份和重放不变量。

### Phase 0 — 规范化动态身份与选项指纹（已部分实现）

为 `backend:package[options]@selector` 引入统一的解析表示，并在 CLI 解析、配置别名、registry 查找、安装路径、inventory、缓存键、锁、来源配置和 shim 解析中使用它。将选项分类为影响解析、影响制品、影响布局、仅影响执行或秘密；只持久化安全的身份投影。

不要把原始版本作为唯一复用键。后端必须在其物理安装身份中使用选项指纹，或者在复用前通过 manifest 验证完整的选项指纹。规范化规则必须有意处理区分大小写的生态，而不能把 npm 的小写规则全局应用。

当前实现已针对现有 osdk 自有 `npm:` 和 `github:` 命名空间完成物理安装身份分支：

- 它对白名单内的安全公开身份选项进行规范化，在安装前拒绝未知的
  公开选项，并排除内部 `__osdk_*` 重放元数据；
- 它从 `tool`、精确 `version`、`platform`、`scope`、规范 `material_options`、
  `dependencies` 与 `materials` 计算与顺序无关、带域分隔的 BLAKE3 `b3-v2:` 身份；
- `.osdk-install.json` schema 1 在嵌套 `identity` 中记录上述值及 `install_id`，并由该指纹
  选择物理根；
- 相同 backend/version 的身份变体可以共存，而复用、activation、shim 执行、`where`、
  uninstall 与 `reshim` 只选择配置的精确身份；以及
- 旧 `.osdk-tool.json` schema 1 和 2 只作为遗留状态识别，绝不能授权复用或执行。项目管理
  的 npm 保持独立。

这有意没有完成整个阶段。CLI/config/alias 共用的统一解析身份、感知选项的远程元数据和缓存键、
显式的秘密/keyed-hash 策略、超越持久化公开选项的指纹化锁身份，
以及向 npm/GitHub 之外推广，仍未完成。

完整阶段验收标准（尚未全部满足）：

- 一个解析器可无歧义地往返处理无作用域和有作用域的 npm 名称、包含 `@` 的 URL、内联选项和所有受支持选择器。
- `backend/package/version/options` 在 config、inventory、lock 和 cache 代码中共用一种规范序列化形式。
- 同一版本的两组影响制品的选项永远不会静默复用或覆盖彼此。
- 调整等价选项的顺序会生成相同指纹；秘密变更会改变 keyed hash，但不会把秘密写入磁盘。
- 别名保留用户的配置键，但解析到同一个规范运行时后端身份。
- 不安全的路径组件、未知动态前缀、重复别名和冲突身份在发生修改前失败。
- 现有 `npm:`/`github:` 配置、inventory 和 schema-3 锁能够迁移，或以可操作的兼容性错误失败。

### Phase 1 — 内联 `http:` 后端

将安全归档 pipeline 提升为内联 `http:<name>` 后端。在内部复用声明式定义，但增加一等动态身份、裸文件安装、平台 URL map、校验和/大小、校验和 URL、归档格式覆盖、strip/bin/rename/bin-path 控制，以及通过纯文本、正则表达式和受限 JSON path 提取远程版本。若尚无安全的内嵌语言，表达式求值可以稍后实现。

验收标准：

- 精确版本的静态 URL 在缓存预热并生成锁后，可以完全离线工作。
- 为 version/OS/architecture 定义 URL 模板，并拒绝未知或不安全的展开。
- 裸可执行文件和受支持归档均能在 Linux、macOS、Windows 和 Wine 测试 fixture 上安装。
- 平台专用 URL/校验和/大小/格式选项包含在身份指纹和锁中。
- 跨平台锁生成会获取元数据/校验和，但绝不下载目标制品。
- redirect、timeout、404/429/5xx、truncation、resume、checksum mismatch、archive traversal、symlink escape 和 ambiguous bin selection 均会安全失败。
- 锁定的离线重放不依赖后来对配置/模板的更改。

### Phase 2 — `cargo:` 和 Go module `go:` 后端

同时添加这两个编译型包生态，因为二者都依赖选定的 compiler/runtime、隔离的输出根目录、源码构建日志和感知选项的复用。保持它们的命名空间与裸 `rust` 和裸 `go` 运行时相互独立。

Cargo 验收标准：

- 支持 registry crate 以及带 `tag:`、`branch:` 和 `rev:` 选择器的 Git URL。
- 在适用时优先使用 cargo-binstall，并且**仅**当退出码为 94 时回退到 `cargo install`。其他失败原样向上传播。
- `features`、`default-features`、`bin`、`crate` 和 `locked` 会影响身份和复用。
- 为可复现的 Git 安装持久化完整提交修订；明确将浮动 branch 标记为不可复现。
- 安装使用隔离的 target/root，且不修改用户的 `CARGO_HOME`。

Go 验收标准：

- 支持 module/cmd 路径、精确语义版本和伪版本、build tag 以及安装环境。
- 强制将 `GOBIN` 指向分阶段安装的 bin 目录，并验证预期可执行文件仍位于其中。
- 配置后使用选定的受管理 Go 运行时，除非显式选择，否则不泄漏到用户全局 module/build cache。
- 精确 module 版本、tag、相关环境策略和 Go 运行时身份参与复用/锁验证。

对这两个后端，本地伪 registry/proxy fixture 必须覆盖依赖失败、编译失败、取消、并发安装、清理和卸载。

### Phase 3 — 优先使用 `uv` 的 `pipx:`

实现隔离的 Python CLI 工具，优先使用受管理/系统的 `uv tool install`，仅根据显式能力规则回退到 pipx。支持 PyPI 名称、Git URL/ref、GitHub 简写、直接归档、extra、包名覆盖、registry URL 和安装器参数。

验收标准：

- 当 uv 可用时，解析器和安装器选择 uv；仅当 uv 不可用或被显式禁用时才使用 pipx。
- 所使用的 Python 运行时和安装器记录在安装 manifest 中，并在复用前验证。
- extra、index URL、VCS commit、包名和安装器模式均纳入指纹。
- 每个工具都有隔离环境，并且只暴露经过验证的 entry point。
- 更新选定的 Python 会使依赖工具失效，或有意重建它们。
- 私有索引凭据绝不进入锁文件、指纹、日志或不相关进程可见的子进程参数。
- 除非已有完整锁定的 wheel/sdist graph，否则离线行为会在修改前失败；不得根据仅版本锁宣称可从冷态离线复现。

### Phase 4 — 原生 Aqua 后端

消费版本化的内置 Aqua registry 快照，无需 Aqua CLI。增加有序自定义 registry 和显式浮动 registry 模式，并使用按内容哈希的编译缓存。复用 `osdk` 验证基础能力，而非调用外部验证器。

验收标准：

- 内置 registry 已固定版本、记录来源，并且可以离线使用。
- 自定义本地/HTTPS registry 按顺序求值，内置 registry 则作为显式的回退策略。
- Registry 来源/内容/选项参与编译缓存键；其中任意一项改变都会使缓存的包元数据失效。
- 平台模板、包别名、files/bin 过滤、required/default var 和预发布策略与 fixture 预期一致。
- 始终验证校验和；受支持的 Minisign、Cosign、SLSA 和 GitHub 证明声明按策略失败关闭。
- 跨平台锁条目记录 URL/校验和/大小；记录前会验证当前平台来源。
- 不向不受信任的替换项/镜像转发任何 registry 或制品凭据。

### Phase 5 — 先扩展 `github:`，再添加同类 provider

基于当前 GitHub 实现继续构建，而不引入已弃用的 `ubi:`。增加共享发布制品 matcher 和其余可移植规则：`matching`、`matching_regex`、`asset_pattern`、补充制品、`version_prefix`、平台专用设置、`size`、`bin`、多项 rename、过滤后的 bin、`bin_path`、`no_app`、prerelease、企业 API URL，以及逐工具证明控制。

验收标准：

- 自动检测拥有 fixture 覆盖：OS/arch/libc 同义词、debug/source/checksum 制品、裸可执行文件、归档、macOS app，以及 `.phar` 等平台无关制品。
- 显式 pattern 或收窄 filter 有文档化的优先级，且必须精确选出一个主制品。
- 补充制品有确定顺序、逐个锁定/验证，且无法覆盖分阶段根目录之外的内容。
- 从一个仓库中选择不同二进制文件的两个别名拥有不同的安装身份。
- Token 查找按主机限定作用域；除非显式信任，否则第三方镜像绝不会收到来源凭据。
- 锁重放不调用 GitHub API，并重新验证缓存的证据；匿名 rate-limit 回退继续接受测试。
- 共享 matcher 足够与 provider 无关，以供后续 `gitlab:` 和 `forgejo:` 适配器使用。

### Phase 6 — 安全插件边界

在复制 asdf 兼容性之前，定义版本化的 capability API。优先选择跨平台的沙箱化/WASM 或能力严格受限的内嵌运行时；如果选择 Lua/vfox 兼容性，则公开结构化 HTTP、JSON、archive、semver、logging 和 process API，而非不受限制的 ambient execution。区分单工具插件和多工具后端插件。

验收标准：

- 插件声明 API 版本、后端前缀、capability、依赖、配置 schema 和完整性元数据。
- 插件可以列出版本、提出安装计划、声明 bin/env 和卸载，而不能绕过路径、网络、凭据、校验和或锁策略。
- 除非已声明并受信任，否则拒绝访问网络主机、生成可执行进程、文件系统根目录、环境和转发凭据。
- 插件 install/update/remove/link/list 操作是事务性的，并保留版本化来源信息。
- 动态插件前缀不能遮蔽内置前缀或其他动态插件前缀。
- 插件代码/配置更改会使相关选项/缓存指纹失效。
- 后端插件可以报告足够的不可变制品身份以支持严格锁；否则锁会显式记录仅版本/不可复现状态。
- Windows、Unix、malformed-result、timeout、crash、cancellation 和 malicious-path fixture 均为必需项。

不要把 Bash/asdf 兼容性作为主要扩展边界。若为迁移而添加，应标为 legacy、要求显式信任，并将其与现代契约隔离。

### Phase 7 — 生命周期、锁和缓存对等

当若干动态命名空间共享该抽象后，应补齐横切语义，而非添加更多一次性后端。

验收标准：

- `use` 接受批量输入，并原子提交 config/lock 状态；支持项目、全局、环境和显式路径目标。
- `exec -- command` 可在不强制使用 `--tool` 的情况下，从解析后的项目配置运行；支持可选覆盖、精确保留子进程退出码，并提供文档化的沙箱控制。
- 锁策略支持自动维护、现有锁粘性、`--platform`、`--global`、本地/环境作用域、`--bump`、`--dry-run` 和 JSON 变更。
- 所有 lock/cache/install 复用路径均匹配规范后端身份以及影响制品的选项指纹。
- 项目配置和锁写入者使用跨进程序列化；在必须共同提交多个文件时使用恢复 journal。
- `system` 和 `path:` 是真实的外部工具选择，且具有显式的不可复现语义。
- 缓存命令可报告大小，并按 category、backend、key 和 age 清除/清理，而不破坏由 CAS 或 HTTP 支撑的实际安装。
- 真实后端契约包含每个固定和动态后端。添加后端时，如果未注册 fixture lifecycle、offline、option identity、lock 和 Windows 用例，CI 必须失败。
- Linux 开发通过 `./scripts/windows-wine-tests.sh` 运行完整的 Windows GNU workspace 套件；原生 Windows 继续留在 CI 中。所有测试都使用临时 `HOME`、`OSDK_*`、`CARGO_HOME`、`RUSTUP_HOME` 和构建目录。
- 每项用户可见变更都在同一个功能提交中更新两份 README，以及成对的中英文 VitePress 页面/导航。

### 平台稳固后再扩展的范围

阶段 7 之后，根据已证明的需求添加后端：`gitlab:`、`forgejo:`、`s3:`、`conda:`、`pkgx:`、`gem:`、`dotnet:` 和 `spm:`。除非迁移需求足以证明合理性，否则不要添加已弃用的 `ubi:` 兼容后端。不要为了 mise 对等性而虚构原生 `composer:` 后端，因为 mise 本身也没有；应通过 HTTP/GitHub/Aqua/插件元数据交付 Composer，或者将其设计为有独立依据的生态后端。

## 7. 发布级完成定义

只有满足以下全部条件，一个阶段才算完成：

1. **身份：** 规范请求、后端、解析版本、平台和影响制品的选项在重启前后均明确且稳定。
2. **隔离：** 测试和安装器不能读取或修改真实的用户管理器状态。
3. **可复现性：** 锁准确说明离线时能复现和不能复现的内容。仅版本的委托锁绝不能描述为制品锁。
4. **安全性：** 凭据、选项秘密和替换 URL 有显式转发边界；不安全路径和歧义制品会在修改前失败。
5. **事务：** 失败、取消和并发操作只会留下旧的有效状态或完整的新状态，绝不会留下混合状态。
6. **生命周期：** 使用真实后端覆盖 list、resolve、install、discover/execute binary、uninstall、reinstall 和 offline 路径。
7. **平台：** Linux、macOS、原生 Windows 和 Wine 下 Windows 的覆盖符合后端声称的支持范围。不受支持的目标会在解析时失败，而非发生部分修改后才失败。
8. **文档：** 用户工作流在中英文中保持一致；实现细节放在成对的 VitePress 实现页面中。
9. **可观测性：** 错误会指出后端、规范工具身份、选择器、来源/provider、阶段和安全的修复方式，而不记录秘密。
10. **兼容性：** 现有 `osdk.toml`、`.tool-versions`、inventory、安装和锁要么继续工作，要么产生经过测试且可操作的迁移错误。

## 8. 历史审计核对

2026-08-15 审计中的以下当前状态论断已被明确取代：

| 历史论断 | 2026-08-28 的当前状态 |
| --- | --- |
| 九个内置后端；npm 仅为捆绑提供 | 13 个固定后端、独立 npm、动态 `npm:<package>` 和 `github:`，外加声明式 TOML 插件 |
| Bun/Deno/Python 依赖其中所述的 GitHub API | Bun/Deno 使用 npm packument/平台包；Python 使用内嵌/经验证的 PBS catalog；GitHub API 行为具备回退和缓存 |
| 来源配置未获一致遵循 | 来源处理已修复；npm registry 选择是一个独立且经过测试的子系统 |
| 缺少 job、prompt、resume、Rust 隔离/卸载和校验和策略 | 这些能力已实现并经过测试 |
| 没有锁文件、离线模式、outdated/upgrade、exec、alias/completion、trust 或并发安装 | 这些能力均已实现 |
| 其中列出的 Node/Python/Java/Rust/Deno/Bun/GitHub 差距仍未完成 | 大多数已在 8 月 16 日路线图中完成；当前差距范围更窄，并已在上文记录 |
| 未验证 Rekor inclusion proof 和 SET | 当前验证会检查 SET、checkpoint 和 Merkle inclusion proof |
| 62 个内联测试；没有 CLI/shim/后端契约或 Windows 运行时测试 | 539 个 Rust 测试注解，包括 67 个 CLI 子进程注解和 26 个 shim 集成注解、后端契约、网络失败矩阵、原生 Windows 和 Wine 执行 |

旧文档中带日期的上游修订表、论证和整改历史仍然有价值。未经重新验证，不得把其中以现在时描述的清单和差距措辞复制到新的规划文档。

## 9. 官方来源索引

核心参考资料：

- [后端](https://mise.jdx.dev/dev-tools/backends/)
- [后端架构](https://mise.jdx.dev/dev-tools/backend_architecture.html)
- [Dev tools 与工具选项](https://mise.jdx.dev/dev-tools/)
- [配置](https://mise.jdx.dev/configuration.html)
- [`mise install`](https://mise.jdx.dev/cli/install.html)
- [`mise use`](https://mise.jdx.dev/cli/use.html)
- [`mise exec`](https://mise.jdx.dev/cli/exec.html)
- [`mise lock`](https://mise.jdx.dev/dev-tools/mise-lock.html)
- [GitHub token 与 mise-versions 行为](https://mise.jdx.dev/dev-tools/github-tokens.html)
- [URL 替换与凭据警告](https://mise.jdx.dev/url-replacements.html)
- [缓存行为](https://mise.jdx.dev/cache-behavior.html)
- [固定的后端类型源码](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs)
- [固定的后端源码目录](https://github.com/jdx/mise/tree/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend)

本地 `osdk` 证据入口：

- `crates/osdk-core/src/backend/registry.rs` — 固定和动态 registry 行为
- `crates/osdk-core/src/backend/mod.rs` — 共享后端契约
- `crates/osdk-core/src/version/mod.rs` 和 `version/resolver.rs` — 选择器与项目解析
- `crates/osdk-core/src/source/mod.rs` 和 `source/select.rs` — 来源候选项和排序
- `crates/osdk-core/src/store/` 和 `pipeline/` — CAS、下载、提取、receipt 和验证
- `crates/osdk-core/src/inventory.rs` — 动态已安装工具身份
- `crates/osdk-cli/src/cli.rs`、`commands.rs` 和 `lockfile.rs` — 命令与锁语义
- `crates/osdk-cli/tests/isolated_cli.rs` 和 `crates/osdk-shim/tests/shim_contract.rs` — 子进程契约
- `scripts/windows-wine-tests.sh` 和 `scripts/windows-runtime-smoke.ps1` — Windows 运行时验证

