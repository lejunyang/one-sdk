# GitHub Release 工具

```text
github:owner/repo[@VERSION]
```

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

## Asset 选择选项

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

`asset-regex` 与 `asset-template` 互斥。未给规则时，osdk 从文件名识别 OS、架构与
libc（Rust triple、goreleaser 的 `linux_amd64` / `darwin_all`、`win64` 等常见写法都能
识别），只在与本机（或 `os=` / `arch=` / `libc=` 指定的目标）相符的 asset 中挑选：

- 永远不会退而选择其他 OS 的产物；musl 系统上不会选 glibc 产物；
- checksum、signature、SBOM、`.deb` / `.msi` 等安装包、源码包以及 osdk 无法解压的
  `.gz` / `.bz2` 单文件压缩都会被排除；
- 同时存在时，Linux 优先静态 musl 构建，Windows 优先 MSVC 构建，并优先 archive 与
  非 debug 构建；
- Windows arm64 与 Apple Silicon 上，release 没有原生构建时会退而使用系统可仿真运行
  的 x64 构建；显式给出 `arch=` 时不做这种退让。

没有合适的 asset，或多个不同产物同样合适（例如 `tool` 与 `toolctl`），都会失败并提示
用 `asset-regex` 指定；同一构建的 `.tar.gz` 与 `.zip` 不算歧义。未知 archive 后缀按裸
二进制处理，Windows 自动补 `.exe`。
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

## 静态 catalog

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

catalog 可为 HTTP(S)、`file://` 或普通本地路径；schema 1 的每个 asset 必须包含
`name`、`url`、`checksum`、`os`、`arch`，`libc` 可选，artifact URL 必须 HTTP(S)。
HTTP catalog 在线时以摘要缓存，offline 从缓存读取；本地文件可直接离线读取。

配置 `catalog-url` 后，`latest` 表示 **catalog 内的最新版本**，显式版本也必须在其中；
不会再查询 GitHub 上的新 Release。`osdk use` 会保留已有工具选项，因此只改 `@VERSION`
不会退出 catalog 模式。浮动版本解析会提示正在使用固定 catalog；缺失版本的错误也会说明
这一限制。要恢复在线 Release 发现，请在对应项目 `osdk.toml` 或用户 `config.toml` 的
工具条目中同时移除 `catalog-url` 和 `catalog-sha256`，再执行 `osdk use`。
清理下载缓存不会改变这个配置。

## GitHub 访问、回退与 token

token 优先级是 `OSDK_GITHUB_TOKEN`、`GITHUB_TOKEN`、`GH_TOKEN`。Authorization
只发给精确的 `api.github.com`，绝不转发给代理。Releases API 最多读 10 页、每页
100 条。匿名限流时可从公开 Atom feed 和 expanded-assets HTML 尽力发现近期公开
release；不能替代完整历史。

内置 `github` 与 `ghproxy` source 会一致覆盖 API、release asset、Raw/Gist、
checksum/signature 文件和 attestation bundle，并按排序失败转移。代理前会先规范化为
官方 URL，确保 pin、缓存和身份不随代理形式漂移。
