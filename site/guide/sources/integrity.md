# 完整性、签名与 Attestation

```bash
osdk --require-checksums install node@20
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

普通 checksum 支持 SHA-256、SHA-512、BLAKE3；npm SRI 支持 `sha256-` 和
`sha512-`，同时存在时优先 SHA-512。`--require-checksums` 的精确含义是：必须有普通
checksum，或有已验证 attestation 提供的可信 artifact SHA-256。在线发现的 checksum
会随 archive 缓存持久化，并在 pipeline 实际执行离线重装时重新校验。

签名验证默认由 `settings.verify_signatures=true` 开启，也可用
`OSDK_VERIFY_SIGNATURES=false` 明确关闭。它只适用于 backend 内置可信公钥的
Minisign manifest；当前注册的是 `github:jdx/mise`。缺少 manifest/签名可继续寻找其他
checksum，签名存在但无效则硬失败。

GitHub Artifact Attestation 策略为：

| 策略 | 行为 |
| --- | --- |
| `off` | 默认，不查询证明 |
| `if-available` | 没有证明可继续；发现但无效、格式错误或仓库不匹配则失败 |
| `required` | 必须存在并通过验证 |

验证绑定 artifact SHA-256、`owner/repo`、GitHub Actions OIDC issuer、Fulcio 证书链
与 SCT、DSSE subject、Rekor body/SET/checkpoint/Merkle inclusion 和签名时间。GitHub
v0.3 TSA bundle 则验证内置 GitHub trust root、timestamp、证书链、签名、摘要与仓库
声明。证明 API 每次最多取 30 条；`bundle_url` 及重定向后地址都必须为 HTTPS，
Snappy 输入与解压 JSON 上限均为 8 MiB。

## TLS 证书校验

TLS 证书按操作系统信任库校验：Windows 证书存储、macOS Keychain、Linux 上的常规
OpenSSL 路径。osdk 不自带根证书副本。

实际影响是证书信任跟随机器。系统级安装的企业 CA，或被系统更新移除的已吊销根证书，
都无需等待 osdk 发版即可生效——这也是 osdk 能在做 TLS 审查的代理后面正常工作的原因。
代价是 osdk 依赖宿主机被正确配置：一个没有装 `ca-certificates` 的精简容器镜像，在装上
根证书之前所有 HTTPS 下载都会失败。

校验无法关闭。证书不受信任、已过期、自签名或域名不匹配时，下载会在写入任何字节之前
失败，并在错误信息中指出具体原因。
