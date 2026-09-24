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
- 检查 Docker、containerd、Buildx、OCI Registry、mirror 测速/计划与原生缓存，并按需
  安全应用 mirror 配置、直接拉取镜像或确认严格限定范围的原生清理；
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

安装器会下载最新版本并用 `SHA256SUMS` 校验，随后检测本机已有的 shell，询问需要
配置哪些，并让你确认或修改 osdk 存放配置、数据和缓存的位置。被选中的 shell 会写入
这些环境变量和 `osdk activate`，新开的 shell 即可直接使用。Windows 上还会激活当前
会话；Unix 上加 `--print-activation` 并 eval 其输出，即可激活正在使用的 shell：

```bash
eval "$(sh install.sh --print-activation)"
```

需要指定版本或安装目录时，先下载脚本再执行：

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh
sh install.sh --version 0.0.1 --install-dir "$HOME/bin"
```

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1
.\install.ps1 -Version 0.0.1 -InstallDir "$HOME\bin"
```

每个交互项都有对应参数，因此无人值守安装不会卡住：

```bash
sh install.sh --shells bash,zsh --config-dir ~/.config/osdk --accept-defaults
sh install.sh --no-modify-shell   # 只安装二进制
```

```powershell
.\install.ps1 -Shells pwsh -AcceptDefaults
.\install.ps1 -NoModifyShell
```

如果本机已有 Rust，也可以运行 `cargo install osdk-cli --locked` 安装主命令 `osdk`。
需要包含 `osdk-shim` 的完整两程序安装时，仍推荐使用 Release 安装器。

如果 GitHub 下载较慢，可以通过可信代理同时获取安装脚本和 Release：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

装好之后 osdk 可以自行更新，不需要再跑一遍安装脚本：

```bash
osdk self upgrade --dry-run   # 看看有什么可用版本
osdk self upgrade             # 下载并替换当前安装
```

两个程序会一起替换，下载内容会做校验和比对。更新源与工具下载一样会做测速，因此
GitHub 镜像更快时会自动走镜像；`osdk source test self` 可以看到实测结果，
`osdk source pin self <id>` 可以固定选择。

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

`osdk lock` 只把项目自己声明的工具写进 `osdk.lock`，用户全局配置里的固定版本不会进去，
因此这份 lock 可以放心提交、在别人机器上复现。要连全局工具一起锁，就把它写进项目配置，
或者用 `osdk lock <tool>` 点名。

需要不可变的 Rust 环境时，请固定明确版本或带日期的 toolchain；`stable`、`beta`、
`nightly` 等 rustup 浮动 channel 写入 lock 后仍会随上游更新。

osdk 也能读取已有的 `.tool-versions`、`.nvmrc`、`.node-version`、
`.python-version`、`.java-version`、`go.mod`、`rust-toolchain.toml`，以及
`package.json` 中的 Node 版本声明。
纯数据声明式 backend 与内置归档 backend 共用锁定产物 URL、checksum、下载缓存和
离线重装路径，并且可以声明工具链所需的环境——C/C++ 交叉编译器是通过 `CC`、
`SYSROOT` 等变量而非仅靠 `PATH` 被构建系统找到的——osdk 会在该版本激活时导出这些
变量。
对 osdk 自有的动态 `npm:<package>`、`cargo:<crate-or-https-url>`、
`go:<module-or-command-path>`、`conda:<package>`、`pypi:<project>` 与 `github:owner/repo` 安装，会改变选择或构建结果的选项与
受管 runtime 依赖也属于安装身份。
osdk 用 `.osdk-install.json` schema 1 记录该身份，并把每个
`b3-v2:` 身份放入独立的指纹化安装根，因此同一 backend/version 的多个身份可以共存。
复用、activation、shim、`where`、`uninstall` 与 `reshim` 都只选择配置精确匹配的身份。旧
`.osdk-tool.json` 只用于识别遗留状态，绝不会被复用或执行。

指南：[项目工具链](site/guide/projects.md) ·
[锁文件与环境复现](site/guide/lockfiles.md)

## 场景：用项目任务替代 Makefile

在 `osdk.toml` 里声明命令，然后用 `osdk run` 执行。任务默认就是伪目标，
依赖按拓扑顺序执行，工具版本由 osdk 注入——无需先激活 shell。

```toml
[tasks]
build = "cargo build --release"

[tasks.ci]
run = [
  "cargo fmt --check",
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },
  { tasks = ["test", "doc"] },
]
depends = ["build"]
```

```bash
osdk run ci
osdk task list
osdk run ci --dry-run
```

数组里的命令依次执行、失败即停；`ignore_error` 表示容忍失败并继续（但会
打印警告）；`{ tasks = [...] }` 并行执行并等待全部完成。不要用 shell 的
`&`——它在 cmd、PowerShell 7 与 5.1 下含义各不相同。详见
[项目任务](site/guide/tasks.md)。

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
osdk use --global 'npm:@antfu/ni@0.21.12' -o installer=npm
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

项目 `use` 会遵循最近的 `package.json` 以及唯一一个兼容的现有原生 lock；自动选择依次
参考项目声明的 `packageManager`、现有原生 lock 的归属安装器，最后才使用配置的默认值，
也可以用 `installer=npm`、`installer=pnpm` 明确选择。默认值可通过 `config.toml` 中的
`settings.npm.default-installer` 或 `OSDK_NPM_DEFAULT_INSTALLER` 设置，它只在项目自身
没有任何声明时生效。Shell 激活只会暴露由可信项目配置选中、且通过校验的包命令，
不会加入项目的整个 `node_modules/.bin`。当前目录向上没有 `package.json` 时，本地 `use`
保留原有的 osdk 隔离安装与 shim 行为。生成的 `.osdk/npm-bin/` 是本地派生状态，通常应
加入版本控制忽略规则。请把包管理器的原生 lock 与 `osdk.lock` 一起提交；后者不能替代
传递依赖图。

全局 npm 与 pnpm 都会在 osdk 控制的前缀中调用各自真正的 global-add，不修改环境中
的 Node 安装。
`where --global` 与 `uninstall --global` 会显式操作配置精确匹配的用户级 npm 安装；不带
该标志时继续保持原有项目/隔离行为。卸载只删除选中的身份根，同一包版本的其他身份仍然
保留。全局卸载还会同步删除对应的用户配置、锁条目，以及不再有其他 owner 的 shim。
项目自身包管理器维护的 npm 依赖及其 `.osdk/npm-bin` 筛选 generation 与这些 osdk 自有
安装根保持独立。

指南：[npm 开发工具](site/guide/npm-tools.md)

## 场景：安装项目自己的应用依赖

`install` 装的是工具，`deps` 装的是项目自己的依赖清单：`[tools]` 把包管理器准备好，
`deps` 再驱动它读 `package.json`，把整份依赖闭包装进项目。要增删单个依赖，仍然用
`osdk install npm:<包名>`。

在 `osdk.toml` 里声明一个 provider，然后运行：

```toml
[deps.pnpm]          # Node：npm / pnpm / yarn / bun
# [deps.go]          # Go / Rust / Deno：go、cargo、deno
# roots = ["apps/*"]  # monorepo：显式声明子项目，绝不盲扫
# [deps.uv]          # Python：pyproject.toml + uv.lock
# [deps.pip-requirements]   # Python：requirements.txt（解析传递依赖；不作冻结承诺）
```

```bash
osdk deps --list            # 列出探测到的 provider 与新鲜度
osdk deps --dry-run         # 打印将要执行的命令，不执行
osdk deps                   # 兑现整份清单
osdk deps --verify          # 按包管理器自己的收据校验已装环境
osdk deps npm               # 当前工作目录最近的 npm 项目
osdk deps //:npm            # 只处理配置根
osdk deps //apps/api:npm    # 只处理指定子项目
```

声明之后不必每次手动调用：裸跑 `osdk install`、`osdk run <任务>`、`osdk exec`
之前，osdk 会先比对清单哈希，过期才兑现。命中时只花零点几毫秒、不启动包管理器，
也不做 `--verify` 那种深度扫描。带具体工具的 `osdk install node@22` 不会触发，
单次跳过用 `--no-deps`，永久关闭某个 provider 用 `auto = false`：

```bash
osdk run dev                # 依赖过期就先兑现，再跑任务
osdk run dev --no-deps      # 这一次不要
```

`pip-requirements` 会用 `uv pip install -r` 解析完整传递闭包；顶层
`requirements.txt` 不会被当作精确 lock 或同步集合。对于有原生 lockfile 的 provider，
osdk 不把「是否冻结」交给包管理器判断，而是自己先看 lockfile 在不在：有就用
冻结安装，没有就退回普通安装**并明确告知**。这一点是必要的——`yarn@1` 与 `bun` 在
缺少 lockfile 时会照常安装而不报错，只靠传参会在半数组合上静默失效。需要严格时用
`--frozen`，它把这种退回变成错误：

```bash
osdk deps --frozen
```

默认不运行任何依赖的构建或生命周期脚本。需要时按 provider 显式打开，这一项需要你
批准配置：

```toml
[deps.pnpm]
allow_build_from_source = true
```

包管理器没装时，`deps` 会走 osdk 平常那条工具安装链自动装上，并装进 osdk 的隔离
目录而非你的项目。CI 里想让工具只来自显式的 `osdk install`，可以关掉自动获取：

```bash
osdk deps --no-install-tools
```

没有 `[deps]` 段时，`osdk deps` 只报告它找到了什么、可以用哪些 provider，不会动手
安装。

指南：[应用依赖](site/guide/deps.md)

## 场景：从 Cargo 安装 Rust CLI

先选择一个精确的受管 Rust 版本，再按精确版本、最新稳定版或数字前缀安装 crate：

```bash
osdk use rust@1.91.1
osdk use cargo:ripgrep@14.1 -o features=pcre2 -o locked=true
eval "$(osdk activate bash)"
rg --version
```

Cargo 工具会拒绝浮动或本地链接的 Rust toolchain。Registry 请求使用
`cargo:<crate>`；HTTPS Git 请求使用 `latest`、`tag:<ref>`、`branch:<ref>`，或者不可变的
`rev:<40 位小写十六进制>` selector：

```bash
osdk use \
  'cargo:https://github.com/BurntSushi/ripgrep.git@rev:0123456789abcdef0123456789abcdef01234567'
```

支持的构建选项包括 `features`、`default-features`、`bin`、`locked`（默认 `false`），
以及仅限 Git 的 `crate`。完整且身份精确匹配的安装可以离线复用，但 Cargo lock 不包含
全新离线构建所需的完整 source graph。Registry 版本记录为 `version-only`，完整 Git
revision 记录为 `immutable-revision`，Git HEAD/tag/branch 则记录为 `floating-ref`。

指南：[Cargo 开发工具](site/guide/cargo-tools.md)

## 场景：安装 Go command package

区分 Go runtime 与 Go command 命名空间，再使用精确的受管 Go toolchain 安装命令：

```bash
osdk use go@1.24
osdk use go:golang.org/x/tools/gopls@0.20.0
eval "$(osdk activate bash)"
gopls version
```

`go:` 支持 module 或嵌套 command path、`latest`、数字前缀、精确语义版本和规范伪版本。
`tags` 与受限 `env` 会参与身份；支持 `CGO_ENABLED=0`，在 C toolchain 能纳入身份绑定前
拒绝开启 cgo。osdk 会选择并记录一个 Go proxy，以 staged `GOBIN` 只调用一次精确受管
`go`，module/build cache 留在 osdk cache 根中。紧凑的 schema 4 lock 记录 proxy、module
root 与精确 Go runtime，不记录传递 module graph；因此可离线复用精确匹配的完整安装，
但不能进行全新离线构建。

指南：[Go 开发工具](site/guide/go-tools.md)

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
osdk install maven@3.9.16 "gradle@=9.3.1" kotlin@2.4.10
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

### Zig

```bash
osdk install zig@latest
osdk exec --tool zig -- zig version
```

Zig 自带 libc（musl、多个 glibc 版本、mingw-w64、wasi-libc），因此装一份就能把
C / C++ 交叉编译到多个目标，无需为每个目标准备 sysroot：

```bash
osdk exec --tool zig -- zig cc -target aarch64-linux-musl -o hello hello.c
osdk exec --tool zig -- zig cc -target x86_64-windows-gnu -o hello.exe hello.c
```

指南：[运行时与生态工作流](site/guide/runtimes.md)

## 场景：管理 Android SDK

直接从 Google 仓库安装 Android SDK 包（NDK、`adb`/`fastboot`、build-tools 等），
并通过传参接受所需协议，因此在 CI 中也能无人值守运行：

```bash
# 先看协议内容，再决定是否接受
osdk android licenses show android-ndk@29.0.14206865

# 显式接受后安装
osdk install android-ndk@29.0.14206865 -o accept-licenses=true
osdk install android-platform-tools@37.0.1 -o accept-license=android-sdk-license

# 使用工具
osdk exec -t android-platform-tools@37.0.1 -- adb devices

# 把接受记录交给 Gradle 复用
osdk android licenses export --sdk-root /path/to/sdk
```

osdk 不会替你接受协议：未传接受选项时，安装会在下载任何内容之前停止。
可用包族为 `android-ndk`、`android-platform-tools`、`android-build-tools`、
`android-cmdline-tools`、`android-cmake`、`android-platforms`、
`android-emulator`、`android-sources`、`android-system-images`。

`android-platforms` 与 `android-sources` 的版本形如 `android-37.2`，`latest` 取最新的
稳定 API 级别。预览版需要按名字显式指定：Google 把 `android-37.2-beta3`、
`android-CANARY` 这类构建**发布在稳定通道上**（`channelRef` 为 `channel-0`），osdk 依据
清单里的 `<codename>` 与 `<beta-api-level>` 识别它们，因此默认的
`prerelease = if-explicit` 策略仍能把它们挡在 `latest` 之外。

同一家族里 `android-36` 与 `android-36.1` 是两个不同的 API 级别，名字本身看不出这一点，
所以 `osdk list-remote` 会标出每个候选的真实 API 级别：

```bash
osdk list-remote android-platforms
# android-36-ext19 (API 36x)   <- ExtensionLevel 19 的 side-by-side 扩展包
# android-36 (API 36)
# android-36.1 (API 36.1)
```

需要「就是这一个、不接受任何回退」时，用 `=` 精确锁定：

```bash
# 恰好 API 36，不会漂移到 36.1
osdk install "android-platforms@=android-36"
```

模拟器系统镜像同样如此，并且包声明的依赖会随之一起安装——安装镜像时会带上它
所需的 `android-emulator`：

```bash
# 浏览可用镜像（各厂商与设备形态都在同一份列表中）
osdk list-remote android-system-images

# 安装镜像时会一并安装它依赖的模拟器
osdk install "android-system-images@android-35;google_apis;x86_64" \
  -o accept-licenses=true
```

模拟器只接受包含 `platform-tools` 的 SDK 目录，因此创建 AVD 前也要安装它。
osdk 会把所有 Android 包组织成 Google 工具所期望的那一套目录结构，且不会重复
存储任何一个包。

用镜像创建并运行虚拟设备：

```bash
osdk android avd create pixel-35 --image "android-35;google_apis;x86_64"
osdk android avd list
emulator -avd pixel-35
```

设备存放在 osdk 的数据目录下，emulator 与 `avdmanager` 会被自动指向该目录，因此在
已激活的 shell 里直接执行 `emulator -avd pixel-35` 即可，无需手动导出任何变量。

设备定义由 osdk 自己写出，而不是调用 `avdmanager`——后者在这套布局下无法工作：
它靠检视自身路径来定位 SDK，因而找高了一层，而 `create avd` 又没有可纠正它的
参数。即便它创建成功，写下的镜像路径也是相对的，在这里会解析到错误的目录。

查看 Google 自家工具能看到什么，以及重建它们读取的索引：

```bash
osdk android sdk-root show
osdk android sdk-root repair
```

升级 osdk 后值得跑一次 `repair`：早先版本装下的包没有索引文件，于是
`sdkmanager` 能列出它们，而 `avdmanager` 会报 `Package path is not valid`。

osdk 把每个 Android 包链接进 Google 工具期望的那套目录布局（Windows 上是
junction），载荷只存一份。卸载时会先删链接再删载荷——顺序相反会留下悬空链接，而它
对模拟器和 Gradle 的存在性检查依然回答"在"。`sdk-root show` 会报告悬空链接，
`repair` 负责清除。

Android 包本身不含 JDK，因此 `sdkmanager`、`avdmanager`、`d8` 等基于 jar 的工具
会在你未自行设置 `JAVA_HOME` 时使用 osdk 管理的 `java`（由 osdk 激活导出的旧值会按当前
目录重算，不会让一个项目的 JDK 驱动另一个项目）。用 `osdk install java` 装一个即可。

指南：[Android SDK 工具](site/guide/android.md)

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

# 下次 pull 之后依然有效的路径：快照目录名含内容哈希，
# 要写进 ComfyUI、llama.cpp 或脚本里的路径请用这个
osdk model path qwen25 --stable

# 在另一台机器上按 osdk.lock 还原同一批快照（pull 写、sync 复现）
osdk model sync
osdk model list
```

把快照渲染成 ComfyUI / Hugging Face 缓存形状的消费者视图（链接回快照、不复制
权重、只读），并生成或打印接入配置：

```bash
osdk model view add comfyui qwen25 --map unet/=diffusion_models
osdk model view path comfyui                 # 稳定路径，贴进消费者配置
osdk model view export comfyui --to extra_model_paths.yaml   # 源码版 ComfyUI
osdk model view list
osdk model view doctor comfyui
osdk model view remove comfyui --model qwen25
```
也可以直接在 `osdk.toml` 里声明模型；`osdk model pull <name>` 会读取这份声明，把
视图写进 lock 并立即渲染。只声明「要什么」不需要信任，只有 `endpoint`/自定义来源
这类会改变字节来源的 key 才需要，而且模型声明不会阻断普通工具命令：

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"
```

`osdk model sync` 会还原 lock 声明的全部模型，**并**重建它们的视图。


需要让模型工具共享 osdk 的 endpoint 与缓存环境时，为已激活的 Shell 启用
Provider 环境：

```bash
osdk model env enable
osdk model env list
osdk model env disable huggingface
```

指南：[模型快照](site/guide/models.md)

## 场景：给 AI 编码 Agent 装 skill

skill 是带 `SKILL.md` 的指令包，供 Claude Code、Codex、Cursor 等 AI 编码 Agent 读取。osdk
从 GitHub 或本地路径安装 skill，内容寻址落地后链接进各 Agent 的 skills 目录，并把不可变身份
写进 `osdk.lock`，团队 `osdk skills sync` 一步复现。osdk 只搬运与链接，绝不执行 skill 里的脚本。

```bash
osdk skills agents                       # 看 osdk 认识哪些 Agent、各自的 skills 目录
osdk skills find agent skills             # 在 GitHub 上搜可安装的 skill（匿名，不接触 skills.sh）
osdk skills add github:vercel-labs/agent-skills --list        # 只列仓库里有哪些 skill
osdk skills add github:vercel-labs/agent-skills/skills/web-design-guidelines -a claude-code
osdk skills add ./my-skills -s my-skill -a codex              # 本地源，选装指定 skill
osdk skills list                         # 已装 skill 与其链接到的 Agent
osdk skills sync                         # 按 osdk.lock 复现（团队 / CI）
osdk skills remove web-design-guidelines
```

`add` 把解析到的 commit 与内容哈希写进 `osdk.lock`；`sync` 缺本地副本时按记录的 commit 重新
下载并核对哈希，移动的 tag 或被换的镜像会被拒绝。默认目录链接（Windows junction / Unix
symlink），无链接环境或 `--copy` 时整树拷贝，且不会覆盖非 osdk 放置的真实目录。也可以在
`osdk.toml` 里用 `[skills]` 声明，让 `osdk skills sync` 直接复现。

指南：[Agent skills](site/guide/skills.md)

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

模型仓库的 metadata 与 CDN 冷启动比 SDK 索引更重，因此使用独立探测预算：默认
`sources.model_probe_timeout_ms = 8000`，分别应用于 metadata 解析、响应头和 64 KiB
样本读取。osdk 优先探测仓库中最小的非空文件；只要成功拿到响应，就认定来源可达，
即使样本 body 超时也只记为吞吐未知，不再误报 `unreachable`。全部来源均失败的结果
不会按常规 6 小时 TTL 缓存。

由 osdk 自身执行的归档和模型文件下载并非失败即停：共享下载管线会对瞬时错误最多
尝试 3 次，前两次分别等待 400 ms、800 ms。下载使用 `.partial` 文件并保存 ETag /
Last-Modified，重试时通过 `Range` + `If-Range` 断点续传；服务端忽略 Range 或对象已
变化时会安全重头下载。模型在某个来源耗尽重试后，还会继续尝试下一个排序后的来源。
不可重试错误，或第三次尝试及全部来源回退都失败时，命令才会停止。

环境变量里已经设置的镜像（`RUSTUP_DIST_SERVER`、`GOPROXY`、`npm_config_registry`
等）会先经过校验，再与 osdk 内置镜像一起参与测速竞争，因此过期或不可用的值不会
仅因为存在就胜出；不可用时会给出提示而不是被静默忽略。如果需要无条件遵循它，
或希望它缺失时直接报错：

```bash
osdk --source-mode env install rust@1.98.0
```

Go 的模块代理是一条独立通道：`[sources.go]` 只决定从哪里下载 Go 工具链本身，
而 `go build` / `go test` 拉取依赖走的是 `GOPROXY`。运行受管 go 时 osdk 会
自动写入一份按优先级排好、以 `|` 连接的 `GOPROXY`（官方源在前、镜像随后、
`direct` 兜底），因此 `proxy.golang.org` 不可达时会自动改用镜像而不是直接
失败。用 `|` 而不是 `,` 是必须的：逗号只在 404/410 时继续尝试下一个，连接
超时会被当成终止错误。

这组镜像用 `go-modules` 这个名字单独管理，与工具链归档源 `[sources.go]` 互不影响，
常规的 `osdk source` 子命令都适用：

```bash
osdk source list go-modules
osdk source test go-modules
osdk source add go-modules --id corp --download-url https://goproxy.corp/
osdk source pin go-modules goproxy.cn
osdk source unpin go-modules
```

pin 表示「优先尝试」而不是「只用这一个」：被 pin 的源移到首位，其余仍留作回退，
因此单点不可达不会直接导致构建失败。只想保留一个端点时，在配置里用
`[sources.go-modules].disable` 去掉其他源。

自己设过 `GOPROXY`（包括 `off`、`direct` 这类策略值）时，osdk 不会覆盖它。

成功联网下载后，可以用 `--offline` 强制只使用缓存。安全要求更严格时，可收紧
产物校验策略：

```bash
osdk --offline install node@20
osdk --require-checksums install github:sharkdp/fd
osdk --attestations required install github:cli/cli@latest
```

会在本机执行代码，或会削弱产物校验、改变下载来源的项目配置，需要先审阅并显式信任。
仅声明安装哪些工具或包不在此列——被拒绝时，osdk 会逐条列出具体是哪些键以及各自原因：

```bash
osdk --yes trust ./osdk.toml
osdk trust list
osdk untrust ./osdk.toml
osdk trust prune                 # 清理配置文件已不存在的记录
```

指南：[下载源、离线与安全](site/guide/sources-security.md)

## 场景：检查并操作原生容器运行时

Docker Hub 开箱内置两个由运营方公开说明的 pull-through cache：`mirror.gcr.io` 和
`docker.m.daocloud.io`。osdk 会用 `library/alpine:latest` 匿名测速，校验 Manifest 等价性与
有界分层样本，再按实测延迟推荐通过检查的镜像。已信任的用户或项目配置中的显式 policy
会完整覆盖这些内置候选：

```toml
[containers.registries."docker.io"]
mirrors = ["https://mirror.example/"]
anonymous_only = true
resolve = "mirror"
```

```bash
osdk container doctor
osdk container doctor --runtime docker --builder my-builder
osdk container doctor --json
osdk container registry test docker.io
osdk container registry test docker.io \
  --image ubuntu:24.04 --platform linux/amd64 --json
osdk container mirrors plan docker.io --runtime docker
osdk container mirrors plan docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --json
osdk container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json
# 自动化采用两步流程，并绑定本次生成的精确计划：
plan_id=$(osdk container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --dry-run --json | jq -r .plan_id)
osdk --yes container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --accept-plan "$plan_id" --json
osdk container cache status
osdk container cache status --runtime buildkit --builder my-builder
osdk container pull ubuntu:24.04
osdk container pull ghcr.io/example/tool:1.0 \
  --runtime containerd --platform linux/amd64 \
  --address unix:///run/containerd/containerd.sock --namespace default
osdk container prune --runtime docker --scope images
osdk container prune --runtime buildkit --scope build-cache --builder my-builder
```

`container doctor` 会先报告选中的 runtime，再展示同一次探测已获得的类型化事实：Docker
版本/平台/rootless/Desktop 与 mirror origin；containerd 版本和 Registry 配置状态；以及
Buildx driver、节点状态、BuildKit 版本、endpoint 与平台。其 schema version 2 JSON 不包含
context、builder、节点名称、namespace、原生配置路径及可能带敏感信息的 endpoint path/query。

Registry 测试只使用匿名 HTTPS，可检查 image digest、平台选择与有界 Range，并对通过
内容校验的 mirror 排序。每份 mirror plan 只针对一个已配置 policy（Docker Hub 也可使用
内置 policy）和一个显式 Docker、containerd 或 BuildKit 控制面，并报告确定的 `plan_id`；
本来可执行的本地 plan 如果没有
显式原生配置路径，会标为 `manual-only`。规划不会写原生配置、启动 builder 或重启 daemon。
Plan JSON 可能包含操作所需的绝对路径、builder 名、mirror origin 及是否存在 path prefix，
但不显示精确 mirror prefix、现有配置内容或生成的 candidate bytes。`mirrors apply` 在一次
调用内完成测速与规划，交互确认时不要求复制 ID；确认后会在锁内复核输入并原子替换文件。
它不会自动提权，也不会重启 daemon 或重建 builder。无人值守 `--yes` 必须带本次计划对应的
`--accept-plan`；先用 `--dry-run --json` 获取 ID。

`container pull` 默认使用生效的 runtime 与 platform。`auto` 模式对 Docker 与 containerd
执行一次有界只读解析，再启动恰好一次原生前台拉取。显式选择 containerd 时必须成对提供
`--address` 与 `--namespace`；`auto` 仅在 containerd 胜出时要求二者，因此 Docker 无需它们
即可继续。子进程继承 stdio，osdk 等待它结束并返回直接退出码；Unix 上若由信号终止，则
规范化为 `128 + signal`。启动后不会回退到其他 runtime，也不会把镜像复制到 osdk 存储。

`container prune` 默认只输出预览，可针对一个已发现的 Docker context 或 Buildx builder，
同时绑定 Docker endpoint 或 Buildx driver/node endpoint 拓扑的敏感信息安全指纹。只有不依赖
context TLS 材料、可通过本地 Unix socket 或 Windows named pipe 直接寻址的 Docker context
支持执行；它通过 `docker --host` 仅删除 dangling image。执行时必须同时
传入 `--execute` 与预览中原样返回的 `--accept-preview sha256:...`，然后确认执行提示（或使用
全局 `--yes`）。BuildKit 因可变 builder 名称无法原子固定而仅支持预览。虽然 `--scope` 仍是必填参数，但 containerd 没有任何可接受的 scope 组合：
不带 selector、也不请求执行时返回类型化“不支持”，传入 selector 或执行参数则会被拒绝。
该命令绝不会扩展为清理全部 system、container、volume、network 或实现私有存储。

指南：[容器运行时、Registry 与原生操作](site/guide/containers.md)

## 场景：查看宿主自带的包管理器

```bash
osdk pkg doctor
osdk pkg doctor --json
osdk pkg mirrors test
osdk pkg mirrors apply --dry-run
```

报告宿主上有哪些系统包管理器（目前是 winget）、版本、已配置的源及其信任级别。
`mirrors test` 实测各镜像源速度并排名，官方源一同参与比较。
只读：不安装、不改配置、不提权。`--json` 的输出带 schema 版本号，且与界面语言无关。

注意 winget 的镜像只加速搜索和列表，不加速安装包下载——manifest 里的下载地址指向
各软件厂商自己的服务器。osdk 会在输出里说明这一点。

`mirrors apply` 是其中唯一会改机器状态的命令：需要管理员，且必须带上
`--accept-plan` 指纹确认。装不上的镜像（比本机旧）会被提前拒绝，执行中失败会自动回滚。

指南：[系统包管理器](site/guide/system-packages.md)

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

## 场景：某个工具自己更新过，已与安装时不一致

```bash
osdk doctor --verify --tool node
osdk install --force node@20.11.1
```

`doctor --verify` 会重新哈希每个已安装文件，指出安装之后被改动的那些——工具自更新、手工改动、
备份只恢复一半都会留下这种痕迹。普通 `install` 把已存在的安装视为已完成，不会修复它；`--force`
会用锁定的版本覆盖重装。校验会读取每个文件，因此需显式开启——普通 `doctor` 仍然很快，执行路径上
也不做任何哈希。

指南：[存储、缓存与 Shell 集成](site/guide/storage-shell.md)

## 场景：诊断环境或切换语言

```bash
osdk doctor
osdk doctor --verify
osdk current
osdk where node
osdk config path
osdk config list
osdk config get jobs
osdk config set shims.exclude "apkanalyzer"
osdk --lang en doctor
OSDK_LANG=zh osdk --help
osdk completions bash > osdk.bash
```

osdk 的命令、帮助、提示、错误和诊断支持中文与英文。`--lang` 覆盖单次命令的
语言，`OSDK_LANG` 设置当前会话偏好。

`osdk doctor` 还会报告代理状态。osdk 只从 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` 读取代理，不读 Windows 的「系统代理」开关；两者不一致时它会明确指出并给出要设置的变量，而不是让「浏览器能上、osdk 超时」这种情况无从解释。

指南：[存储、Shell 集成、诊断与多语言](site/guide/storage-shell.md)

## 支持范围

| 类别 | 当前支持 |
| --- | --- |
| 平台 | Windows、macOS、Linux |
| 运行时 | Node.js、Python、Java JDK/JRE、Go、Rust、Deno、Bun、Zig |
| 包管理器与 JVM 工具 | npm、pnpm、Yarn、Maven、Gradle、Kotlin |
| 其他开发工具 | 通过 `npm:<package>` 安装 npm 包、通过 `cargo:...` 安装 Registry crate 或 HTTPS Git 仓库、通过 `go:<module-or-command-path>` 安装 Go command package、通过 `conda:<package>` 安装 conda 包与 CUDA 等工具链、通过 `pypi:<project>` 安装 Python CLI（每个工具一个虚拟环境，依赖在环境间共享）、通过 `github:owner/repo` 安装公开 GitHub Release，或通过 `http:https://...{version}...` 安装精确 checksum 锁定的 HTTPS 制品 |
| 模型平台 | Hugging Face、ModelScope |
| Agent skills | 从 GitHub（`github:owner/repo` 含子目录）或本地路径安装 `SKILL.md` 包，链接进 Claude Code / Codex / Cursor / OpenCode / Gemini CLI / GitHub Copilot 等 Agent |
| 原生容器操作 | Docker Engine、containerd、Docker Buildx、匿名 OCI Registry 测试、内置 Docker Hub mirror 测速、安全原生 mirror apply、直接原生镜像拉取、原生缓存状态、本地 endpoint Docker 清理，以及 BuildKit 清理预览 |
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
- [应用依赖](site/guide/deps.md)
- [Cargo 开发工具](site/guide/cargo-tools.md)
- [Go 开发工具](site/guide/go-tools.md)
- [直接 HTTPS 制品](site/guide/http-artifacts.md)
- [模型快照](site/guide/models.md)
- [Agent skills](site/guide/skills.md)
- [下载源、离线与安全](site/guide/sources-security.md)
- [容器运行时、Registry 与原生操作](site/guide/containers.md)
- [存储、Shell 集成、诊断与多语言](site/guide/storage-shell.md)
- [实现文档](site/guide/implementation/index.md)

欢迎通过 [issues](https://github.com/lejunyang/one-sdk/issues) 和 Pull Request
参与贡献。

## 许可证

MIT
