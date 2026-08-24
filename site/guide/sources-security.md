# 下载源与供应链安全

osdk 的 SDK/模型下载源与项目依赖 Registry 是两套控制面。本页说明前者，以及
offline、预发布、checksum、签名、GitHub attestation 和通用 GitHub Release backend。
npm-compatible Registry 见[JavaScript 包管理器](./package-managers#registry-预检)。

## Source 命令参考

```text
osdk source list TOOL_OR_PROVIDER
osdk source test TOOL
osdk source test huggingface|modelscope --model owner/repo[@revision]

osdk source add TOOL_OR_PROVIDER
  --id ID
  --download-url URL
  [--index-url URL]
  [--forward-credentials]

osdk source remove TOOL_OR_PROVIDER ID
osdk source pin TOOL_OR_PROVIDER ID
osdk source unpin TOOL_OR_PROVIDER
```

| 命令/参数 | 作用 |
| --- | --- |
| `list TOOL_OR_PROVIDER` | 列出有效来源、类型、URL 和 pin |
| `test TOOL` | 强制重新探测普通 backend 并输出吞吐/TTFB 排名 |
| `test PROVIDER --model ...` | 对 Hugging Face/ModelScope 的真实仓库 metadata 与文件采样测速 |
| `add ... --id ID --download-url URL` | 添加或替换同 ID 的用户级 custom source |
| `--index-url URL` | metadata/index 与下载根不同时单独指定 |
| `--forward-credentials` | 允许自定义模型 endpoint 接收 provider 凭据 |
| `remove TOOL_OR_PROVIDER ID` | 删除用户级 custom source |
| `pin TOOL_OR_PROVIDER ID` / `unpin TOOL_OR_PROVIDER` | 在用户配置中设置/移除 source pin |

`add`、`remove`、`pin`、`unpin` 都编辑用户 `config.toml`。`--source ID` 是一次性
优先来源并保留其他来源作为回退；同一调用里的工具请求必须使用规范 backend ID，
例如写 `node` 而不是 `nodejs`，否则当前实现不会应用覆盖。`--refresh-sources` 对
`install`、`use`、`upgrade`、`exec` 强制重新探测；对 `model pull`，仅在没有显式
endpoint 或 pin、选择策略为 `auto` 且非 offline 时刷新。当前对 `lock`、`outdated`、
`list-remote` 不生效。
模型 `source test` 缺少 `--model` 会失败，普通工具使用 `--model` 也会失败。

## 有效来源列表

```toml
[sources]
selection = "auto"       # auto|pinned|ordered
probe_timeout_ms = 1500
cache_ttl = "6h"

[sources.node]
pin = "corp"
disable = ["tuna"]

[[sources.node.custom]]
id = "corp"
kind = "custom"          # official|mirror|custom
download_url = "https://mirror.example/node/"
index_url = "https://mirror.example/node/index.json"
headers = [["X-Example", "value"]]
forward_credentials = false
priority = 0
enabled = true
```

有效列表等于“内置来源减去 `disable`，再加 `custom`”。custom 的同名 ID 覆盖内置
来源，`enabled=false` 被过滤，较小 `priority` 排在前面。项目中的 source 设置需要
[显式信任](./projects#项目配置信任)。

`headers` 是显式 source 配置，与 `forward_credentials` 不同。osdk 自己发起的 metadata
请求和 source probe 只在初始 URL 与该 source 的 index/download URL 同 origin 时附加
这些 header；同源 redirect 保留，第一次跨源 redirect 后永久移除，header 值也不会
明文写入 cache。当前 Aube 2.1 embedded API 无法安全接收任意 `Source.headers`，因此
`npm:<package>` 的实际 package fetch 不转发这里的 header；认证 npm Registry 应通过
Aube/npm 原生可信配置或环境变量提供凭据。

## 选择、探测与故障转移

| `selection` | 行为 |
| --- | --- |
| `auto` | 读取 TTL 内测速缓存，否则并发探测；以吞吐量为主、TTFB 为惩罚排序 |
| `ordered` | 直接按 `priority` 顺序 |
| `pinned` | 没有具体 `sources.<tool>.pin` 时与 `ordered` 相同 |

具体 pin 会把该来源移到第一位，但其余来源仍保留为失败回退，并非“只允许这一源”。
默认探测超时 1500 ms、缓存 TTL 6h；非法 TTL 当前静默回退为 6h。普通 SDK probe
最多读约 1,000,000 bytes；模型 probe 最多 1 MiB。

metadata 或下载失败时，backend 按排序后的候选继续尝试。HTTP metadata 缓存允许
在线请求失败后使用 stale 值；严格 offline 则只读已有缓存。

## 离线模式

```bash
osdk --offline install bun@1.3.14
osdk --offline install                    # 可结合当前平台 lock
osdk --offline model pull qwen hf:Qwen/Qwen2.5-7B-Instruct@main
```

`--offline` 或 `OSDK_OFFLINE=true` 严格禁止网络：

- metadata、SDK archive、模型 metadata 和所选文件必须已缓存；
- 自动 source probe 被跳过；`source test` 及 SDK 安装类命令中的 `--refresh-sources`
  会失败，`model pull` 不会刷新，本就不支持该参数的命令仍忽略它；
- 缺少缓存时明确报错，不会偷偷联网；
- 对支持通用 artifact receipt 的 backend，lock 中的 artifact URL/checksum 可支持离线
  重装；pipeline 实际重装且有 checksum 时重新校验字节，已有完整安装则直接复用；
- `npm:<package>` 不使用通用 artifact URL；它需要随 `osdk.lock` 提交且校验通过的
  graph sidecar，以及预热的 Aube cache/store；
- `attestations=required` 还要求按 artifact SHA-256 缓存的证明 bundle，lock evidence
  不能代替重新验证。

`OSDK_OFFLINE` 只控制 osdk 自己及其 hook 所管理的兼容环境；项目子进程是否完全
离线仍取决于下游工具的原生参数。

## 预发布版本

```bash
osdk install bun@canary
osdk install deno@beta
osdk install github:owner/repo@1.2.0-beta.1
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

| 策略 | 行为 |
| --- | --- |
| `never` | 拒绝预发布，包括显式精确版本或通道 |
| `if-explicit` | 默认；仅显式预发布版本或 `canary|nightly|beta` 通道可选预发布 |
| `allow` | `latest`、前缀和 range 也可隐式选择预发布 |

该策略用于支持预发布感知的 Python、Bun、Deno 和 GitHub backend。
`list-remote` 当前仍只显示稳定版本。lock 会同时保存原始请求与精确解析版本。

## 完整性、签名与 Attestation

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

## 任意 GitHub Release 工具

```text
github:owner/repo[@VERSION]
```

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

### Asset 选择选项

所有选项通过可重复的 `-o|--opt KEY=VALUE` 传入：

| 选项 | 取值与作用 |
| --- | --- |
| `asset-regex=REGEX` | 正则选择 asset；必须恰好命中一个 |
| `asset-template=TEMPLATE` | 精确文件名模板；支持 `{version}`、`{os}`、`{arch}`、`{libc}` |
| `bin=PATH` | archive 内一个 binary 的安全相对路径 |
| `bins=P1,P2` | archive 内多个 binary；与 `bin` 互斥 |
| `rename=NAME` | 重命名单个输出 binary；要求最终只有一个 bin |
| `strip-components=N` | 安装后逐层进入 N 个唯一的非 `.osdk-*` 子目录 |
| `os=VALUE` | `linux|macos|darwin|windows` |
| `arch=VALUE` | `x64|x86_64|amd64|arm64|aarch64|x86|i686|arm|armv7` |
| `libc=VALUE` | `gnu|musl|none` |
| `catalog-url=URL_OR_PATH` | 使用 schema 1 静态 catalog，绕过 Releases API |
| `catalog-sha256=HEX` | 使用 `catalog-url` 时必填，验证其精确内容 |
| `catalog-subdir=PATH` | 为锁定 artifact 记录/恢复 archive 内子目录 |

`asset-regex` 与 `asset-template` 互斥。未给规则时按 host OS、架构、archive 类型
启发式评分，并排除 checksum、signature 和 source asset；零命中或多命中都失败。
未知 archive 后缀按裸二进制处理，Windows 自动补 `.exe`。

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1

osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

### 静态 catalog

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

catalog 可为 HTTP(S)、`file://` 或普通本地路径；schema 1 的每个 asset 必须包含
`name`、`url`、`checksum`、`os`、`arch`，`libc` 可选，artifact URL 必须 HTTP(S)。
HTTP catalog 在线时以摘要缓存，offline 从缓存读取；本地文件可直接离线读取。

### GitHub 访问、回退与 token

token 优先级是 `OSDK_GITHUB_TOKEN`、`GITHUB_TOKEN`、`GH_TOKEN`。Authorization
只发给精确的 `api.github.com`，绝不转发给代理。Releases API 最多读 10 页、每页
100 条。匿名限流时可从公开 Atom feed 和 expanded-assets HTML 尽力发现近期公开
release；不能替代完整历史。

内置 `github` 与 `ghproxy` source 会一致覆盖 API、release asset、Raw/Gist、
checksum/signature 文件和 attestation bundle，并按排序失败转移。代理前会先规范化为
官方 URL，确保 pin、缓存和身份不随代理形式漂移。
