# osdk — 一站式 SDK 管理器

**简体中文** · [English](README.md) ·
[官方网站](https://lejunyang.github.io/one-sdk/) ·
[中文文档](https://lejunyang.github.io/one-sdk/) ·
[English docs](https://lejunyang.github.io/one-sdk/en/)

osdk 是一个跨平台（Windows / macOS / Linux）CLI，用统一方式管理多种语言 SDK
及其版本：**Node.js、npm、pnpm、Yarn、Java、Maven、Gradle、Kotlin、Python、
Rust、Go、Deno、Bun**。它把现有单语言管理器（nvm/fnm/uv/SDKMAN/rustup）
通常无法同时提供的四类能力组合在一起：

1. **跨版本内容去重。** 基于 BLAKE3 的内容寻址存储只保留一份相同文件；各安装
   版本通过硬链接、reflink 或复制从存储中物化。两个 Node.js 次版本若共享文件，
   磁盘只保存一次。
2. **统一下游包缓存。** npm/pnpm/Yarn/pip/Go/Cargo/Gradle 的全局缓存统一指向
   共享目录，让不同项目和 SDK 版本复用已下载依赖。
3. **多源与最快镜像自动选择。** 每个 SDK 都提供官方源和可用镜像；osdk 会探测
   速度并选择最快来源，元数据或下载失败时自动切换。也支持添加自定义源或固定
   指定来源。
4. **不可变模型快照。** Hugging Face 仓库会解析为精确 commit，按文件断点续传
   与验证 SHA-256，复用同一个 CAS，并在 `osdk.lock` 的独立 `[models]` 中记录。

## 安装

Linux 或 macOS 可通过 gh-proxy 下载并执行最新版一键安装脚本：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

Windows PowerShell：

```powershell
$env:OSDK_DOWNLOAD_BASE_URL = "https://gh-proxy.com/https://github.com"
irm https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

以上示例既通过 gh-proxy 获取 Raw 安装脚本，也通过
`OSDK_DOWNLOAD_BASE_URL` 代理脚本后续下载的 GitHub Release 二进制与
`SHA256SUMS`。

两个安装器都会使用 Release 中的 `SHA256SUMS` 校验归档。需要传入自定义参数时，
先下载脚本再执行：

```bash
curl -sSfL \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh \
  -o install.sh
sh install.sh \
  --base-url https://gh-proxy.com/https://github.com \
  --version 0.1.0 \
  --install-dir "$HOME/bin"
```

```powershell
Invoke-WebRequest `
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1
.\install.ps1 `
  -BaseUrl https://gh-proxy.com/https://github.com `
  -Version 0.1.0 `
  -InstallDir "$HOME\bin"
```

Unix 使用 `sh install.sh --help`，PowerShell 使用
`Get-Help .\install.ps1 -Detailed` 查看完整参数。安装器支持
`OSDK_VERSION`、`OSDK_BIN_DIR`、`OSDK_REPOSITORY`、
`OSDK_DOWNLOAD_BASE_URL` 和 `OSDK_TARGET` 环境变量覆盖。

### 从源码构建

需要 Rust。中国大陆建议使用镜像，避免 `static.rust-lang.org` 访问缓慢：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y
cargo build --release        # 二进制：target/release/{osdk,osdk-shim}
```

维护者发布新版本时，需要先更新 `workspace.package.version`，再向 `main` 推送一条
提交信息明确包含 `[publish]` 的 commit。普通提交不会触发二进制发布 workflow。

## 快速开始

```bash
osdk install node@20            # 安装，并自动选择最快镜像
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk use -g node@20             # 安装 + 设为全局默认 + 生成 shim
osdk use node@18                # 固定到当前项目（osdk.toml）
node --version                  # 通过 shim 执行当前生效版本

# Shell 激活（shim 的替代方案：按目录更新 PATH 与环境变量）
eval "$(osdk activate bash)"    # 加入 ~/.bashrc；也支持 zsh|fish|powershell
# 后续移除 hook，并恢复当前 shell 的 PATH/环境变量：
eval "$(osdk deactivate bash)"

# 不可变 Hugging Face 模型快照
osdk model pull qwen25 \
  hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors'
osdk model path qwen25
```

PowerShell 激活会防止命令查找回调重入。Windows shim 也会显式通过 `ComSpec`
启动 `.cmd` / `.bat` 工具，保持批处理参数、标准输入输出和退出码。

下载会重试瞬时故障，并使用 HTTP `Range` / `If-Range` 安全续传经过验证的部分文件。
成功联网一次后，可使用 `--offline` 完全从 osdk 缓存解析元数据并重新安装归档。

osdk 会从当前目录向上查找项目版本文件：`osdk.toml`、兼容 asdf 的
`.tool-versions`，以及生态原生文件（`.nvmrc`、`.node-version`、
`.python-version`、`.java-version`、`go.mod`、`rust-toolchain.toml`）。Node 还会
把 `package.json#engines.node` 和 `devEngines.runtime` 解析为 npm semver range。
跨目录统一优先级为：`osdk.toml` > `.tool-versions` > `.nvmrc` >
`.node-version` > `package.json` > 用户全局配置。

## 模型快照

`osdk model pull` 把模型作为多文件仓库快照管理，而不是 SDK 单归档：

```bash
export HF_TOKEN=... # 私有或 gated 仓库可选
osdk model pull qwen25 hf:Qwen/Qwen2.5-7B-Instruct@main
osdk model list
osdk model verify qwen25
osdk model path qwen25
osdk model remove qwen25
```

branch/tag 会先解析为不可变 commit。每个选中文件支持断点续传和 SHA-256 验证，
随后进入共享 CAS 并原子物化。精确仓库、commit、endpoint、variant、文件大小和
SHA-256 会写入 `osdk.lock` 顶层 `[models]`；token 和短期签名下载 URL 不会落盘。
可重复使用 `--include` / `--exclude` 选择文件，使用 `--variant` 标记格式或量化，
并用 `--offline` 从已缓存 metadata 与文件重建快照。

认证支持 `OSDK_HF_TOKEN`、`HF_TOKEN` 和 `HUGGING_FACE_HUB_TOKEN`；
Authorization 只附加到配置的 Hugging Face endpoint 请求。

## Node 工作流

解析 lock 时可覆盖 Node artifact 架构：

```bash
osdk lock node@20 -o arch=arm64
```

目标架构会写入对应的平台 lock 区段。osdk 暂无仅下载模式，因此安装和执行会拒绝
跨架构 artifact。可用 `-o corepack=true` 启用 Corepack，或在
`[settings.node]` 中持久配置 `corepack = true`；osdk 只调用该 Node 安装自带的
Corepack，启用 shim 失败时会回滚这次安装。

在受管 Node 版本之间迁移可移植的全局 npm 包：

```bash
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

默认仅输出演练计划；npm 自身和标记了原生构建或安装脚本的包会跳过。`--apply`
只调用目标 Node 的受管 npm，并把目标 bin 目录放在 `PATH` 首位；失败时恢复目标
原有的全局包集合。

## Python 实现与 Catalog

简写仍表示 CPython：

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
osdk python find pypy-3.11
```

完整 identity 为 `python@<implementation>-<version>+<variant>`；实现和变体会写入
`osdk.lock`，普通与 free-threaded CPython 可以并存。`python find` 按受管、
`PATH`、系统解释器的顺序输出。

内置 known-good catalog 固定来自 uv 的指定 commit，每个条目都有 SHA-256。配置
更完整的内网或刷新 catalog 时必须同时提供：

```toml
[settings.python]
catalog_url = "https://example.test/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

也支持本地路径和 `file://` URL。新 catalog 只有在精确 digest、schema、实现、
变体和每个 artifact checksum 全部通过后才替换 last-good 缓存；失败先回退
last-good，再回退内置 catalog。预发布策略默认是 `if-explicit`：

```bash
osdk --prerelease never install python@3.15.0rc1
osdk --prerelease allow install python@latest
```

除非策略为 `allow`，`latest` 不会选择预发布版本；`never` 也会拒绝显式 RC。

## Java 运行时与 JVM 工具

Java 默认安装 Temurin JDK，package type 会显式写入 lock：

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu -o package-type=jdk
```

JRE identity 带 `jre-` 前缀，所以同一 Java 版本的 JDK 与 JRE 可以并存。Foojay
结果会按运行时类型和 host libc 过滤。内置 Temurin LTS catalog（8、11、17、21、
25）在空缓存离线模式也能解析；已有 verified lock artifact 无需访问 Foojay 即可
安装。需要时可配置兼容 Foojay 的镜像或静态 endpoint：

```toml
[settings.java]
catalog_url = "https://mirror.example.test/disco/v3.0/packages"
```

Maven、Gradle 和 Kotlin 是独立 candidate，不是 Java option：

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

它们拥有独立安装 identity、shim 和内置稳定候选，并分别验证上游 SHA-512 或
SHA-256。所有工具统一使用离线/cache/lock pipeline，安装时不会调用用户全局 Java。

## Rust 生命周期管理

Rust 仍委托 rustup，但每个生命周期命令都会注入 osdk 隔离的 `RUSTUP_HOME` 和
`CARGO_HOME`：

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust component remove rustfmt --toolchain stable
osdk rust component list --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust target remove x86_64-pc-windows-gnu --toolchain stable
osdk rust target list --toolchain stable
osdk rust check --repair
```

`check` 输出隔离 rustup 的更新状态；`--repair` 对齐真实 rustup 工具链和 osdk
marker。目录选择默认继续使用 osdk 项目 pin；rustup override 兼容必须显式执行：

```bash
osdk rust override import [path]
osdk rust override export [path]
osdk rust toolchain link local-dev /absolute/toolchain
```

import 把隔离 rustup override 写入 `osdk.toml`；export 把当前 osdk pin 写回隔离
rustup。linked toolchain 会暴露本地 `bin`，但禁止作为可复现远程 artifact 写入
lock。

## 项目包管理器

osdk 会读取 `package.json#packageManager` 和 `devEngines.packageManager` 中
Corepack 风格的精确版本：

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "pnpm@9.15.0"
}
```

支持 `npm`、`pnpm`、`yarn`。优先级为 `osdk.toml [tools]` >
`packageManager` > `devEngines.packageManager`。不带版本、非法 manager、URL
与 hash/build 后缀都会明确失败。

npm backend 独立安装 npm registry 的 `npm` 包并验证 npm SRI：

```bash
osdk install npm@11.5.2
osdk uninstall npm@11.5.2
```

选择 npm/pnpm/Yarn 会自动加入受管 Node。运行时 PATH 固定为包管理器 bin 在前、
精确受管 Node 在后，绝不调用用户全局 Node。lock 保存两者精确版本并支持不查
metadata 的离线重装。

## 可复现项目与命令执行

把当前项目解析为精确、按平台区分的版本：

```bash
osdk lock                         # 写入/合并 osdk.lock
osdk install                      # 使用当前平台对应的 lock
osdk outdated                     # 对比已安装版本与当前解析结果
osdk upgrade                      # 安装当前解析版本并刷新 lock
```

`osdk.lock` 为 Linux、macOS 和 Windows 保存独立区段，其中包含原始请求、精确解析
版本、backend 参数，以及工具安装后使用的精确 artifact URL、文件名、已验证校验和
与 Sigstore 认证证据。无参数安装直接使用锁定的 artifact identity，不重新查询上游
release registry。

lock 中的证据是审计记录，不是绕过信任校验的捷径：启用 attestation 的锁定重装
仍会用缓存 bundle 重新验证缓存 artifact。显式执行
`osdk install node@20` 时仍以显式请求为准。

无需修改项目 pin，即可在指定的受管工具环境中运行命令：

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool python@3.12 -- python -c "print('ok')"
```

生成 Shell 补全：

```bash
osdk completions bash|zsh|fish|powershell
```

在用户配置中定义可复用的版本别名：

```bash
osdk alias set node default 20
osdk alias set node maintenance default
osdk alias list node
osdk use node@maintenance
osdk alias unset node maintenance
```

别名可以指向另一个别名；循环引用以及 `latest`、`lts`、`system` 等保留名称会被
拒绝。工具名别名会规范化，例如 `osdk alias set nodejs default 20` 会把别名保存
到 `node` 下。

## 下载源与镜像

```bash
osdk source list node                 # 查看源及固定状态
osdk source test node                 # 探测速率并输出排名
osdk source pin node tuna             # 固定使用指定源
osdk source add node --id mycorp \
  --download-url https://mirror.corp/node/ \
  --index-url    https://mirror.corp/node/index.json
osdk --source official install go@1.22 # 单次覆盖
```

Go 内置 `go.dev`、阿里云和 `golang.google.cn` 三个来源。

`github:owner/repo` 的 GitHub Releases API 元数据、Release 资产、Raw 文件、校验和/
签名文件和 attestation bundle 都遵循同一 source 顺序。内置 `ghproxy` 会把这些
GitHub URL 全部改写到 `https://gh-proxy.com/`。GitHub token 只发送给官方
`api.github.com`，不会转发给第三方代理。

## 内容去重与缓存

```bash
osdk doctor                     # 目录、同文件系统检查、链接模式、backend
osdk prune                      # 清理未被任何安装引用的存储对象
osdk cache dir                  # 共享缓存和存储目录
osdk cache env                  # 下游包管理器缓存环境变量
```

## 语言（i18n）

osdk 支持中文和英文。它会根据 locale 自动选择语言
（`LC_ALL` / `LC_MESSAGES` / `LANG`，例如 `zh_CN.UTF-8` 选择中文），并本地化
全部消息、错误和 `-h` / `--help`。覆盖优先级从高到低为：

```bash
osdk --lang zh install node@20   # 单次命令参数
export OSDK_LANG=zh              # 环境变量
# 或在 config.toml 中设置：[settings]\n lang = "zh"
```

## 目录（可通过环境变量覆盖）

| 用途 | Linux 默认位置 | 环境变量 |
| --- | --- | --- |
| 数据（安装） | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| CAS 存储 | `<data>/store` | `OSDK_STORE_DIR` |
| SDK 安装目录 | `<data>/installs` | `OSDK_INSTALL_DIR` |
| 下载缓存 | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| 配置 | `~/.config/osdk/config.toml` | `OSDK_CONFIG_DIR` |

内容存储和安装目录位于同一文件系统时才能使用硬链接；跨文件系统时 osdk 自动回退
到复制，`osdk doctor` 会给出警告。

## 各 SDK 的获取方式

| SDK | 获取方式 |
| --- | --- |
| Node.js | nodejs.org 官方预编译归档，验证 `SHASUMS256` |
| Go | go.dev/dl JSON 索引，逐文件 SHA-256 |
| Python | 静态 PBS release 索引 + Astral release 镜像，验证 `SHA256SUMS`，不依赖 GitHub API |
| Java | Foojay Disco API（默认 Temurin），支持多个发行版 |
| Rust | 隔离 rustup bootstrap 和 toolchain home，选择镜像并验证 SHA-256 |
| pnpm | 官方 npm 平台包，验证 npm SRI |
| Yarn | `yarn` / `@yarnpkg/cli-dist` npm 包，验证 npm SRI |
| Deno | 官方 `@deno/<platform>` npm 包，验证 npm SRI |
| Bun | 官方 `@oven/bun-<platform>` npm 包，验证 npm SRI |
| npm | 独立 npm registry 包，验证 npm SRI |
| `github:owner/repo` | 任意 GitHub Release；自动匹配 host asset，支持归档和裸二进制 |

### GitHub Release 工具

安装任意通过 GitHub Releases 发布的工具：

```bash
osdk use -g github:sharkdp/fd          # 最新 release，自动选择 host asset
osdk install github:cli/cli@2.62.0     # 指定 tag
osdk list-remote github:sharkdp/fd     # 可用 release tag
```

只有通用 `github:owner/repo` backend 需要 GitHub Releases API。设置
`GITHUB_TOKEN` 或 `OSDK_GITHUB_TOKEN` 可提高直连 API 限额。API 元数据、Raw
文件、Release 资产、校验文件和 attestation bundle 都可通过 gh-proxy 失败转移，
且 token 不会被转发给代理。
Release 列表支持分页，最多读取 1,000 个 release。

可用显式 option 覆盖启发式 asset 选择：

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1

osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

regex/template 必须恰好命中一个 asset。`bin`/`bins` 从归档选择文件，`rename`
要求只选一个 binary。文件缺失会删除整个安装，不留下 complete marker；Windows
会规范 `.exe`。

固定 digest 的静态 catalog 可完全绕过 Releases API：

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

schema 1 asset 包含 `name`、`url`、`checksum`、`os`、`arch` 和可选 `libc`。
catalog digest、最终 asset 与规则都会写入 lock。

## 预发布通道

统一的 `--prerelease never|if-explicit|allow` 策略适用于 Python、Bun、Deno 和
GitHub Release。默认值是 `if-explicit`：

```bash
osdk install bun@canary
osdk install deno@beta
osdk install github:owner/repo@1.2.0-beta.1
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

显式 `canary`、`nightly`、`beta` 会把 npm dist-tag 或匹配的 GitHub prerelease
tag 解析为精确版本。`never` 拒绝所有预发布；只有 `allow` 才允许
latest/prefix/range 隐式选择预发布。远程列表默认仍只显示稳定版。lock 同时保留
原始 channel 和精确版本，因此 dist-tag 消失也不影响离线复现。

## 离线模式

成功的在线元数据请求和下载归档会按 URL 与工具版本缓存。后续命令可以完全禁止
网络访问：

```bash
osdk install bun@1.3.14
osdk uninstall bun@1.3.14
osdk --offline install bun@1.3.14
```

离线缓存缺失会明确失败，不会静默联网。离线模式下也会禁用 source 探测和刷新。

能获得可信 key 时，签名验证默认开启。仅在明确需要时才设置
`OSDK_VERIFY_SIGNATURES=false`。设置 `OSDK_REQUIRE_CHECKSUMS=true`（或传入
`--require-checksums`）可拒绝任何既没有上游校验和，也没有 lock/cache receipt
可验证 SHA-256/SHA-512/BLAKE3 的 artifact。

通用 `github:owner/repo` backend 还支持 GitHub Artifact Attestations：

```bash
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

同一策略也可通过 `settings.attestations` 或
`OSDK_ATTESTATIONS=off|if-available|required` 配置，默认是 `off`。
`if-available` 允许 release 没有 attestation，但发现格式错误、身份不匹配或密码学
验证失败的 bundle 时一定失败；`required` 在没有 bundle 时也会失败。

已验证 bundle 按仓库和 artifact SHA-256 缓存，因此 `--offline` 和锁定重装无需
信任 lockfile 中的证据，也能重新验证。

验证使用内置 Sigstore public-good trust root，检查 Fulcio certificate chain 和
SCT、GitHub Actions OIDC issuer 与仓库、artifact signature、DSSE subject digest、
Rekor body 一致性和 signing time。它还会使用受信任日志 key 验证 Rekor Signed
Entry Timestamp（SET）、signed checkpoint、proof 的 root/tree-size 绑定和
canonical log entry 的 Merkle path。全部 proof 检查均可离线完成；proof 缺失、
篡改或不匹配都会失败。新 receipt/lock 证据使用 `sigstore-bundle+rekor` kind；
旧 `sigstore-bundle` 证据仍可读取，但绝不会成为跳过信任校验的捷径。

## 架构

- `crates/osdk-core`：库，包括 `Backend` trait、统一管线
  （download → verify → extract → CAS ingest → materialize）、CAS 存储和链接模式、
  source 选择、配置/目录、shim 和 shell 激活。
- `crates/osdk-cli`：`osdk` 二进制。
- `crates/osdk-shim`：轻量启动器；每个 shim 从当前工作目录解析生效版本，并执行
  对应真实二进制。

## 开发

离线 backend contract 是主要正确性门禁。它对每个内置 backend 和 generic
GitHub 运行同一套 resolve → install → execute → uninstall 断言，并让 registry
中的真实 backend 消费本地 locked fixture。故障注入覆盖 403、429、5xx、timeout、
连接中断、畸形 metadata、stale cache、下载中断、并发安装、失败 marker 清理、
损坏 receipt/manifest、跨文件系统 copy fallback，以及 shim 的
stdin/stdout/stderr、退出码、递归和冲突。定时公网 smoke 只监测上游漂移，不承担
主要正确性证明。

```bash
cargo test --workspace
cargo clippy --workspace --all-targets   # CI 使用 -D warnings
cargo fmt --all --check

# 在 Windows 构建二进制后运行运行时矩阵：
pwsh -File scripts/windows-runtime-smoke.ps1 -BinDir target/debug

# 同时从 Linux 交叉验证 Windows cfg：
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y mingw-w64
cargo clippy --locked --workspace --all-targets \
  --target x86_64-pc-windows-gnu -- -D warnings

# 通过固定版本并校验 SHA-256 的 Wine 执行完整 Windows GNU 测试：
./scripts/windows-wine-tests.sh
```

CI（`.github/workflows/ci.yml`）在 Ubuntu、macOS 和 Windows 上运行格式检查、
Clippy 与测试。独立的原生 macOS terminal 门禁会同时在 Apple Silicon 和 Intel
runner 上执行交互 PTY 合约。Windows runner 还会在临时隔离状态下、完全离线地
执行 `.cmd`、PowerShell、Git Bash shim、PowerShell 激活/撤销、symlink 权限回退、
真实 NTFS volume detection、stdin/stdout/stderr、参数和退出码，并覆盖空格、中文路径以及
超过传统 260 字符限制的受管 SDK 状态目录；可执行文件和工作目录保持在 Shell
自身的进程启动长度限制内。
`github:owner/repo` 等带命名空间的 backend ID 也在覆盖范围内，确保缓存、锁、
安装和解压临时目录在 Windows 上均为合法路径。Linux job 会先交叉 lint 所有
`#[cfg(windows)]` 路径，再通过固定版本并校验 SHA-256 的 Wine 执行完整 Windows
GNU workspace；原生 Windows/MSVC runner 仍是最终平台门禁。另有 Rust 1.88 job
检查声明的最低 Rust 版本与锁定依赖图。CI job 和 Windows runtime matrix 都有
硬超时；runtime 脚本会为每个 Shell 合约输出独立日志分组，阻塞点不再无限等待。

## 许可证

MIT
