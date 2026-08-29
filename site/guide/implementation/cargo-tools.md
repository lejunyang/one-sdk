# Cargo 开发工具实现

本页说明用户侧 [Cargo 开发工具](../cargo-tools)工作流背后的实现。Cargo 工具属于
动态原生工具：osdk 会先解析来源和精确的受管 Rust 依赖，再把一次有界安装委托给
`cargo-binstall` 或 `cargo install`，之后由 osdk 接管 binary inventory 与生命周期。

## 动态身份与解析

`ToolSpec::parse` 通过中央 `CARGO_SCHEMA` 处理 `cargo` 命名空间；动态 Registry factory
随后为每个规范 ID 创建一个 `CargoPackageBackend`。subject 决定两类来源之一：

- Registry crate 名会转成小写，并限制为可移植的 Cargo 名称字符；
- HTTPS Git URL 保留路径大小写，但必须是规范形式，不能含凭据、query、fragment、
  路径穿越或末尾斜杠，并且必须包含非根仓库路径。

解析器会区分仓库 URL 后的 selector 和 URL user information 中可能出现的 `@`；
随后 Git URL 校验器仍会拒绝 user information。Registry selector 只允许 `latest`、
精确语义版本或一到两段数字前缀。Git selector 只允许 `latest`、`tag:<ref>`、
`branch:<ref>` 和 `rev:<40 位小写十六进制>`。ref 校验会拒绝 `..`、`@{`、
`.lock`、控制字符、以点开头的路径段等歧义或不安全形式。

选项 schema 会规范化 `features`、`default-features`、`bin`、`crate` 和 `locked`。
feature 会经过校验、排序和去重；默认值 `default-features=true` 与 `locked=false`
会从规范身份中省略。`crate` 复用可移植 Registry crate 校验，并且只允许 Git 来源。
所有保留下来的公开选项都属于安装身份。

## 解析与精确 Rust 绑定

Registry 解析向常规 ranked-source selector 请求 Cargo metadata。默认来源把
`https://crates.io/api/v1/crates` 与 `sparse+https://index.crates.io/` 配对，也把
rsproxy API 与其 sparse index 配对，因此所选 metadata endpoint 与 Cargo index 始终
一致。响应与缓存 metadata 上限均为 8 MiB。解析会移除 yanked release，再排序和去重；
`latest`/前缀请求选择最高稳定匹配，精确请求则可以选择一个未 yanked 的预发布版本。
所选 index 作为私有解析 metadata 保存，之后传给 provider。Git selector 不需要远端
版本列表请求，会按原文保留。

解析前，CLI 编排会检测任意 `cargo:` 请求，并要求且只允许一个受管 `rust` 请求。
显式 Rust 请求优先；否则注入常规 active/configured Rust 选择。该选择必须解析成精确
版本，因此浮动的 `stable`、`nightly` 或 `latest` 会在 provider 执行前失败。Rust 请求
会被分区并优先安装/解析；得到的精确版本再以私有 runtime metadata 注入每个 Cargo
请求与解析版本。

backend 随后要求一个完整、非链接的 osdk Rust 安装，在该精确 toolchain 中定位
`cargo` 与 `rustc`，按 rustup 的 `manifest-rustc-*` 清单纳入 rustc 组件 payload，并纳入
所选目标 sysroot library 目录下的全部普通文件。这些字节连同精确版本、平台和 toolchain
名称共同形成原生依赖身份。带锁、原子写入且位于 runtime marker 旁的 receipt 会缓存
`b3-rust-v2:` 身份以及完整排序后的路径/大小/mtime inventory；只有 inventory 未变化时
热路径才复用摘要，metadata 漂移会触发完整内容重算。链接 toolchain
在这里和写 lock 时都会被拒绝，因为它没有稳定的受管 artifact 身份。所选受管 runtime
中上述受身份约束的构建关键文件任一字节发生变化，已有 Cargo 工具候选都会无法通过 runtime 身份校验。

## 安装身份与 material 分类

`NativeToolLifecycle` 用以下内容构造隔离的 `InstallIdentity`：

- 规范 Cargo backend ID 与已解析 selector/version；
- 当前平台与 isolated scope；
- 全部规范公开 Cargo 选项；
- 精确 Rust 版本/平台及其 `b3-rust-v2:` 构建关键身份；
- Registry package 与所选 index，或 Git URL 与 selector。

得到的 `b3-v2:` install ID 同时决定物理根和跨进程锁。不同 feature、binary、
workspace crate、lock 策略、Rust 构建关键身份、Registry index、仓库拼写或 selector 不可能
别名到同一安装。`cargo-resolution.json` schema 1 会在发布根内再次记录 source kind、
source、version、backend 与 replay 分类，并在复用时校验。

这条原生路径不使用 archive 下载/CAS pipeline。provider 运行时由 Cargo 负责 source
与依赖获取；osdk 负责 staging 根、持久 binary 身份、校验与发布。

## Provider 选择与退出码 94 边界

`controlled_binstall` 只会在 osdk 受管 `<data>/cargo/bin` 中查找
`cargo-binstall`（含平台可执行后缀），绝不采用环境 PATH 中的同名程序。只有同时满足
以下条件才走该优先路径：

- osdk 在线；
- 来源是 Registry crate，且不是 `cargo-binstall` 自身；
- 没有 `features` 选项；
- `default-features` 不是 `false`；
- 受控 binary 是普通文件。

`bin`、`locked`、精确版本、安装根与所选 index 都会传给 `cargo-binstall`；同时禁用
确认、telemetry、GitHub token discovery 及其 compile/quick-install strategy。成功后，
osdk 发布其输出。只有退出码 94 表示“没有兼容 binary artifact”：stage 会在继续持有
同一身份锁时删除并重建，然后仅尝试一次 `cargo install`。其他退出码、spawn 错误、
权限错误、超时或 capture 失败都是终止错误，不会尝试第二个 provider。Git 来源及需要
source build 的选项会直接走 `cargo install`。

对于 Registry crate，`cargo install` 会收到精确的 `=<version>`。Git 请求会把 selector
分别转换成 HEAD 不加 flag，或者 `--tag`、`--branch`、`--rev`。Git 专用的 `crate` 值
会成为 Cargo package 参数。两个路径都会按需传递 `features`、
`--no-default-features`、`--bin` 和 `--locked`。

## 进程隔离与边界

provider 通过 `CommandSpec::clear_env` 执行，不经过 shell；stdin 为空，stdout/stderr
并发读取，墙钟上限为一小时，每条输出流的捕获上限为 1 MiB。stage 会提供完整的子进程
环境：

```text
HOME, USERPROFILE        = <stage>/home
CARGO_HOME               = <stage>/cargo-home
CARGO_TARGET_DIR         = <stage>/target
CARGO_INSTALL_ROOT       = <stage>
RUSTUP_HOME              = osdk 受管 rustup home
RUSTC                    = <exact-toolchain>/bin/rustc[.exe]
PATH                     = <exact-toolchain>/bin + 清理后的系统路径
TMPDIR, TEMP, TMP         = <stage>/tmp
CARGO_TERM_COLOR         = never
GIT_TERMINAL_PROMPT      = 0
```

精确 toolchain 目录始终排在首位；osdk 会移除自身 shim 与 Cargo-home proxy 路径、
去重其余项，并保留 linker 与构建辅助程序需要的系统路径。发布前，osdk 会删除私有 home、
Cargo home、target、临时目录和 Cargo tracking metadata；它们都不会成为已安装工具
的一部分。

## Staging、发布与复用

共享原生工具 lifecycle 会获取精确身份锁；若已有候选则先校验，否则创建唯一 sibling
stage。完整但无效或被篡改的根会 fail closed，而不是被静默覆盖；不完整根只能在持有
该身份锁时删除。

发布会拒绝 symlink 和 provider 预先创建的保留 metadata，要求普通 `bin` 目录，枚举每个
可移植可执行文件，并在 `.osdk-native-receipt.json` 中记录大小与 SHA-256。随后依次写入
`.osdk-install.json`、`.osdk-complete` marker，以及把 install ID 绑定到发布 metadata
BLAKE3 摘要的相邻 seal；最后用 no-replace 目录 rename 原子暴露安装。尚未发布的 stage
在 drop 时会被删除。

复用、激活、shim、列表、`where` 与卸载都汇聚到同一校验边界：规范根与身份、无
symlink、有效 seal、匹配的 manifest 与 receipt、未变化的受管 Rust 构建关键文件，以及
精确的 binary 路径、大小和 SHA-256。卸载会在持有身份锁时只删除该身份对应的根。

## 离线行为

Registry selector 的解析可以读取之前缓存的 metadata 响应，Git selector 则不需要
metadata 请求，但这不代表支持全新离线安装。`prepare` 会先允许完整精确安装通过校验/
复用路径；如果离线状态仍需要创建 stage，backend 会在任何 provider 启动前停止，因为
Cargo 原生 lock 和 `osdk.lock` 都不包含完整 source graph。

因此，已经预热的 Cargo cache 也不会被当成受支持的重放契约。离线成功只表示复用
完整且身份精确匹配的现有安装，而不是从偶然存在的 cache 重新运行 provider。

## Lock schema 4 与如实重放

写 lock 时，私有 `__osdk_*` 字段会从公开 options 表移除，改为写入类型化 native
metadata：

```toml
schema = 4

[platforms.linux-x64.tools.rust]
request = "1.91.1"
version = "1.91.1"

[platforms.linux-x64.tools."cargo:ripgrep"]
request = "14"
version = "14.1.1"
options = { locked = "true" }

[platforms.linux-x64.tools."cargo:ripgrep".native]
runtime = "rust"
runtime_version = "1.91.1"
replay = "version-only"
source = "sparse+https://index.crates.io/"
```

native runtime 条目必须命名 `rust`、使用精确版本、匹配同一平台表中的 Rust 工具条目，
并且只携带一种受支持的 replay 分类。Registry 条目还必须在 `source` 中记录精确选择的
规范 `sparse+https://.../` index；凭据、query 和 fragment 都会被拒绝，Git 条目也会
拒绝该字段：

- Registry 版本使用 `version-only`；
- 完整的 40 位小写十六进制 Git revision 使用 `immutable-revision`；
- Git HEAD、tag 与 branch 使用 `floating-ref`。

读取时，这些类型化数据会注入私有 request option，之后继续走常规精确身份路径。
native 条目不能同时携带通用 artifact 或 npm metadata。Lock schema 1 到 3 无法表达
runtime 绑定，因此其中任何 `cargo:` 条目都会被拒绝，必须重新生成 schema 4。replay
标签描述 selector 强度，不是依赖图，也不保证全新离线安装。

## 主要验证点

针对性回归覆盖：

- 严格的 Registry/Git ID、selector、option 和 lock key 校验；
- 排除 yanked 版本的 Registry 解析及离线 metadata-cache 读取；
- 精确 Rust 注入、Rust-first 调度，以及 lock/runtime 一致性；
- 大小写不同的 Git 身份和对选项敏感的指纹；
- 受控 `cargo-binstall` 选择、成功路径、退出码 94 reset/fallback，以及其他所有失败的
  终止处理；
- 清空/私有 provider 环境与 provider 参数构造；
- 完整安装复用、全新离线拒绝、binary/receipt/seal/runtime 校验、原子发布与精确删除；
- schema 4 native round trip，以及 schema 1 到 3 的拒绝。

关键实现文件包括
[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)、
[`cargo_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/cargo_package.rs)、
[`native_tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/native_tool.rs)、
[`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)
与 [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs)。
