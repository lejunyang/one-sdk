# 版本解析机制

本页描述 `osdk` 如何把项目声明或命令行输入变成可安装的精确版本。这里的“版本解析”只决定工具与版本；下载源排名和安装事务分别在后续阶段完成。

## 从请求到精确版本

入口位于 [`gather_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)。显式参数（如 `node@20`）由 [`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 解析；没有显式工具时，CLI 汇总配置中的 `[tools]`、项目包管理器声明和 Node 项目元数据。选择 npm、pnpm 或 Yarn 时，[`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) 会确保同一次操作包含 Node；若项目没有 Node 声明，则使用 `latest`。

`VersionSpec` 的语义在 [`version/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 中定义：

- 空值、`latest`、`stable`、`current` 表示最新稳定版；
- `lts`、`lts/<name>` 表示最新或指定 LTS 线；
- 完整 semver（可带前置或构建标识）是精确版本；
- 不完整数字（如 `20`、`20.11`）是组件前缀；
- Node 项目元数据可产生 npm 风格 semver range，并支持 `||`；
- `system` 是保留的版本规格；当前通用 backend 不会把它解析为 PATH 中的工具，Rust backend 目前会将其映射为 `stable`。在实现真正的 unmanaged/PATH 模式前，不应把它描述为可用的安装选择。

候选列表约定按版本升序排列。[`select_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 从尾部选择最高匹配项：`latest` 只取稳定版，range 也只取稳定版，前缀按点分隔组件匹配而不是字符串前缀匹配。`select_version_with_prerelease` 为选择使用它的 backend 提供预发布策略：默认 `if-explicit`，`never` 拒绝预发布，`allow` 可让 `latest`、range 或前缀选中预发布版。Python、GitHub 和基于 npm package 的自定义 resolver 会显式应用该策略；通用 resolver 和部分 backend 仍使用 `select_version`，所以当前行为依 backend 而异。

## 工作目录解析优先级

[`resolve_active`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) 从当前目录一路向父目录查找，但优先级按“文件类型”全局分层，而不是简单采用最近文件：

1. `osdk.toml` / `.osdk.toml` 的 `[tools]`；
2. `.tool-versions`；
3. backend 声明的 idiomatic 文件，且保持 backend 给出的文件名顺序；
4. Node 的 `package.json#engines.node` 或 `devEngines.runtime`；
5. 用户全局 `[tools]`。

因此父目录的高优先级 `osdk.toml` 会压过子目录的 `.nvmrc`。普通 idiomatic 文件读取第一个非空、非注释值并移除前导 `v`；`go.mod` 和 `rust-toolchain.toml` 使用各自的结构化解析。相关回归覆盖见 [`version/resolver.rs` tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs)。

## 项目包管理器

[`resolve_package_manager`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) 的顺序是：向上查找 `osdk.toml [tools]` 中的 `npm`、`pnpm`、`yarn`，然后向上查找 `package.json#packageManager`，最后读取 `devEngines.packageManager`。仅接受这三个 manager 的精确 semver；缺失版本、URL、路径、hash 或 build suffix 都会明确失败。`packageManager` 优先于 `devEngines.packageManager`。

## Backend 解析与例外

CLI 在应用版本 alias 和一次性 backend 选项后调用 [`Backend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)。默认实现对精确版本直接返回，不请求远端列表，并保留所有 request options；非精确请求通过 `list_remote_versions` 和 `select_version` 解析。精确版本“免查列表”并不决定加密验证保证：安装阶段会应用当前 checksum/attestation 策略；在没有可用证据且 `require_checksums=false` 时仍可能继续。

部分 backend 覆盖默认算法。例如 Node 处理目标架构与 npm range，Python 处理实现、变体、catalog 和预发布策略，Java 处理发行版及 JDK/JRE，Rust 则把 channel 或版本交给隔离的 rustup。入口分别见 [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs)、[`python.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/python.rs)、[`java.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/java.rs) 和 [`rust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/rust.rs)。

## Lockfile 快路径与边界

无显式工具且无额外选项的 `osdk install` 会优先读取最近的 `osdk.lock`。[`locked_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) 按当前平台恢复已保存版本字符串、公开 options 和锁定 artifact 信息；没有当前平台区段时才回退到常规项目解析。大多数 backend 保存精确版本；Rust 的 `stable`、`beta`、`nightly` 等浮动 channel 则仍是 channel 名，后续 rustup 安装可能得到更新 toolchain。锁文件按平台保存独立结果，允许同一项目并存 Linux、macOS 和 Windows 解析。

锁定的是解析结果和 artifact 身份，不是“已经可信”的声明。重新安装会重新执行当前可用或策略要求的 checksum/attestation 验证；若锁记录没有 digest/evidence 且 `require_checksums=false`，pipeline 仍可能在没有加密完整性验证的情况下安装。若显式传工具或 `-o`，不会使用 lockfile 快路径。

## 可验证的不变量

- 解析优先级、父目录继承、结构化文件和非法 package-manager 值由 [`resolver` 单元测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) 覆盖。
- semver 前缀、range、LTS 与预发布策略由 [`version` 单元测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) 覆盖。
- 精确版本必须保留 backend options 的回归测试在 [`backend/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs)。
- CLI 的 package-manager 自动选择、Node 注入和 lock 恢复由 [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs) 与 [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) 覆盖。
