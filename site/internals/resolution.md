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
- `system` 是保留的版本规格；当前通用 backend 不会把它解析为 PATH 中的工具，Rust backend 目前会将其映射为 `stable`。这个版本规格写法不会启用独立的 [`tool_order` system 执行来源](../guide/config/tool-selection#system-tools)。

候选列表约定按版本升序排列。[`select_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 从尾部选择最高匹配项：`latest` 只取稳定版，range 也只取稳定版，前缀按点分隔组件匹配而不是字符串前缀匹配。逐字 pin 只做字符级比较，命中不了就解析失败。精确版本分三级匹配：先字面相等，再按 semver 核心版本比较（忽略 build metadata，`21.0.12` 可命中 `21.0.12+8`，预发布标识必须一致，同核心多 build 取最高），最后回退到点分隔组件前缀——仅在同核心版本缺失时让 `21.0.12` 命中四段式 PSU `21.0.12.1+1`，主要服务于带 build 号与 PSU 四段版本的 Java；严格三段 semver 的 backend 不会走到第三级。组件前缀匹配前会先检查「请求文本本身是否就是某个已发布版本」，命中则直接返回该候选：否则 `android-36` 会在 `android-36` 与 `android-36.1` 并存时选到后者。`select_version_with_prerelease` 为选择使用它的 backend 提供预发布策略：默认 `if-explicit`，`never` 拒绝预发布，`allow` 可让 `latest`、range 或前缀选中预发布版；逐字 pin 与带预发布标识的精确版本一样算作显式。Python、GitHub 和基于 npm package 的自定义 resolver 会显式应用该策略；通用 resolver 和部分 backend 仍使用 `select_version`，所以当前行为依 backend 而异。

## 可配置的声明优先级

[用户参考](../guide/config/tool-selection)集中维护来源枚举、默认顺序、继承、各工具适用范围和示例。`selection::candidates` 先收集符合条件的声明再排序，保留覆盖位置而不校验失效值，并检查托管声明冲突；`execution::resolve` 再加入可选外部执行目标。诊断模块复用这些收集器与 backend idiomatic 文件表，文件系统清单与选择证据明确分离。默认使用 `resolve_without_probes`，将延迟的 system 兼容性标明而不静默放宽严格策略；`--probe` 使用真实解析器及有界原生验证。收集失败时标记清单不完整，严格冲突保留候选但不报告有效 winner。

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


### 主动启用外部工具执行

`tool_order` 还接受 `system`，但必须明确列出；默认值和 `[]` 都禁用它。[系统工具参考](../guide/config/tool-selection#system-tools) 列出适配器、home 优先于 PATH、Windows 仅 `.exe` 的限制、探测及示例。外部工具是绝对执行目标，不伪装成托管版本或安装。显式 CLI 版本、托管锁重放及 frozen deps 保持权威；非 frozen deps 可使用未固定的外部工具，该策略下每次重新验证，不为外部路径写可移植 receipt。动态包工具的运行时 receipt 仍保持托管，不允许与 system 执行混用。
