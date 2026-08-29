# osdk — 一站式 SDK 管理器

**简体中文** · [English](README.md) ·
[中文文档](https://lejunyang.github.io/one-sdk/) ·
[版本发布](https://github.com/lejunyang/one-sdk/releases)

osdk 为 Windows、macOS 和 Linux 项目提供一个统一管理语言运行时、包管理器、
开发工具与模型快照的 CLI。你可以用它：

- 用一套命令安装并切换完整的项目工具链；
- 为团队和 CI 保存区分平台且可复用的项目锁定结果；
- 自动选择响应更快的 SDK 镜像和依赖 Registry；
- 在网络不可用时复用已下载的元数据与产物；
- 像管理开发工具一样管理 Hugging Face 和 ModelScope 模型快照；
- 在不改变状态的前提下检查 Docker、containerd、Buildx 及其原生缓存；
- 使用中文或英文查看存储、缓存、生效版本和环境诊断。

从[快速上手](site/guide/getting-started.md)开始，或查看
[完整功能概览](site/guide/features.md)。

## 安装

Linux 和 macOS：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

安装器会下载最新版本，并使用 `SHA256SUMS` 校验。需要指定版本或安装目录时，
先下载脚本再执行：

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh
sh install.sh --version 0.1.0 --install-dir "$HOME/bin"
```

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1
.\install.ps1 -Version 0.1.0 -InstallDir "$HOME\bin"
```

如果 GitHub 下载较慢，可以通过可信代理同时获取安装脚本和 Release：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

PATH 设置、安装器参数、源码构建和校验方式见
[安装指南](site/guide/installation.md)。

## 快速开始

```bash
# 安装多个运行时，并发执行下载。
osdk --jobs 4 install node@20 python@3.12 go@1.22

# 设置用户级默认版本。
osdk use -g node@20

# 为当前项目固定版本。
osdk use python@3.12

# 查看当前目录实际使用的版本。
osdk current

# 启用按目录自动切换。
eval "$(osdk activate bash)"

node --version
python --version
```

Shell 激活也支持 zsh、fish 和 PowerShell。需要完整命令说明时，运行
`osdk --help` 或 `osdk <command> --help`。

## 场景：让项目工具链可复现

在仓库中固定工具、解析版本，再安装当前平台对应的锁定结果：

```bash
osdk use node@20
osdk use python@3.12
osdk use go@1.22
osdk lock
osdk install
```

检查符合约束的新版本，或者临时运行命令而不修改项目固定版本：

```bash
osdk outdated
osdk upgrade
osdk exec --tool node@20 -- node --version
```

需要不可变的 Rust 环境时，请固定明确版本或带日期的 toolchain；`stable`、`beta`、
`nightly` 等 rustup 浮动 channel 写入 lock 后仍会随上游更新。

osdk 也能读取已有的 `.tool-versions`、`.nvmrc`、`.node-version`、
`.python-version`、`.java-version`、`go.mod`、`rust-toolchain.toml`，以及
`package.json` 中的 Node 版本声明。
纯数据声明式 backend 与内置归档 backend 共用锁定产物 URL、checksum、下载缓存和
离线重装路径。
对 osdk 自有的动态 `npm:<package>` 与 `github:owner/repo` 安装，选择安装器、构建策略、asset、平台或
布局的选项也属于安装身份。osdk 用 `.osdk-install.json` schema 1 记录该身份，并把每个
`b3-v2:` 身份放入独立的指纹化安装根，因此同一 backend/version 的多个身份可以共存。
复用、activation、shim、`where`、`uninstall` 与 `reshim` 都只选择配置精确匹配的身份。旧
`.osdk-tool.json` 只用于识别遗留状态，绝不会被复用或执行。

指南：[项目工具链](site/guide/projects.md) ·
[锁文件与环境复现](site/guide/lockfiles.md)

## 场景：从直接 HTTPS 制品安装工具

对于没有专用 backend 的工具，可以把一个精确语义化版本绑定到 HTTPS `{version}` URL
模板和发布方提供的 SHA-256。裸可执行文件可直接安装：

```bash
# 请把示例摘要替换为精确 1.2.3 制品的 SHA-256。
osdk install \
  'http:https://downloads.example.com/acme-{version}[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=file,rename=acme]@1.2.3'
```

同一 backend 也支持 `tar.gz`、`tar.xz` 和 ZIP 归档，以及明确的 `bin`/`bins`、
`subdir`、`strip-components` 和单 binary `rename` 布局选项。明文 HTTP、凭据、query、
跨源 redirect、浮动版本和缺失 checksum 都会 fail closed。联网安装并执行 `osdk lock`
该下载不使用代理；只有 DNS 返回的每个地址都通过保守的公网地址检查后才会固定使用，
并受 512 MiB 传输上限和 10 分钟 HTTP 请求 timeout 约束。归档最多 16,384 个条目，
累计声明展开大小最多 2 GiB；发布还要求至少发现一个可执行文件，Windows 只发布以
`.exe` 命名的输出。联网安装并执行 `osdk lock` 后，`osdk --offline install` 可以从精确
身份对应的缓存重放锁定 URL、文件名和 checksum；lock 本身不包含制品字节。

指南：[直接 HTTPS 制品](site/guide/http-artifacts.md)

## 场景：使用包管理器并自动选择可用 Registry

可以独立安装 npm、pnpm 或 Yarn，也可以让 `package.json#packageManager` 中的
精确版本自动加入项目工具链：

```bash
osdk install npm@11.5.2
osdk install pnpm@9.15.0
osdk install yarn@4.9.1
```

在包管理器进程启动前，osdk 可以为 npm、pnpm、Yarn、Bun 和 Deno 选择健康的
已配置 Registry。用以下命令检查当前选择：

```bash
osdk registry test
osdk registry test pnpm
```

显式 Registry 参数、环境变量、私有 Registry 和包管理器原生配置始终由你控制。

指南：[包管理器与 Registry 选择](site/guide/package-managers.md)

## 场景：添加 npm 发布的开发工具

给包名加上 `npm:` 前缀，即可与 npm 包管理器本身区分。在 Node 项目中，`use` 会把包
加入最近的 `package.json`，保留它已有的依赖区段（否则默认写入
`devDependencies`），并在 Shell 激活后提供项目本地命令。激活只会暴露由 osdk 筛选、
且属于已配置包的已校验命令，不会把整个项目 `node_modules/.bin` 加入 PATH：

```bash
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .

# 不采用自动选择时，显式指定安装器。
osdk use npm:eslint@9 -o installer=pnpm

# 安装用户级工具，不修改当前项目。
osdk use --global 'npm:@antfu/ni@0.21.12' -o installer=aube
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

项目 `use` 会遵循最近的 `package.json` 以及唯一一个兼容的现有原生 lock；自动选择会在
该 lock 兼容时优先使用 Aube，也可以用 `installer=aube`、`installer=npm`、
`installer=pnpm` 明确选择。Shell 激活只会暴露由可信项目配置选中、且通过校验的包命令，
不会加入项目的整个 `node_modules/.bin`。当前目录向上没有 `package.json` 时，本地 `use`
保留原有的 osdk 隔离安装与 shim 行为。生成的 `.osdk/npm-bin/` 是本地派生状态，通常应
加入版本控制忽略规则。请把包管理器的原生 lock 与 `osdk.lock` 一起提交；后者不能替代
传递依赖图。

全局 npm、pnpm 与 Aube 都会在 osdk 控制的前缀中调用各自真正的 global-add，不修改环境中
的 Node 安装。Release 安装会同时提供 Aube 全局模式所需的 `osdk-aube` 辅助程序。Aube
2.1 新建或修复全局安装时需要联网；已完整安装且选项身份匹配的精确版本可以在不启动
Aube 的情况下离线再次选中。安装过程本身必须使用原生离线模式时，请选择 npm 或 pnpm。
`where --global` 与 `uninstall --global` 会显式操作配置精确匹配的用户级 npm 安装；不带
该标志时继续保持原有项目/隔离行为。卸载只删除选中的身份根，同一包版本的其他身份仍然
保留。全局卸载还会同步删除对应的用户配置、锁条目，以及不再有其他 owner 的 shim。
项目自身包管理器维护的 npm 依赖及其 `.osdk/npm-bin` 筛选 generation 与这些 osdk 自有
安装根保持独立。

指南：[npm 开发工具](site/guide/npm-tools.md)

## 场景：使用各语言生态

各生态使用一致的命令风格，具体 backend 的能力说明见对应指南；需要生态专属操作时，
使用对应的扩展命令。

### Node.js

```bash
osdk install node@20 -o corepack=true
osdk lock node@20 -o arch=arm64
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

### Python

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@pypy-3.11
osdk python find
osdk python find pypy-3.11
```

### Java 与 JVM 工具

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install maven@3.9.16 gradle@9.7.0 kotlin@2.4.10
```

### Go

```bash
osdk install go@1.22
osdk use go@1.22
osdk exec --tool go@1.22 -- go version
```

### Rust

```bash
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk rust component add rustfmt --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust check --repair
```

指南：[运行时与生态工作流](site/guide/runtimes.md)

## 场景：固定模型快照

从 Hugging Face 或 ModelScope 拉取指定文件、校验本地快照，并获取快照路径：

```bash
export HF_TOKEN=... # 私有或 gated 仓库可选

osdk model pull qwen25 \
  hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors'
osdk model pull qwen25-ms \
  ms:Qwen/Qwen2.5-7B-Instruct@master \
  --include '*.json' --include '*.safetensors'
osdk model verify qwen25
osdk model path qwen25
osdk model list
```

需要让模型工具共享 osdk 的 endpoint 与缓存环境时，为已激活的 Shell 启用
Provider 环境：

```bash
osdk model env enable
osdk model env list
osdk model env disable huggingface
```

指南：[模型快照](site/guide/models.md)

## 场景：控制下载源、离线与安全策略

让 osdk 排序可用下载源、固定首选镜像、添加可信内网源，或只为一条命令覆盖来源：

```bash
osdk source list node
osdk source test node
osdk source pin node tuna
osdk source add node --id mycorp \
  --download-url https://mirror.corp/node/ \
  --index-url https://mirror.corp/node/index.json
osdk --source official install go@1.22
```

成功联网下载后，可以用 `--offline` 强制只使用缓存。安全要求更严格时，可收紧
产物校验策略：

```bash
osdk --offline install node@20
osdk --require-checksums install github:sharkdp/fd
osdk --attestations required install github:cli/cli@latest
```

会影响下载源或执行行为的项目配置，需要先审阅并显式信任：

```bash
osdk --yes trust ./osdk.toml
osdk trust list
osdk untrust ./osdk.toml
```

指南：[下载源、离线与安全](site/guide/sources-security.md)

## 场景：检查容器运行时与原生缓存

```bash
osdk container doctor
osdk container doctor --runtime docker --builder my-builder
osdk container doctor --json
osdk container cache status
osdk container cache status --runtime buildkit --builder my-builder
```

容器命令通过有界的只读原生接口检查 Docker、containerd 和 Buildx。人类可读输出
简洁且支持中英文；`--json` 输出确定且带 schema 版本。这些命令不会启动构建器、
清理数据、改写原生配置或扫描运行时私有存储。

指南：[容器运行时与原生缓存](site/guide/containers.md)

## 场景：检查缓存并回收空间

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
osdk prune --dry-run
osdk --yes prune
```

`cache clean` 删除已下载的归档；`prune` 回收不再引用的共享内容；
`prune --dry-run` 不会删除任何数据。

指南：[存储、缓存与 Shell 集成](site/guide/storage-shell.md)

## 场景：诊断环境或切换语言

```bash
osdk doctor
osdk current
osdk where node
osdk config path
osdk config list
osdk --lang en doctor
OSDK_LANG=zh osdk --help
osdk completions bash > osdk.bash
```

osdk 的命令、帮助、提示、错误和诊断支持中文与英文。`--lang` 覆盖单次命令的
语言，`OSDK_LANG` 设置当前会话偏好。

指南：[存储、Shell 集成、诊断与多语言](site/guide/storage-shell.md)

## 支持范围

| 类别 | 当前支持 |
| --- | --- |
| 平台 | Windows、macOS、Linux |
| 运行时 | Node.js、Python、Java JDK/JRE、Go、Rust、Deno、Bun |
| 包管理器与 JVM 工具 | npm、pnpm、Yarn、Maven、Gradle、Kotlin |
| 其他开发工具 | 通过 `npm:<package>` 安装 npm 包、通过 `github:owner/repo` 安装公开 GitHub Release，或通过 `http:https://...{version}...` 安装精确 checksum 锁定的 HTTPS 制品 |
| 模型平台 | Hugging Face、ModelScope |
| 容器检查 | Docker Engine、containerd、Docker Buildx 与原生缓存状态 |
| 项目输入 | `osdk.toml`、`.tool-versions`、常见生态版本文件 |
| Shell | Bash、zsh、fish、PowerShell |
| CLI 语言 | 中文、英文 |

## 文档

- [功能概览](site/guide/features.md)
- [快速上手](site/guide/getting-started.md)
- [项目工具链](site/guide/projects.md)
- [锁文件与环境复现](site/guide/lockfiles.md)
- [运行时与生态工作流](site/guide/runtimes.md)
- [包管理器与 Registry 选择](site/guide/package-managers.md)
- [npm 开发工具](site/guide/npm-tools.md)
- [直接 HTTPS 制品](site/guide/http-artifacts.md)
- [模型快照](site/guide/models.md)
- [下载源、离线与安全](site/guide/sources-security.md)
- [容器运行时与原生缓存](site/guide/containers.md)
- [存储、Shell 集成、诊断与多语言](site/guide/storage-shell.md)
- [实现文档](site/guide/implementation/index.md)

欢迎通过 [issues](https://github.com/lejunyang/one-sdk/issues) 和 Pull Request
参与贡献。

## 许可证

MIT
