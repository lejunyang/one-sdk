# Cargo 开发工具

osdk 通过 `cargo:` 命名空间安装 crates.io 兼容 Registry 或规范 HTTPS Git
仓库中的 Rust 命令行应用。每个工具都绑定一个精确的 osdk 受管 Rust toolchain；
切换 Rust 版本或修改构建选项会得到不同的安装身份。

::: tip 先区分两个名字
`rust@1.91.1` 安装 Rust toolchain；`cargo:ripgrep@14.1.1` 使用该 toolchain
安装 `ripgrep` crate。`cargo:` 请求不能使用环境中的 Cargo、本地链接的 toolchain，
也不能绑定浮动的 Rust 选择。
:::

## 安装 Registry 工具

先选择一个精确的受管 Rust 版本，再安装或固定 Cargo 工具：

```bash
osdk use rust@1.91.1
osdk use cargo:ripgrep@14.1
eval "$(osdk activate bash)"
rg --version
```

也可以在不修改项目配置时，一次安装两个请求：

```bash
osdk install rust@1.91.1 cargo:ripgrep@14.1.1
osdk exec --tool rust@1.91.1 --tool cargo:ripgrep@14.1.1 -- rg --version
```

Cargo 工具必须对应且只对应一个显式请求或配置中的 Rust，并且 Rust 版本必须精确。
这个流程会拒绝 `rust@stable`、`rust@nightly`、`rust@latest` 和本地链接的 Rust
toolchain。项目也可以直接写成：

```toml
[tools]
rust = "1.91.1"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
```

Cargo 工具会把受管 Rust 的精确版本/平台，以及 Cargo/rustc 启动器、rustc 组件 payload
和所选目标的完整 sysroot library payload 的内容身份记录为安装依赖。metadata receipt 会
避免每次 shim 启动都重新散列这些大文件；路径、大小或修改时间发生变化时仍会触发内容重算。因此，
用另一 Rust 版本安装相同 crate 时，不会静默复用第一次构建的结果。

## Registry 版本

Registry 请求格式如下：

```text
cargo:<crate>[@SELECTOR]
```

| 请求 | 选择结果 |
| --- | --- |
| `cargo:ripgrep` 或 `cargo:ripgrep@latest` | 最新稳定且未 yanked 的版本 |
| `cargo:ripgrep@14` | 最高的稳定、未 yanked `14.x` 版本 |
| `cargo:ripgrep@14.1` | 最高的稳定、未 yanked `14.1.x` 版本 |
| `cargo:ripgrep@14.1.1` | 该精确且未 yanked 的版本 |

Registry crate 名会规范化为小写。明确请求的预发布精确语义版本只要存在且未 yanked
也可使用。这里不接受 semver range、通配符、前导 `v`，以及 `^14` 这类 Cargo
requirement 语法。

`osdk list-remote cargo:ripgrep` 会列出所选 Cargo metadata source 中可见的版本。
osdk 当前内置 crates.io 与 rsproxy 默认来源，并在选择前使用常规 source 探测与缓存策略。

## 从 HTTPS Git 仓库安装

把仓库 URL 写成 `cargo:` 的 subject：

```text
cargo:https://git.example.com/team/tool.git@latest
cargo:https://git.example.com/team/tool.git@tag:v1.2.3
cargo:https://git.example.com/team/tool.git@branch:release/1.x
cargo:https://git.example.com/team/tool.git@rev:0123456789abcdef0123456789abcdef01234567
```

省略 selector 等价于 `latest`，会安装仓库默认分支当前的 HEAD。tag 和 branch 会作为
明确 Git ref 传给 Cargo。只有 `rev:` 后恰好跟 40 位小写十六进制字符时，才表示
不可变 Git revision。

仓库必须是带仓库路径的绝对、规范 HTTPS URL。凭据、query、fragment、反斜杠、
路径穿越和末尾斜杠都会被拒绝。URL 路径大小写会保留，并属于工具身份的一部分。

对于 workspace 仓库，用 `crate` 选择需要由 Cargo 安装的 package：

```toml
[tools]
rust = "1.91.1"
"cargo:https://git.example.com/team/workspace.git" = {
  version = "rev:0123456789abcdef0123456789abcdef01234567",
  crate = "workspace-cli",
  bin = "workspace-cli",
  locked = true,
}
```

`crate` 只对 Git 来源有效。Git 请求没有远端版本目录，因此 `list-remote` 只适用于
Registry crate。

## 构建选项

选项可以保存到结构化工具项中，也可以用 `-o` 传给单次命令。下面所有支持的选项
都属于安装身份。

| 选项 | 默认值 | 作用 |
| --- | --- | --- |
| `features` | 无 | 逗号分隔的 feature 名；也接受 TOML 数组并进行规范化 |
| `default-features` | `true` | `false` 会向 Cargo 传递 `--no-default-features` |
| `bin` | package 的全部 binary | 只选择一个可移植的 binary 名 |
| `crate` | 仓库默认 package | 选择 Git 仓库中的 package；Registry 请求会拒绝它 |
| `locked` | `false` | `true` 会向 Cargo 传递 `--locked` |

例如：

```bash
osdk install rust@1.91.1 \
  'cargo:ripgrep[features=pcre2,default-features=false,bin=rg,locked=true]@14.1.1'
```

修改 feature 集合、默认 feature 策略、binary、workspace crate、`locked` 设置、
精确 Rust 依赖、来源或 selector，都会选择不同的指纹化安装。复用、激活、shim、
`where`、`uninstall` 和 `reshim` 都要求身份匹配。

## 安装器选择与隔离

对于符合条件的在线 Registry 安装，osdk 优先使用其受管 Cargo home 中的受控
`cargo-binstall`。Git 来源、`cargo-binstall` 自身、带 `features` 的请求、
`default-features=false` 请求和离线安装会直接使用所选受管 toolchain 的
`cargo install`。如果执行了 `cargo-binstall`，只有退出码 94（该 provider 约定的
“没有可用 binary”）才会回退到 `cargo install`；其他非零退出、启动失败或超时都会
直接终止。

两个 provider 都会清空环境后运行。osdk 会提供 staging 内私有的 `HOME`、
`CARGO_HOME`、target 目录、临时目录和安装根；`PATH` 先放所选受管 Rust toolchain 的
bin 目录，并保留经过清理、供 linker 与构建工具使用的系统路径，`RUSTC` 也明确指向该
toolchain。provider 不会安装到用户的常规 Cargo home，环境中的 Cargo/rustc 也不能抢占。

只有通过校验的 binary 才会从 stage 发布到 osdk 的指纹化安装根。Cargo source 和
构建目录只是临时 provider workspace，会在发布前删除。

## 生命周期命令

Cargo 工具使用常规受管工具生命周期：

```bash
osdk current cargo:ripgrep
osdk list cargo:ripgrep
osdk list-remote cargo:ripgrep
osdk where cargo:ripgrep@14.1.1
osdk outdated cargo:ripgrep
osdk upgrade cargo:ripgrep
osdk --yes uninstall cargo:ripgrep@14.1.1
osdk reshim
```

要让 Cargo 工具安装继续通过校验，所选精确 Rust toolchain 必须仍已安装且未改变。
如果存在多个本来都能匹配的受管 Rust 身份而产生歧义，请通过 lockfile 选择工具。

## 离线行为与 lock 重放

已经完整安装的 Cargo 工具，在来源、selector、公开选项、平台和受管 Rust 身份都
精确匹配时，可以在离线状态下完成校验与复用，而不启动 provider。全新离线安装、
修复或重建则明确不受支持，即使 Cargo 已有部分缓存也不例外：Cargo 原生 lock 与
osdk lock 都不包含完整 source graph。

当前 `osdk.lock` schema 4 会记录精确 Rust runtime 绑定，并如实写入以下三类重放等级：

| 来源与 selector | 重放分类 | 保证 |
| --- | --- | --- |
| 已解析为精确版本的 Registry crate | `version-only` | 固定顶层 crate 版本，但不固定完整依赖/source graph |
| Git `rev:<40 位小写十六进制>` | `immutable-revision` | 把仓库 selector 固定到一个完整 commit revision |
| Git `latest`、`tag:` 或 `branch:` | `floating-ref` | 记录 ref；它之后可能解析到不同 source |

lock 还会保留公开构建选项、Registry crate 所选的规范且无凭据 sparse HTTPS index，
并要求同一平台区段中存在匹配的精确 `rust` 条目。它足以选择并复用已经完整安装的匹配身份，但不是
离线 source bundle，也不会把浮动 Git ref 变成不可变 commit。

解析、provider 选择、身份校验、发布和 lock schema 的实现细节见
[Cargo 开发工具实现](./implementation/cargo-tools)。
