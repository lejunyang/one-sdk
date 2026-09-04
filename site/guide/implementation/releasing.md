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
