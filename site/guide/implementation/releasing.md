# 发布流水线

仓库的 [`.github/workflows/publish.yml`](https://github.com/lejunyang/one-sdk/blob/main/.github/workflows/publish.yml)
只在 `main` 分支最新提交的 message 明确带有发布标记时运行。普通 push 不会发布。发布前必须
先更新 `[workspace.package].version`；如果对应的 `v<version>` tag 已存在，prepare job 会拒绝
继续。

## 一次发布会生成什么

流水线先并行构建 Linux x64/arm64、macOS Intel/Apple Silicon 和 Windows x64 的三个
程序，并上传临时 artifact。构建全部成功后，它按以下依赖顺序发布 crates.io package：

1. `osdk-core`；
2. 等待该版本可被 crates.io API 查询；
3. `osdk-cli`；
4. `osdk-shim`。

`osdk-cli` 和 `osdk-shim` 都以精确版本依赖 `osdk-core`，所以 Registry 可见性等待不能
省略。core 在自身上传前经过 dry-run，两个依赖它的 package 则在 core 可见后一起
经过 `cargo publish --locked --dry-run`。发布 job 会移除仓库的构建镜像配置，并使用官方
crates.io index 与仓库 lockfile 校验可发布依赖。只有 crate
发布全部成功后，流水线才创建 GitHub tag/Release、归档五个平台的预编译程序并生成
`SHA256SUMS`。这样不会在 crates.io 发布失败时留下一个看似完整的 GitHub Release。

用户安装主命令时使用：

```bash
cargo install osdk-cli --locked
```

这会安装 `osdk`。`osdk-shim` 是独立 package；完整的
日常安装仍优先使用 GitHub Release 安装器，因为安装器会把两个同版本程序放到同一目录。

## 二进制体积

用户下载到的就是这两个可执行文件，所以发布 profile 是按体积而非按峰值速度调的。当前 `osdk` 约 9.3 MB，`osdk-shim` 约 3.2 MB。

达到这个结果的配置，以及在本仓库实测到的数据：

| Profile | 两个二进制合计 |
| --- | --- |
| `opt-level = 3`、`lto = "thin"` | 44.6 MB |
| `opt-level = 3`、`lto = "fat"`、`codegen-units = 1` | 37.0 MB |
| `opt-level = "z"`、`lto = "fat"`、`codegen-units = 1`、`panic = "abort"` | 16.7 MB |

按体积优化对 shim 是安全的。shim 是对延迟最敏感的那个二进制，因为每次执行 `node` 或 `npm` 都会经过它；但实测 `opt-level = "z"` 下启动中位数为 48.3 ms，`opt-level = 3` 为 50.5 ms —— 进程创建开销占主导，缩小代码在这里没有可观测的代价。

真正付出代价的地方并不直观：`opt-level = "z"` 会让 sha2 的可移植实现损失约 65% 吞吐，在哈希 256 MiB 时从 2300 MiB/s 降到 800 MiB/s。而每个下载的归档都要做校验，放任不管就等于让每次安装都变慢。因此工作区清单用 per-package profile override 把哈希相关的 crate 固定回 `opt-level = 3`，以约 0.02 MB 的体积代价换回完整吞吐。BLAKE3 实测不受影响（它自带手写 SIMD），但同样做了固定，因为进入内容寻址存储的每个文件都要经它哈希。

当 per-package override 匹配不到任何包时，Cargo 只发 warning、不报错。依赖一旦改名，这项保护就会在构建全绿的情况下静默失效，所以 `crates/osdk-core/src/pipeline/verify.rs` 里的 `hashing_crates_are_pinned_to_a_fast_opt_level` 会断言这些固定项存在。

`panic = "abort"` 只作用于发布出去的二进制。Cargo 对测试目标会忽略该设置，因此依赖 `catch_unwind` 的测试在 `cargo test --release` 下仍然正常工作。

## shim 为什么比 CLI 小得多

调 profile 只解决了一半问题。shim 曾经和 CLI 几乎一样大，原因是结构性的：它只读取状态，但持有 `Arc<dyn Backend>`，而 `Registry::new` 会实例化全部 13 个 backend，于是 `Backend` 的每个方法都落入 vtable，链接器无法证明其不可达。整条安装链路因此被留在 shim 里，包括占 `osdk-core` 314 个依赖 crate 中 240 个的 sigstore 子树。

这个代价是实测出来的：构建若干只链接 `osdk-core`、且只调用只读接口的探针二进制。

| 探针链接的内容 | 体积 |
| --- | --- |
| 仅目录解析 | 0.13 MB |
| 加上 `Config::load` | 0.74 MB |
| 加上 `http::client` | 1.83 MB |
| 加上 `Registry` 与 `dyn Backend` | 7.02 MB |

最后一步看似已经说明问题，但它把两件事混在了一起：一是 vtable 保活的安装链路，二是 13 个 backend 自身的只读代码 —— 后者是 shim 无论如何都需要的。改用静态分发调用同样这 13 个 backend（此时链接器可以丢掉用不到的 `install` 函数体）只需 1.83 MB。所以单独归因于安装链路的部分约为 5.15 MB。

因此，四个仅安装用到的 trait 方法 —— `list_remote_versions`、`resolve_version`、`install`、`uninstall` —— 被放到默认开启的 `install` feature 之后，shim 则以 `default-features = false` 依赖 `osdk-core`。sigstore 相关 crate 改为可选并由该 feature 引入，于是 shim 的依赖图从 982 个 crate 降到 441 个，也不再包含第二份 `reqwest`。

关闭安装路径时，`GithubAttestation` 和 `VerificationEvidence` 会被替换为无法构造（uninhabited）的占位类型。所有校验调用都位于 `if let Some(attestation) = attestation` 之内，而无法构造类型的 `Option` 恒为 `None`，因此这些分支在编译期即不可达，同时函数签名、结构体字段和调用方都保持原样。另一种做法 —— 在三十多处引用（其中包含公开字段）上逐个加 `#[cfg]` —— 会难读得多。

### shim 必须单独构建

Cargo 会在单次 `cargo build --workspace` 内统一（unify）feature，所以把两个二进制放在一条命令里构建，会让 shim 重新启用 `install` 并悄悄退回原来的体积。产物照样能正常工作，且没有任何警告。

因此发布构建对每个二进制各用一次调用：

```bash
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

两者可以共用同一个 target 目录；Cargo 会把两种 feature 变体并存缓存，来回切换时不会重新编译。shim 中还有一条编译期断言：一旦安装路径被重新链接进来，构建会失败并给出原因说明。该断言仅作用于 release 构建，因此开发时 `cargo check`、`cargo test`、`cargo clippy` 仍可照常对整个工作区执行。

## 首次发布认证

截至当前，三个 crate 都尚未在 crates.io 创建。crates.io Trusted Publishing 要求 crate
先存在，因此第一次发布需要在 GitHub 仓库创建名为 `crates-io` 的 Environment，并在其中
添加 `CARGO_REGISTRY_TOKEN` secret。Token 需要 `publish-new` 和 `publish-update` scope；
不要把它写入仓库、日志或
普通配置文件。可为该 Environment 设置 required reviewer，把不可撤销的首次发布放在人工
确认之后。

首次发布成功后，在每个 crate 的 crates.io Settings 中添加相同的 GitHub Actions Trusted
Publisher：

| 字段 | 值 |
| --- | --- |
| Repository owner | `lejunyang` |
| Repository name | `one-sdk` |
| Workflow filename | `publish.yml` |
| Environment | `crates-io` |

三个 crate 都配置完成并验证下一次发布成功后，删除 GitHub Environment 中长期有效的
`CARGO_REGISTRY_TOKEN`。后续 workflow 使用 `rust-lang/crates-io-auth-action` 通过 GitHub
OIDC 换取任务生命周期内的短期 token，并在 job 结束时自动撤销。流水线保留首次发布 token
作为 OIDC 未配置时的 bootstrap fallback；删除 secret 后就只剩 Trusted Publishing 路径。

## 发布检查清单

1. 同时更新 workspace version 和 `[workspace.dependencies]` 中 `osdk-core` 的精确版本，并同步面向用户的版本说明。
2. 确认 CI、Windows Wine workspace 测试和文档构建通过。
3. 确认 `crates-io` Environment 的 reviewer/credential 已就绪。
4. 让 `main` 上用于发布的最新提交明确包含发布标记并 push。
5. 检查 `osdk-core`、`osdk-cli`、`osdk-shim` 三个 package 和 GitHub Release 均为同一版本。

crate 版本不可覆盖。若流水线只完成了部分 crate，修复原因后需要先 bump workspace version，
再重新发布；不要尝试覆盖已经上传的版本。
