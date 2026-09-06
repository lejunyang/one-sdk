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

## 环境变量里的镜像

各工具链本身都支持用环境变量指定镜像：rustup 读 `RUSTUP_DIST_SERVER`
（以及 `RUSTUP_UPDATE_ROOT`），go 命令读 `GOPROXY`，npm 系读
`npm_config_registry`、`pnpm_config_registry`、`YARN_REGISTRY`、
`YARN_NPM_REGISTRY_SERVER`、`BUN_CONFIG_REGISTRY` 等。

默认（`mode = "auto"`）下 osdk 会先校验这些值，再把它们**与内置镜像一起**参与
探测并按实测速度择优，而不是无条件采用：

- 校验不通过（不是合法 URL、不是 https、带凭据、带 query 或 fragment）时，
  osdk 打印一条 warning 并忽略该值，继续用内置镜像，而不是静默丢弃；
- 校验通过则作为 id 为 `env` 的候选加入探测，可在 `osdk source list <工具>`
  中看到；若它与某个内置镜像地址相同，则不会重复出现；
- `GOPROXY` 的 `off`、`direct` 以及逗号/竖线分隔的回退列表是合法的 go 设置，
  但不是单一可探测的镜像，因此会被跳过并给出说明。

需要无条件遵循环境变量时（例如公司内网镜像即使较慢也必须使用）用
`--source-mode env`，此时缺失或不合法都会**报错**而不是回退，避免配置错误被
静默忽略。

优先级：显式选择始终高于环境变量。`osdk source pin` 与一次性的 `--source ID`
仍然优先，环境变量只在没有显式选择时参与竞争。

## 有效来源列表

```toml
[sources]
selection = "auto"       # auto|pinned|ordered
mode = "auto"            # auto|env，见下文「环境变量里的镜像」
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
明文写入 cache。受管的 npm/pnpm 子进程只会收到 registry 覆盖以及一份 osdk 自有的空配置，
因此 `npm:<package>` 的实际 package fetch 不转发这里的 header。项目包管理器调用可以使用原生
可信配置；全局 npm 工具为了隔离 prefix，会拒绝认证或私有原生配置透传，全局安装请使用
可匿名访问的已配置 Registry。

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
  重装；pipeline 实际重装且有 checksum 时重新校验字节，已有完整 GitHub 安装仅在 receipt
  的锁定文件名/checksum 与动态选项身份都匹配时复用；
- `npm:<package>` 不使用通用 artifact URL。schema 4 `osdk.lock` 延续 schema 3 的 npm
  metadata 模型，只保存 scope、installer 与可选原生 lock 身份，而不携带依赖图；仅靠这些
  metadata 不能冷恢复依赖图。已有完整
  安装也只在记录的选项匹配时复用；支持原生 lock 重放的操作还需要安装器拥有的 lock 与
  已预热 cache/store。旧 lock schema 2 graph sidecar 仅用于兼容读取；
- `cargo:` 工具只有在来源、selector、选项、平台和精确受管 Rust 身份都匹配时，才能
  复用已有完整安装。全新离线安装或修复不受支持，因为 Cargo 原生 lock 与 `osdk.lock`
  都不包含完整 source graph；
- `go:` 工具也只能复用身份精确匹配的完整安装。其 schema 4 lock 会记录选中的 Go
  proxy、发现的 module root、公开构建选项与精确受管 Go 身份，但不记录全新离线构建所需
  的传递 module graph；
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
Cargo Registry 解析使用自己的固定规则：始终排除 yanked release，`latest` 与数字前缀
只选择稳定版本，只有精确 Cargo selector 可以选择明确的未 yanked 预发布版本。

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

### TLS 证书校验

TLS 证书按操作系统信任库校验：Windows 证书存储、macOS Keychain、Linux 上的常规
OpenSSL 路径。osdk 不自带根证书副本。

实际影响是证书信任跟随机器。系统级安装的企业 CA，或被系统更新移除的已吊销根证书，
都无需等待 osdk 发版即可生效——这也是 osdk 能在做 TLS 审查的代理后面正常工作的原因。
代价是 osdk 依赖宿主机被正确配置：一个没有装 `ca-certificates` 的精简容器镜像，在装上
根证书之前所有 HTTPS 下载都会失败。

校验无法关闭。证书不受信任、已过期、自签名或域名不匹配时，下载会在写入任何字节之前
失败，并在错误信息中指出具体原因。

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
对 osdk 自有 GitHub 安装，受支持的 asset、平台、catalog 摘要与布局选项属于动态安装
身份。`catalog-url` 可用于获取，
但不会持久化到动态身份；必填的 `catalog-sha256` 标识 catalog 内容。含 userinfo、查询参数或
fragment 的 HTTP(S) catalog URL 会被拒绝，避免通过这个选项持久化凭据。未知公开选项会在安装前拒绝；若同一版本的现有安装缺少新身份或身份不同，osdk 不会执行它。每个规范身份都记录在 `.osdk-install.json` schema 1 中，带 `b3-v2:` `install_id` 并使用独立的指纹化根，因此同版本变体可以共存。复用、activation、shim、`where`、uninstall 与 `reshim` 都由配置的精确身份驱动；旧 `.osdk-tool.json` 状态只会被识别，绝不会被复用或执行。

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
