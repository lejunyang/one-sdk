# 版本解析机制

本页描述 `osdk` 如何把项目声明或命令行输入变成可安装的精确版本。这里的“版本解析”只决定工具与版本；下载源排名和安装事务分别在后续阶段完成。

## 从请求到精确版本

入口位于 [`gather_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands/install.rs)。显式参数（如 `node@20`）由 [`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 解析；没有显式工具时，CLI 汇总配置中的 `[tools]`、项目包管理器声明和 Node 项目元数据。选择 npm、pnpm、Yarn 或动态 `npm:<package>` 工具时，[`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands/install.rs) 会确保同一次操作包含 Node；若项目没有 Node 声明，则使用 `latest`。

`npm:<package>` 在通用 `tool@version` 分割前单独解析，以保留 scoped 包的
`npm:@scope/name@version` 语法。其动态 backend id 会规范为
`npm:<package>`；裸 `npm` 仍是包管理器 backend。细节见
[npm 开发工具实现](./npm-tools#身份、解析与生命周期编排)。

`cargo:` 使用同一套 URL-aware 语法解析器，并带更严格的命名空间 schema。Registry
subject 接受精确/latest/数字前缀 selector；规范 HTTPS Git subject 只接受 latest、tag、
branch 或完整小写 revision。Cargo 请求还会注入或保留且只保留一个配置/显式的精确
Rust 请求；Rust 会优先解析，其精确版本在 Cargo 继续解析前绑定到所有 Cargo 请求。
详见 [Cargo 开发工具实现](./cargo-tools#解析与精确-rust-绑定)。

`go:` 使用专用 namespace schema 校验规范 module/command path、语义或伪版本、tags 与
受限构建环境。它会注入或保留一个受管 Go 请求，先解析该 runtime，再通过已排序 Go proxy
metadata 发现最长 module root。详见 [Go 开发工具实现](./go-tools#规范身份与版本选择)。

`VersionSpec` 的语义在 [`version/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 中定义：

- 空值、`latest`、`stable`、`current` 表示最新稳定版；
- `lts`、`lts/<name>` 表示最新或指定 LTS 线；
- 完整 semver（可带前置或构建标识）是精确版本；
- 前导 `=`（如 `=android-36`）是**逐字 pin**：要求候选列表中存在字符级完全相同的版本，绝不回退到更宽松的匹配层级，也不要求文本是合法 semver。它区别于「精确版本」——后者在字面不命中时仍会按下文三级规则降级。之所以需要它，是因为某些目录的标识符本身不是版本，且在前缀匹配下互不排斥：`android-36` 与 `android-36.1` 是两个不同的 API 级别，而点分隔组件前缀匹配会让前者匹配上后者。逐字 pin 同时被视为「显式请求」，因此可在不放宽全局策略的前提下取到预发布版本；
- 不完整数字（如 `20`、`20.11`）是组件前缀；
- Node 项目元数据可产生 npm 风格 semver range，并支持 `||`；
- `system` 是保留的版本规格；当前通用 backend 不会把它解析为 PATH 中的工具，Rust backend 目前会将其映射为 `stable`。在实现真正的 unmanaged/PATH 模式前，不应把它描述为可用的安装选择。

候选列表约定按版本升序排列。[`select_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 从尾部选择最高匹配项：`latest` 只取稳定版，range 也只取稳定版，前缀按点分隔组件匹配而不是字符串前缀匹配。逐字 pin 只做字符级比较，命中不了就解析失败。精确版本分三级匹配：先字面相等，再按 semver 核心版本比较（忽略 build metadata，`21.0.12` 可命中 `21.0.12+8`，预发布标识必须一致，同核心多 build 取最高），最后回退到点分隔组件前缀——仅在同核心版本缺失时让 `21.0.12` 命中四段式 PSU `21.0.12.1+1`，主要服务于带 build 号与 PSU 四段版本的 Java；严格三段 semver 的 backend 不会走到第三级。组件前缀匹配前会先检查「请求文本本身是否就是某个已发布版本」，命中则直接返回该候选：否则 `android-36` 会在 `android-36` 与 `android-36.1` 并存时选到后者。`select_version_with_prerelease` 为选择使用它的 backend 提供预发布策略：默认 `if-explicit`，`never` 拒绝预发布，`allow` 可让 `latest`、range 或前缀选中预发布版；逐字 pin 与带预发布标识的精确版本一样算作显式。Python、GitHub 和基于 npm package 的自定义 resolver 会显式应用该策略；通用 resolver 和部分 backend 仍使用 `select_version`，所以当前行为依 backend 而异。

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

`tool_order` 是所有工具共用的一份来源类型顺序，从高到低排列，不是按工具名分别配置的表。仅适用于当前工具的来源参与选择；每个文件名取最近声明，`idiomatic` 还保留下述 backend 固定文件顺序。支持 `osdk`、`global`、
`.tool-versions`、`.nvmrc`、`.node-version`、`idiomatic`、
`package.json#packageManager`、`package.json#devEngines.packageManager`、
`package.json#engines.node`、`package.json#devEngines.runtime`。遗漏项按默认顺序补齐，
不代表禁用；未知值和重复值报错；`[]` 清除继承的自定义顺序。`idiomatic` 将其他原生文件归为一类：Python 使用 `.python-version`；Go 先 `go.mod` 后 `.go-version`，Rust 先 `rust-toolchain.toml` 后 `rust-toolchain`，Java 先 `.java-version` 后 `.sdkmanrc`。这些文件名不能独立写入 `tool_order`；不同文件名之间先按 backend 顺序，再考虑目录距离。默认顺序是项目 osdk、
.tool-versions、.nvmrc、.node-version、其他 idiomatic、packageManager、
devEngines.packageManager、engines.node、devEngines.runtime、global；`tools="native"`
把项目 osdk 移到原生项之后。自定义顺序可以明确提升 global。

项目 osdk 只使用最近配置及显式 `extends`，不隐式合并父配置。precedence 的用户、
extends、项目层按字段覆盖，数组替换；未写的策略字段继承。工具声明同类中被覆盖的
父层仅作为 shadowed 证据，不参与冲突检查，也不校验已失效的版本值；这包括已被覆盖的 `extends` 声明，证据仅保留位置，不复制原始版本值。其他工具的声明以 ignored 记录，不解释其版本或完整性值。原生管理器字段跨管理器名称选择最近身份；自动选择遇到不支持的身份会报错。`osdk current` 输出同一选择结果中的
tool、path、field、category、winner/shadowed/ignored 和 reason；结构化配置字段、JSON 字段和简单
文件行号会在已知时报告，不能确定的路径保留为空，不猜测。

`tool_conflicts="error"` 检查不同来源的有效声明：精确版本满足范围不算冲突，数字前缀、
支持的 semver 比较符及 `||` 按范围检查两两交集；配置 alias 先展开。它不把多个范围
合并成新的版本请求，最终仍选择最高优先级声明。不同的 latest/LTS/channel 或不透明选择器
无法离线证明兼容时单独报“无法验证”，不伪称已确认冲突，也不联网安装来解释。
`prefer` 选择最高项，但仍校验有效候选声明的结构。packageManager 要求精确 semver，不支持 URL、hash 或 build suffix；
devEngines.packageManager 的多选数组明确拒绝，单对象/单元素数组支持范围。
显式 `tool@version`、指定的 deps provider/installer 及锁重放保持各自权威。shim 归属维护遇到无效项目选择时保留已安装命令，但不执行它们；隐式派发仍执行项目冲突策略。内置 deps provider 只解析自身需要的工具。

`[settings.precedence]` 下的 `npm`、`python`、`go` 只覆盖对应生态的**包源策略**，不控制工具版本顺序。可取 `osdk`、`native`、`inherit`（恢复使用 `packages`）；例如 `python="native"` 保留 Python 包索引策略，不会提升 `.python-version`。这些字段不重排管理器内部的认证、scope 或 TLS 来源。uv 遵循同目录 uv.toml 优先、最近项目配置及用户/首个系统配置的候选发现；
复杂配置（命名索引、绑定及其他未建模字段）在 native 下整组保留，osdk 显式索引
实际注入并与其组合时报错；若调用环境已经覆盖对应字段而无需注入，不触发此错误。发现可能的 uv workspace 时整组委托 uv，包含成员资格与排除规则，
不注入 osdk 索引；uv 仍可能校验成员元数据。候选文件不等于逐字段 winner。含原生选项或无法读取的 requirements/constraints、`uv pip sync`/`compile` 的位置输入，以及显式配置/目录（含 `-C`）/offline/dotenv 调用参数（包括子命令之前的参数）委托 uv；`UV_NO_CONFIG/UV_OFFLINE/UV_NO_INDEX`
的 false-like 值不会单凭存在就禁用注入。不会改写原生文件或在执行失败后重装。

## Backend 解析与例外

CLI 在应用版本 alias 和一次性 backend 选项后调用 [`Backend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)。默认实现对精确版本直接返回，不请求远端列表，并保留所有 request options；非精确请求通过 `list_remote_versions` 和 `select_version` 解析。精确版本“免查列表”并不决定加密验证保证：安装阶段会应用当前 checksum/attestation 策略；在没有可用证据且 `require_checksums=false` 时仍可能继续。

部分 backend 覆盖默认算法。例如 Node 处理目标架构与 npm range，Python 处理实现、变体、catalog 和预发布策略，Java 处理发行版及 JDK/JRE，Rust 则把 channel 或版本交给隔离的 rustup。Cargo Registry 工具获取配对的 metadata/index source 数据，排除 yanked release，再解析精确/latest/数字前缀 selector；Cargo Git selector 则按原文保留。Go command 工具通过已排序 Go proxy 解析精确/latest/数字前缀/伪版本，并绑定发现的 module root。入口分别见 [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs)、[`python.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/python.rs)、[`java.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/java.rs)、[`rust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/rust.rs)、[`cargo_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/cargo_package.rs) 与 [`go_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/go_package.rs)。

## Lockfile 快路径与边界

无显式工具且无额外选项的 `osdk install` 会优先读取最近的 `osdk.lock`。[`locked_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) 按当前平台恢复已保存版本字符串、公开 options 和锁定 artifact 信息；没有当前平台区段时才回退到常规项目解析。大多数 backend 保存精确版本；Rust 的 `stable`、`beta`、`nightly` 等浮动 channel 则仍是 channel 名，后续 rustup 安装可能得到更新 toolchain。锁文件按平台保存独立结果，允许同一项目并存 Linux、macOS 和 Windows 解析。

回退到项目配置且安装成功时，`install` 会把该次精确解析写回当前平台区段；命中既有 lock
的复现路径不改写。项目级 `use` 安装后直接 upsert 目标工具及其注入 runtime，并在 lock
写入失败时回滚 `[tools]` 编辑。显式 `install TOOL...` 不改变项目声明，因此也不写 lock。

锁定的是解析结果和 artifact 身份，不是“已经可信”的声明。重新安装会重新执行当前可用或策略要求的 checksum/attestation 验证；若锁记录没有 digest/evidence 且 `require_checksums=false`，pipeline 仍可能在没有加密完整性验证的情况下安装。若显式传工具或 `-o`，不会使用 lockfile 读取快路径；后者在裸项目安装成功后仍会写入新结果。

## 可验证的不变量

- 解析优先级、父目录继承、结构化文件和非法 package-manager 值由 [`resolver` 单元测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) 覆盖。
- semver 前缀、range、LTS 与预发布策略由 [`version` 单元测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 覆盖。
- 精确版本必须保留 backend options 的回归测试在 [`backend/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)。
- CLI 的 package-manager 自动选择、Node 注入和 lock 恢复由 [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs) 与 [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) 覆盖。

执行适配器位于 `package_sources.rs`；版本策略位于 `version/resolver.rs`。原生解析的来源无法确定时只报告候选，不把文件扫描顺序当作包管理器优先级。详见[包管理器](../guide/tools/package-managers.md)。
