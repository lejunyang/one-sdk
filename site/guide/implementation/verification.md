# 制品验证与安全边界

osdk 在安装流水线中把“字节是否一致”“元数据是否由已知密钥签名”和“制品是否由指定 GitHub 仓库生成”视为三种不同的证明。它们可以互相补充，但不能互相替代。核心入口位于 [`pipeline`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 和 [`verification`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/verification/mod.rs)。

## 默认策略

内置默认值定义在 [`Settings::default`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs)：

| 能力 | 默认值 | 含义 |
| --- | --- | --- |
| `verify_signatures` | `true` | 后端提供受信任的签名校验路径时启用它 |
| `require_checksums` | `false` | 没有可用校验和时仍可继续安装 |
| `attestations` | **`off`** | 默认不查询、下载或验证 GitHub artifact attestation |

可以用 `OSDK_VERIFY_SIGNATURES`、`OSDK_REQUIRE_CHECKSUMS`、`OSDK_ATTESTATIONS` 或对应配置覆盖这些值；命令行 `--require-checksums` 和 `--attestations` 再覆盖已加载配置。配置优先级和环境变量解析见 [`config/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs)，CLI 覆盖见 [`app.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/app.rs)。

## 校验和

[`pipeline::verify`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs) 支持 SHA-256、SHA-512 和 BLAKE3，以 64 KiB 分块读取文件并进行不区分大小写的十六进制比较。npm SRI 支持 `sha512-...` 与 `sha256-...`，多个值中优先 SHA-512。GitHub 通用后端按以下顺序寻找摘要：

1. 在线且启用签名校验时，可用的受信任 minisign 校验和清单；
2. 否则使用静态目录中显式提供的摘要；
3. 否则使用 `<asset>.sha256`、`<asset>.sha256sum` 或 `<asset>.sha256.txt` sidecar；
4. 否则使用 `SHASUMS256.txt`、`SHA256SUMS`、`sha256sums.txt` 或 `checksums.txt`。

下载完成后、解压或执行前，流水线重新计算摘要。成功的摘要会写入下载缓存旁的 `.checksum`，使离线重装仍能重新验证制品。`osdk.lock` 中的 URL 和文件名用于重建锁定安装计划；若锁文件包含 checksum，重装时会对当前 artifact bytes 重新计算并比较。历史 evidence 仅是审计记录。相关实现见 [`github.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/github.rs)、[`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) 和 [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs)。

安全边界：普通 sidecar 和共享 checksum 文件本身没有身份认证，只能发现传输损坏或与该摘要不一致的内容，不能证明发布者身份。`require_checksums=true` 只要求存在可验证摘要；经成功 attestation 认证的 SHA-256 也满足这一要求。默认 `require_checksums=false`，因此没有摘要和 attestation 的制品可以安装。

## Minisign 签名

签名验证针对校验和清单，而不是单独定义另一套制品签名协议。osdk 先用编译进二进制的公钥验证 minisign 清单，再从已验证清单读取目标制品的 SHA-256，最终仍由安装流水线校验制品字节。当前受信任密钥表在 [`trusted_key`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs) 中，是按 `github:owner/repo` 精确匹配的封闭列表。

`verify_signatures=true` 并不意味着所有下载都必须有签名：没有内置密钥、清单不可达或签名文件不可达时会继续尝试其他校验和来源；但一旦取得清单和签名，签名无效就是硬错误，不会降级为信任该清单。离线模式不发起签名清单请求，而是依赖先前持久化并重新计算的摘要。

## GitHub artifact attestation

Attestation 仅接入 `github:owner/repo` 后端。策略为：

- `off`：**默认值**；完全跳过 attestation。
- `if-available`：没有返回 bundle 或离线时没有缓存 bundle 可以继续；已取得但无效的 bundle 会失败。API、代理或 bundle 下载错误也会传播，不能理解为“所有网络错误都忽略”。
- `required`：制品、在线或缓存 bundle 缺失，以及任何验证错误都会阻止安装。

验证器先计算制品 SHA-256，并以 `owner/repo + digest` 获取或定位缓存 bundle。GitHub API 返回的内联 JSON 直接使用。对于 `bundle_url` 响应，初始 URL 与最终响应 URL 必须是 HTTPS，压缩内容与 Snappy 解压结果分别限制为 8 MiB；GitHub API 响应中的内联 bundle 和本地 bundle cache 当前没有等价的读取大小上限。这里不保证 redirect 同源，也不逐跳检查中间 URL。bundle 缓存通过同目录临时文件和 rename 发布。

对于带 Rekor 记录的 bundle，验证覆盖：嵌入的 Sigstore 公共信任根、Fulcio 证书链和 SCT、GitHub Actions OIDC issuer、签名证书中的仓库身份、制品签名与 DSSE 摘要、Rekor canonical body 一致性、Signed Entry Timestamp、签名 checkpoint、Merkle inclusion proof 以及签名时间。验证要求恰好一个 transparency-log entry。

GitHub TSA bundle 没有 Rekor entry，走独立路径：先从已签名 DSSE statement 校验仓库 claim，再使用内嵌 GitHub trust root 校验制品摘要与 bundle；该路径显式跳过 tlog 和 SCT，因为它依赖 TSA 结构而不是 Rekor 证明。不要把它描述成 Rekor 验证。成功结果写入 `.osdk-artifact.json`，并可复制到锁文件作为 evidence。

信任边界还包括：GitHub token 仅发送给 `api.github.com`，不会转发给第三方代理；代理只是传输通道，最终信任来自摘要、签名或 attestation，而不是镜像身份。内嵌信任根随 osdk 版本更新，不会在线自动替换。

## 归档与路径安全

[`extract.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/extract.rs) 支持 `tar.gz`、`tar.xz`、`tar.zst` 和 ZIP。归档先展开到缓存 scratch 目录，再选择性剥离唯一顶层目录。ZIP 使用 `enclosed_name()`，无法安全包含在目标目录中的条目会被跳过；tar 使用 `tar::Archive::unpack(dest)` 的目标目录约束。目录型 `subdir` 还必须是仅含普通组件的相对路径，拒绝绝对路径、`.`、`..` 和前缀组件。模型文件路径应用同样的 lexical 约束，见 [`safe_relative_path`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs)。

边界需要明确：代码没有设置解压大小、文件数或压缩比上限，因此不防御归档炸弹；tar 链接由归档库处理，后续 CAS 会原样重建提取出的符号链接，并未显式拒绝绝对或越界 link target。ZIP 的不安全名称是跳过而非整包失败。对发布者不完全信任的制品，应在额外沙箱中安装；认证摘要或 attestation 只能证明 bytes/provenance，不能使恶意归档结构变得安全。

## 关键测试

- [`pipeline/verify.rs` 单元测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs)覆盖摘要向量、清单解析、SRI 和 minisign 成败。
- [`verification/mod.rs` 测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/verification/mod.rs)使用真实 fixture 覆盖离线 Sigstore、错误仓库、篡改制品、缺失/篡改 Rekor proof、checkpoint、SET、TSA bundle 和 attestation 产生摘要证据。
- [`pipeline/mod.rs` 测试](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs)覆盖离线缓存重验、严格 checksum gate 和失败不写 complete marker。
- [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs)覆盖 CLI policy 覆盖、锁定摘要篡改拒绝，以及 required 模式不信任锁文件中未经重新验证的 evidence。
