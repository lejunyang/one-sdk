# 详细功能

本页按日常工作流介绍 osdk 的命令与配置。

## 安装与并发

版本请求统一使用 `<工具>@<版本>`：

```bash
osdk install node@20
osdk install python@3.12
osdk --jobs 4 install node@20 go@1.22 python@3.12
```

`latest`、`stable`、`lts` 和版本前缀会解析为当前可用的精确版本。不同工具可以
并发安装；下载失败会重试，服务器支持时会安全续传。

部分后端支持额外参数：

```bash
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk install java@21 -o distribution=zulu
```

## 项目版本与全局版本

```bash
osdk use node@20       # 写入当前项目 osdk.toml
osdk use -g node@20    # 写入用户全局配置
osdk current           # 查看当前目录生效版本
osdk where node        # 输出安装目录
```

项目配置示例：

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
```

osdk 从当前目录向上查找最近的配置，因此同一仓库的子目录可以继承工具版本。

### Node 项目版本、架构与 Corepack

Node 还会读取 `package.json#engines.node` 和 `devEngines.runtime`，并按 npm
semver range 选择最高匹配稳定版。跨目录统一优先级为：
`osdk.toml` > `.tool-versions` > `.nvmrc` > `.node-version` >
`package.json` > 用户全局配置。无效 range 会明确报错。

```bash
osdk lock node@20 -o arch=arm64
osdk install node@20 -o corepack=true
```

`arch=x64|arm64|x86|arm` 会写入目标平台 lock 区段。osdk 暂无仅下载模式，因此
安装会拒绝不能在当前 host 执行的架构。Corepack 也可通过
`[settings.node] corepack = true` 配置；osdk 只调用目标 Node 自带的 Corepack，
失败时不会保留完成 marker。

全局包迁移默认只生成计划：

```bash
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

迁移排除 npm 自身和带原生构建或安装脚本的包。apply 只使用受管 npm，失败时恢复
目标版本原有的全局包集合。

## Python 多实现、变体与 Catalog

`python@3.14` 仍是 CPython 简写。完整请求为：

```bash
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

实现、精确版本和变体都写入 lock；普通 CPython 与 free-threaded/debug 变体使用
不同 identity 并可共存。发现本机解释器：

```bash
osdk python find
osdk python find pypy-3.11
```

输出按 managed、PATH、system 分层并去重。内置 known-good catalog 固定来自 uv
download metadata 的指定 commit，所有条目都有 SHA-256。自定义远程或本地
catalog 必须同时配置 digest：

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

只有 digest、schema 和每条 artifact checksum 全部通过后才更新 last-good；失败
不会破坏旧缓存。预发布默认 `if-explicit`，`latest` 不会意外选择 RC：

```bash
osdk --prerelease never install python@3.15.0rc1
osdk --prerelease allow install python@latest
```

## Java JDK/JRE 与 JVM 工具

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu
```

`package-type=jdk|jre` 会进入 lock；JRE 使用 `jre-<version>` identity，因此可与
同版本 JDK 并存。Foojay package 会按 type 与 host libc 过滤。内置 Temurin LTS
版本 8/11/17/21/25 支持空缓存离线解析，已有 locked artifact 在 Foojay 不可用时
仍可安装。

可把 metadata 切换到兼容 Foojay 的镜像或静态 endpoint：

```toml
[settings.java]
catalog_url = "https://mirror.example.test/disco/v3.0/packages"
```

JVM 工具以独立 backend 管理：

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

三者分别拥有独立版本、目录和 shim，并验证 Apache SHA-512、Gradle SHA-256 与
Kotlin SHA-256。离线安装和 lock 行为与其他 backend 一致。

## Rust 组件、目标与工具链

以下命令全部使用 osdk 隔离的 `RUSTUP_HOME` / `CARGO_HOME`：

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust component remove rustfmt --toolchain stable
osdk rust component list --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust target remove x86_64-pc-windows-gnu --toolchain stable
osdk rust target list --toolchain stable
osdk rust check --repair
```

`check` 解析隔离 rustup 状态；repair 会补建缺失 marker 并清理没有真实工具链的
marker。osdk 项目配置是默认目录选择机制；需要与 rustup override 互操作时必须
显式 import/export：

```bash
osdk rust override import ./repo
osdk rust override export ./repo
osdk rust toolchain link local-dev /absolute/toolchain
```

linked toolchain 可本地执行，但会被可复现 lock 明确拒绝。

## packageManager 与独立 npm

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "npm@11.5.2"
}
```

osdk 支持 `packageManager` 和 `devEngines.packageManager` 中的
`npm|pnpm|yarn@精确版本`。优先级为 `osdk.toml [tools]` >
`packageManager` > `devEngines.packageManager`。无版本、URL/hash 后缀与非法
manager 都会明确报错。

独立 npm backend 从 registry 下载 `npm` 包并验证 SRI。选择任一包管理器都会
自动加入受管 Node；PATH 中 manager bin 在前、目标 Node 在后，所以 launcher
不会调用用户全局 Node。两个精确版本都会写入 lock 并支持离线重装。

## 项目配置信任

项目配置中的 `[tools]` 版本固定和 `[aliases]` 只是安全数据，无需信任即可读取。
其他项目级配置可能改变执行行为或下载源，osdk 会在读取任何这类字段前要求显式
信任：

```bash
osdk --yes trust                 # 信任最近的 osdk.toml
osdk --yes trust ./osdk.toml     # 信任指定文件
osdk trust list                  # 查看有效或已失效记录
osdk untrust                     # 取消最近项目配置的信任
```

信任记录同时绑定规范路径和规范化 TOML 内容的 BLAKE3 哈希。文件内容变化或仓库
移动后信任自动失效；软链接按真实目标记录，路径穿越不会产生另一个身份。CI 可用
`OSDK_TRUSTED_CONFIG_PATHS` 提供由系统路径分隔符连接的已审阅文件或目录，而不
持久化本地信任。trust 命令只加载用户配置，因此未信任项目不能影响自己的批准。

## Shim 与 Shell 激活

`osdk use` 会生成 shims，执行 `node`、`python` 等命令时，shim 根据当前目录
解析项目或全局版本。

也可以安装 shell hook，让 osdk 在切换目录时更新 `PATH` 和工具专属环境变量：

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

撤销当前 shell 的激活状态：

```bash
eval "$(osdk deactivate bash)"
```

安装目录发生变化后可运行 `osdk reshim` 重新生成全部 shims。

## 可复现锁文件

```bash
osdk lock
osdk install
osdk outdated
osdk upgrade
```

`osdk.lock` 为 Linux、macOS 和 Windows 保存独立区段，记录：

- 原始版本请求与精确解析版本；
- 后端选项；
- 实际下载 URL 和文件名；
- 已验证的校验和；
- 已验证的 GitHub attestation 证据。

无参数 `osdk install` 优先使用当前平台锁定的归档身份，不重新查询上游。锁文件
中的证据是审计记录，不是跳过验证的理由；重新安装仍会验证缓存的归档。

## 临时执行

无需修改项目 pin，就能以指定工具环境运行命令：

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool python@3.12 -- python -c "print('ok')"
osdk exec --tool node@20 --tool pnpm@latest -- pnpm install
```

## 版本别名

```bash
osdk alias set node maintenance 20
osdk alias set node default maintenance
osdk alias list node
osdk use node@default
osdk alias unset node maintenance
```

别名可以引用另一个别名。osdk 会拒绝循环引用，以及 `latest`、`lts`、`stable`
等保留名称。

## 多源与镜像选择

```bash
osdk source list node
osdk source test node
osdk source pin node tuna
osdk source unpin node
```

默认情况下，osdk 对候选源执行轻量探测并缓存排名，在元数据请求或归档下载失败
时继续尝试后续来源。

添加企业或私有镜像：

```bash
osdk source add node \
  --id mycorp \
  --download-url https://mirror.example.com/node/ \
  --index-url https://mirror.example.com/node/index.json

osdk --source mycorp install node@20
osdk source remove node mycorp
```

Go 内置 `go.dev`、阿里云和 `golang.google.cn` 三个来源。
`github:owner/repo` 会让 GitHub Releases API、Release 资产、Raw 文件、校验和/
签名文件和 attestation bundle 统一遵循 source 顺序。内置 `ghproxy` 会把这些
GitHub URL 全部改写到 `https://gh-proxy.com/`；`GITHUB_TOKEN` 只发送给官方
`api.github.com`，不会转发给第三方代理。

## 内容去重

归档验证和解压后，每个文件会按 BLAKE3 内容哈希写入共享存储。安装目录不再
保留重复副本，而是按能力使用：

1. 硬链接；
2. reflink；
3. 普通复制。

```bash
osdk doctor
osdk prune --dry-run
osdk --yes prune
```

`prune` 只清理没有任何安装版本引用的存储对象。

## 下游包缓存

osdk 为常见包管理器提供共享缓存环境，避免 SDK 版本之间重复下载依赖：

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
```

覆盖 npm/pnpm/Yarn、pip、Go、Cargo 和 Gradle 等生态。`cache clean` 清理下载
归档，不删除已安装 SDK 或内容存储。

`uninstall`、`cache clean` 和非演练 `prune` 统一使用确认策略：交互终端显示中文
或英文提示；非交互环境不会等待 stdin，而是明确失败。CI 和脚本应传
`--yes`、设置 `OSDK_YES=true`，或在 `[settings]` 中配置 `yes = true`。
`--quiet` 只关闭进度输出，不代表同意删除。

## 离线模式

成功的元数据请求和下载包会按 URL 与工具版本缓存：

```bash
osdk install bun@latest
osdk uninstall bun@1.3.14
osdk --offline install bun@1.3.14
```

离线模式禁止网络请求。缓存缺失会明确报错，不会静默联网；源探测和源刷新也会
停止。

## 完整性、签名与 Attestation

osdk 会使用后端提供的 SHA-256、SHA-512、BLAKE3 或 npm SRI 验证归档。

```bash
osdk --require-checksums install node@20
export OSDK_REQUIRE_CHECKSUMS=true
```

严格模式会拒绝没有可验证校验和的归档。签名验证默认开启，可通过
`OSDK_VERIFY_SIGNATURES=false` 显式关闭。

GitHub Release 通用后端还支持 GitHub Artifact Attestations：

```bash
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

策略也可写入 `settings.attestations` 或环境变量 `OSDK_ATTESTATIONS`：

- `off`：默认，不查询 attestation；
- `if-available`：允许没有 attestation，但发现的无效证明一定失败；
- `required`：必须存在且通过验证。

当前实现验证 Fulcio 证书链、SCT、GitHub Actions OIDC 颁发者与仓库、归档
签名、DSSE subject digest、Rekor body 一致性和签名时间。上游 `sigstore`
0.14 尚不验证 Rekor Merkle inclusion proof 或 Signed Entry Timestamp。

## 任意 GitHub Release 工具

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

osdk 会按当前操作系统与架构选择匹配的 Release asset，并处理归档或单文件
二进制。可设置 `GITHUB_TOKEN` 或 `OSDK_GITHUB_TOKEN` 提升直连 API 限额；
API 元数据、Raw 文件、Release 资产、校验文件和 attestation bundle 均可回退
ghproxy，且不会把 token 转发给代理。

Release API 支持分页（上限 1,000）。复杂 release 可用显式规则：

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1
osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

0 或多命中都会失败。多 binary 物化是原子的，Windows 自动规范 `.exe`。

静态 catalog 模式完全不访问 GitHub API：

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

schema 1 asset 包含 URL、checksum、os、arch 和可选 libc；digest、最终 asset
与规则都会写入 lock。

## 预发布策略与通道

```bash
osdk install bun@canary
osdk install deno@beta
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

`if-explicit` 是 Python、Bun、Deno 和 GitHub 的默认策略。显式
`canary|nightly|beta` 映射 dist-tag 或 prerelease tag；lock 保存原始 channel
和精确版本。`never` 拒绝所有预发布，只有 `allow` 才允许 latest/prefix/range
隐式选择预发布。默认列表不混入预发布。

## 声明式后端

除内置后端外，osdk 可以从隔离的用户配置/数据插件目录加载 schema 1 TOML
后端。声明式后端定义版本索引、平台映射、下载模板、可执行文件和校验和规则，
仍然走同一套验证、解压和内容存储管线。

## 国际化

CLI 自动根据 `LC_ALL`、`LC_MESSAGES` 或 `LANG` 选择中文或英文。覆盖优先级：

```bash
osdk --lang zh install node@20
export OSDK_LANG=zh
```

也可以写入用户配置：

```toml
[settings]
lang = "zh"
```

## 可靠性合约

CI 的本地 backend contract 对全部内置 backend 与 generic GitHub 运行统一的
resolve/install/execute/uninstall 生命周期，并用本地 HTTP fixture 注入 403、
429、5xx、timeout、断流、畸形 metadata 和 stale cache。它还验证并发安装锁、
失败后无 complete marker、损坏 receipt/manifest、跨文件系统 copy fallback、
shim I/O/退出码/递归/命令冲突。公网 live smoke 只用于发现上游漂移。

Windows runner 还会离线执行 `.cmd`、PowerShell 和 Git Bash wrapper、真实
activation/deactivation、symlink 权限回退、NTFS volume detection、
stdin/stdout/stderr、参数、退出码，以及含空格、中文和长路径的场景。Linux
cross-Clippy 仍是所有 Windows cfg 路径的必需门禁。

## 补全与诊断

```bash
osdk completions bash
osdk completions zsh
osdk completions fish
osdk completions powershell
osdk doctor
osdk config path
osdk config list
```

## 目录覆盖

| 用途 | Linux 默认位置 | 环境变量 |
| --- | --- | --- |
| 数据根目录 | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| 内容存储 | `<data>/store` | `OSDK_STORE_DIR` |
| SDK 安装 | `<data>/installs` | `OSDK_INSTALL_DIR` |
| 下载缓存 | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| 配置 | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

内容存储与安装目录位于同一文件系统时才能使用硬链接。`osdk doctor` 会报告当前
链接模式和跨文件系统问题。
