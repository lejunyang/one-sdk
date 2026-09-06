# 存储、Shell 与扩展

本页说明 osdk 的目录、CAS 和缓存边界，以及 shim、shell hook、临时执行、补全、
诊断和数据型声明式 backend。

## 目录布局与覆盖

| 用途 | Linux 默认位置 | 环境变量 |
| --- | --- | --- |
| 数据根 | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| 内容存储 | `<data>/store` | `OSDK_STORE_DIR` |
| SDK 安装 | `<data>/installs` | `OSDK_INSTALL_DIR` |
| 缓存根 | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| 配置目录 | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

派生目录：

```text
<data>/models                模型快照
<data>/shims                 命令 shim
<data>/rustup                隔离 RUSTUP_HOME
<data>/cargo                 Rust backend 的隔离 CARGO_HOME；受控 cargo-binstall 位置
<data>/plugins               声明式 backend
<cache>/downloads            SDK/模型下载文件
<cache>/tmp                  临时解压
<cache>/remote               metadata 与证明缓存
<cache>/sources              来源测速缓存
<cache>/pkg                  下游工具原生缓存
```

`OSDK_BIN_DIR` 属于 osdk 二进制安装脚本，不是 SDK 状态目录；不要把它与
`OSDK_INSTALL_DIR` 混用。

## CAS 与物化

验证、解压后的 SDK 文件和模型文件按 BLAKE3 内容哈希写入 CAS，路径形如
`store/<前2位>/<第3-4位>/<完整hash>`。安装/快照 manifest 记录相对路径、hash、Unix
mode 和 symlink 目标，再把对象物化到版本目录。

`settings.link_mode` 或 `OSDK_LINK_MODE` 接受：

| 模式 | 行为 |
| --- | --- |
| `auto` | 同文件系统优先 hardlink，再 reflink、copy；跨文件系统尝试 reflink 后 copy |
| `hardlink` / `hard` | 优先硬链接，失败回退 copy |
| `reflink` / `clone` / `cow` | 优先写时复制克隆，失败回退 copy |
| `copy` | 普通字节复制 |
| `symlink` / `sym` | 显式符号链接；失败不回退，且 `auto` 永不选择它 |

SDK/模型 CAS 不包含不同 package manager 的原生缓存；后者格式互不兼容，见
[原生缓存语义](./package-managers#原生缓存语义)。

## 缓存层与命令

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| 命令 | 当前行为 |
| --- | --- |
| `cache dir` | 打印缓存根、downloads、CAS 和 downstream 根目录 |
| `cache env` | 打印所有受支持的下游原生缓存环境映射 |
| `cache clean` | 只删除并重建 `<cache>/downloads` |
| `prune --dry-run` | 当前只输出演练提示，不计算或列出候选对象 |
| `prune` | 以 SDK installs 与模型 snapshots 的 manifest 为根，删除无引用 CAS 对象 |

`cache clean` 不删除 CAS、安装、模型、remote/source metadata 或 `<cache>/pkg`。
GC 遇到损坏 manifest 会拒绝继续，防止误删仍在使用的对象。

`osdk container cache status` 是另一条路径：它通过受支持的原生聚合接口查询 Docker
Engine 或 Buildx 构建器自有的存储。详见[容器运行时、Registry 与原生操作](./containers)。

通用 shell hook 在用户未设置时还映射
`npm_config_cache`、`PIP_CACHE_DIR`、`GOMODCACHE`、`GOCACHE`、`CARGO_HOME` 和
`GRADLE_USER_HOME`。这里没有 Maven `M2_HOME`/`maven.repo.local` 重定向。Rust 的
直接 shim/`exec` 会以 `<data>/cargo` 覆盖通用 `<cache>/pkg/cargo` 映射。
Cargo 开发工具 provider 不会把两者当作构建 cache：每次安装都有 stage 私有的 `HOME`、
`CARGO_HOME`、target 与安装根。只有符合条件的受控 `cargo-binstall` 会从
`<data>/cargo/bin` 发现；临时 source/build workspace 会在发布前删除。
Go 开发工具 provider 则把 module/build cache 固定到 `<cache>/pkg/go-mod` 和
`<cache>/pkg/go-build`；其 `HOME`、`GOPATH`、`GOBIN` 与临时目录属于 stage 私有状态，
发布前会删除。

## 删除与确认

```text
osdk uninstall|rm TOOL@VERSION
osdk cache clean
osdk prune [--dry-run]
osdk model remove NAME
```

`uninstall`、`cache clean` 和非演练 `prune` 需要确认。交互终端显示提示；非交互
环境必须使用 `--yes`、`OSDK_YES=true` 或 `settings.yes=true`，否则失败。
`--quiet` 只关闭进度，不代表同意。`model remove` 当前不要求确认。
对 osdk 自有动态工具，uninstall 会派生完整配置身份，只删除对应指纹化根；相同 backend 和
version 的其他身份仍然保留。

## Shim 与 Shell 激活

安装和 `use` 会生成 shim；如果安装目录变化，可重建：

```text
osdk reshim
```

对动态工具，`reshim` 只为配置精确匹配的 `.osdk-install.json` schema 1 身份发布 launcher，
不会选择同版本的其他根，旧 `.osdk-tool.json` 状态也永远不可执行。

shim 在每次执行时按当前目录解析版本，因此 IDE、CI 和未安装 prompt hook 的进程也
能使用项目 pin。它会避免递归调用自身；Windows `.cmd`/`.bat` 目标通过
`%ComSpec% /D /S /C call` 执行以保留参数、stdin/stdout 和状态。生成的 `.cmd` 包装
按系统 OEM 代码页写入安装路径，让没有控制台、回退到 OEM 代码页的 cmd 也能进入非
ASCII 安装路径；遇到 OEM 代码页表达不了的字符时，再在引用行前用 `chcp 65001` 切到
UTF-8 兜底。

Shell 激活命令：

```text
osdk activate bash|zsh|fish|powershell|pwsh
osdk deactivate bash|zsh|fish|powershell|pwsh
```

常见接入方式：

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

hook 在 prompt/目录变化时重新计算环境。PATH 顺序为：

```text
已生成 shims > 独立 npm/pnpm/yarn > Node > 其他 backend > 原 PATH
```

它还导出 backend 环境、下游缓存和已启用的模型 adapter。osdk 保存所有被管理变量
的原值；`deactivate` 移除 hook 并恢复环境。PowerShell hook 带重入保护。

### 挑选要生成的 shim

默认给每个可执行文件都生成 shim。`[settings.shims]` 可以收窄范围：

```toml
[settings.shims]
include = []                      # 非空时只生成匹配项
exclude = ["android-ndk:*"]       # 最后生效，因此总是胜出
```

模式支持 `*` 与 `?`，忽略大小写；带 `backend:name` 前缀时只作用于该 backend。
排除只是不生成 shim——工具仍然装着，激活后仍在 PATH 上，`osdk exec` 也可用。
改完执行 `osdk reshim` 生效。

生成与路由读的是同一份判定，因此被排除的 backend 也不会在运行时抢到某个
共享的命令名。

## 临时执行

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

`-t/--tool` 至少一个且可重复。osdk 先安装指定版本，再把 backend bin 和环境加入
单个子进程；不修改项目 pin，也不读取 lock。

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool node@20 --tool pnpm@10 -- pnpm test
osdk exec -t bun@latest -- bunx vite
```

`pnpx` 路由到受管 `pnpm dlx`，`bunx` 路由到受管 `bun x`；未声明对应 backend
会报错。包管理器调用可能在启动前执行 Registry 预检。子命令失败会使 osdk 返回
错误，但当前不保证原样透传其数值退出码。

## 补全

```text
osdk completions bash|elvish|fish|powershell|zsh
```

命令把补全脚本写到 stdout；请按 shell 的惯例保存或 source。例如：

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions zsh > ~/.zfunc/_osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```

## 诊断

```text
osdk doctor
osdk doctor --verify
osdk doctor --verify --tool TOOL
osdk config path
osdk config list
osdk source list TOOL
osdk registry test [MANAGER]
```

| 命令 | 输出 |
| --- | --- |
| `doctor` | 平台、data/store/install 目录、store 与 install 是否同文件系统、shim 路径及是否在 PATH、backend ID |
| `doctor --verify` | 以上全部，并重新哈希每个已安装文件，指出不再匹配的部分 |
| `doctor --verify --tool` | 同样的检查但只针对单个工具；完整校验会读取每个字节，耗时数分钟 |
| `config path` | 配置目录、用户配置文件、当前项目配置 |
| `config list` | 部分最终设置与目录、registry、模型环境、tools、aliases |
| `source list` | 某 backend/provider 的来源与 pin；`doctor` 不列镜像 |
| `registry test` | npm-compatible Registry 的匿名探测与选择计划 |
| `container doctor` | Docker/containerd 只读选择，以及独立的 Buildx 报告 |

顶层 `doctor` 当前不直接打印 `link_mode`；使用 `config list` 查看。它与诊断原生容器
控制面的 `container doctor` 不同。

## 校验已安装文件

osdk 在下载时校验字节，但在重新检查之前，安装完成之后的变化没有任何环节会发现。会造成这种变化的
有三类：工具自己原地更新、手工改动、备份只恢复了一半或磁盘位翻转。这几种情况下 osdk 仍然报告
它当初安装的版本，实际运行的却是另一个二进制。

`osdk doctor --verify` 会按各安装目录下 `.osdk-manifest.json` 的记录重新哈希每个文件，
并报告不再匹配的部分：

```text
osdk doctor --verify
```

```text
  正在校验已安装文件
  node@20.11.1 : 与 osdk 安装时的内容已不一致
    E:\osdk-data\data\installs\node\20.11.1
      node.exe: contents changed
  已检查 9 个安装，1 个发生变化
  重装即可恢复（普通 install 会跳过已存在的安装）：
    osdk install --force node@20.11.1
```

它区分四类漂移：内容变化、文件缺失、文件类型变化（文件被换成链接，或反之）、以及链接指向已改变。
早于清单机制的安装会被报告为无法校验，而不是默认通过。

这会读取每个文件，因此需要显式开启。普通 `osdk doctor` 仍是快速的环境检查，执行路径上也不做任何
哈希——运行工具的速度不受影响。

开销来自磁盘读取速度，而不是哈希计算。对 11.6 GB、32,463 个文件的完整校验实测约 4.5 分钟，
主要耗在两个 NDK 和一个系统镜像上。指定你实际怀疑的工具即可保持可用：

```text
osdk doctor --verify --tool node
```

同样的检查用时不到 1 秒。`--tool` 必须与 `--verify` 一起使用，未知的名字会被拒绝，而不是
静默地什么都不检查。

### 修复发生漂移的安装

普通 `osdk install` 把已存在的安装视为已完成并立即返回，因此不会修复文件已被改动的安装。
`--force` 会覆盖重装：

```text
osdk install --force node@20.11.1
```

只影响你显式指定的版本。osdk 代为安装的依赖不会跟着被强制重装：把依赖装到位，不构成重建它的理由。

由于安装目录是硬链接到 CAS 的，改动已安装文件同时也改动了其背后的 store 对象。因此 osdk 在复用
对象前会确认它仍然哈希到自己所在的文件名，不符则丢弃，这样重装才是真正的修复，而不是把损坏的字节
重新链接回来。完好的对象仍会复用，去重不受影响。

### 它不做什么

校验是一次快照，不是一项策略。它只能告诉你某个安装与 osdk 当初放进去的内容不再一致，无法判断这次
改动是正当的自更新还是恶意替换——因为在文件系统层面两者完全相同。会自更新的工具在每次更新后都会持续
报告漂移；对这类工具，`--force` 会重装 osdk 所锁定的版本，而这正是锁定版本的意义。

## 声明式 Backend

osdk 启动时自动加载以下目录直接子级的 `*.toml`，无需单独安装命令：

```text
<config>/plugins/*.toml
<data>/plugins/*.toml
```

config 目录先加载，再加载 data 目录。任何非法定义或与内置/其他 backend 的 ID/别名
冲突都会使 registry 加载失败，不允许 shadow。只加载普通文件，不递归；每个文件
最大 1 MiB。

### Schema 1 示例

```toml
schema = 1
id = "acme"
bin_paths = ["bin"]
bin_names = ["acme", "acmectl"]
idiomatic_files = [".acme-version"]

[versions]
values = ["1.0.0", "1.2.3"]
# 或 url = "https://example.test/{os}/{arch}/versions.txt"

[archive]
url = "https://example.test/{version}/{file}"
file = "acme-{version}-{os}-{arch}.tar.gz"
kind = "tar.gz"          # tar.gz|tar.xz|tar.zst|zip|7z
strip_root = true

[archive.checksum]
algorithm = "sha256"     # sha256|sha512|blake3
url = "{archive_url}.sha256"
# 或 value = "<固定十六进制摘要>"
# 或者当上游根本不发布摘要文件时：
#   attestation = { repo = "owner/repo" }

# 可选。编译工具链仅靠 PATH 无法使用，需要通过这类变量把构建系统指向它。
[env]
CC = "{install_path}/bin/acme-gcc"
SYSROOT = "{install_path}/sysroot"
```

### 描述工具链环境

`PATH` 和 shim 让工具的命令可以运行，对解释器或 CLI 来说这就够了。C/C++ 工具链
不同：CMake、Autoconf、Make 通过环境变量定位交叉编译器，因此只把 `bin/` 加入
`PATH` 的定义，装出来的编译器构建系统依然找不到。

可选的 `[env]` 表补上这个缺口。它声明的每个变量都会在该版本激活时导出——包括
`osdk exec`、shell 激活以及 shim。

```toml
[env]
CC = "{install_path}/bin/aarch64-none-elf-gcc"
CXX = "{install_path}/bin/aarch64-none-elf-g++"
AR = "{install_path}/bin/aarch64-none-elf-ar"
SYSROOT = "{install_path}/aarch64-none-elf"
ACME_RELEASE = "{version}"
```

`{install_path}` 展开为该版本自己的安装根，因此取值不必写出宿主机绝对路径；
`{version}` 和 `{id}` 同样可用。

由于这些变量会进入子进程，它是数据式定义唯一可能把构建指向任意宿主状态的地方。
取值因此受到限制，且下列规则都在**解析定义时**而不是激活时强制执行：

- 变量名只能使用 ASCII 字母、数字和 `_`，且不能以数字开头；
- `PATH` 为保留名——目录请通过 `bin_paths` 声明，以保证 shim 生成与激活行为一致。
  `LD_PRELOAD`、`LD_LIBRARY_PATH`、`DYLD_INSERT_LIBRARIES`、`DYLD_LIBRARY_PATH`
  同样保留，因为它们会把进程或动态加载器重定向到安装根之外。保留名不区分大小写；
- 取值不能是绝对路径、不能包含 `..`、不能包含控制字符，从而始终留在
  `{install_path}` 锚定的安装根内；
- 仅接受 `{install_path}`、`{version}`、`{id}`；其他占位符一律失败，渲染后仍残留
  占位符会直接报错，而不是导出一个奇怪的值。

声明了 `[env]` 的定义仍然不能执行代码：它只描述变量，由 osdk 负责导出。

### 上游改名时怎么办

单一模板隐含一个假设：上游会永远保持一致的命名。真实项目不会。LLVM 就是活例子：
Linux x86-64 上它先发布 `clang+llvm-18.1.8-x86_64-linux-gnu-ubuntu-18.04.tar.xz`，
到 19.1.0 改成了 `LLVM-19.1.0-Linux-X64.tar.xz`——词序不同、大小写不同，还嵌了一个
无法从任何平台信息推导出来的发行版号。而 Windows 至今没改，所以**同一个 release 里
两套命名同时存在**。

`[[archive.overrides]]` 就是为此存在。每条 override 声明它适用的条件和要替换的字段：

```toml
[archive]
url = "https://github.com/llvm/llvm-project/releases/download/llvmorg-{version}/{file}"
file = "LLVM-{version}-Linux-X64.tar.xz"
kind = "tar.xz"
strip_root = true

# 19.1.0 之前 Linux 用旧命名。`ubuntu-18.04` 这段无法推导，只能写死。
[[archive.overrides]]
versions = "<19.1.0"
os = "linux"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-linux-gnu-ubuntu-18.04.tar.xz"

# Windows 在这个分界线两侧都保持旧命名。
[[archive.overrides]]
os = "windows"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-pc-windows-msvc.tar.xz"
```

条件包括 `versions`（semver 需求，如 `<19.1.0` 或 `>=17, <18`）以及 `os`、`arch`、
`libc`。它们是**联合**关系：只有一条 override 设置的所有条件都成立时才适用——这正是
LLVM 需要的，因为改名是按平台发生的，而不是整个 release 一起改。

override 可替换 `url`、`file`、`kind`、`strip_root` 和 `checksum`。未设置的字段回退到
`[archive]`，所以只改文件名的 override 不必重复其余内容。

两条规则保证结果可预测：

- **最具体的匹配优先**，按设置的条件数量计算，**与声明顺序无关**。重排定义文件不会
  改变最终安装哪个归档；
- **两条同等具体的匹配会报错**，在解析该版本与平台时报出。按顺序取其一会让结果依赖
  一个很容易被无意改动的因素，因此 osdk 要求你收窄其中一条。

条件同时接受两种 arch 写法（`x64` 与 `x86_64` 都匹配 x86-64），因此不必在
`{arch_llvm}` 之外再学一套词汇。非 semver 的版本永远不满足 `versions` 需求，此时回退
到默认值而不是让安装失败。

### 用 attestation 作为摘要来源

有些上游根本不发布摘要文件。LLVM 就是例子：它的 release 带 `.sig`（GPG），从 19.1.0
起还带 `.jsonl` sigstore bundle，但没有 `.sha256`。而 sigstore bundle 的 in-toto
subject 里本来就含有该制品的 SHA-256，因此这个 attestation 既是签名也是摘要来源——不需
要再去取第二个文件。

```toml
[archive.checksum]
algorithm = "sha256"
attestation = { repo = "llvm/llvm-project" }
```

这与 `gh attestation verify --repo llvm/llvm-project` 做的是同一件事，且**不需要**
`gh` CLI：osdk 用内嵌的 trusted root 验证 bundle，并要求证书来自 GitHub Actions 的
OIDC issuer、且来自你指定仓库的工作流。

由于摘要只有在字节落盘之后才能得知，验证发生在下载**之后**而不是之前。但归档仍然不会
在未验证的情况下被解压——attested 摘要会成为流水线实际强制校验的 checksum。

依赖它之前有两点需要知道：

- **此时验证是强制的。** 当 attestation **就是**摘要来源时，osdk 会无视全局
  `attestations` 设置（其默认值是 `off`）强制要求验证。否则一个声明了 attestation 的
  定义反而会装上一个完全没有完整性证据的归档；
- **覆盖范围并不完整。** 只有上游启用 attestation 之后、由 GitHub Actions 工作流构建
  的制品才有。对 LLVM 而言意味着 19.1.0 及之后，而且即便如此每个 release 也只有部分
  制品有——它的 Windows 归档完全没有 attestation。`[[archive.overrides]]` 之所以能按
  版本和平台切换摘要来源，正是为了应对这种情况。

### 验证与安全边界

- `versions.values` 与 `versions.url` 必须且只能设置一个，最多 10,000 个版本；
- ID 首字符为小写 ASCII 字母或数字，其余仅小写字母、数字、`-`、`_`，并保留
  `github` namespace；
- `bin_paths`、`bin_names` 不能为空，所有路径/名称必须安全且留在安装根内；
- archive URL 或文件名至少一处必须随 `{version}` 变化；每条 `[[archive.overrides]]`
  同样如此，且按其继承后的字段计算；
- 每条 `[[archive.overrides]]` 必须至少设置一个条件（`versions`、`os`、`arch`、
  `libc`），并且至少替换一个字段——这样一条 override 既不会无条件覆盖默认值，也不会
  占用一次匹配却什么都不改；`versions` 必须是合法的 semver 需求。最具体的匹配优先，
  两条同等具体的匹配会报错而不是按顺序取其一；
- archive 类型仅为 `tar.gz|tar.xz|tar.zst|zip|7z`；支持 `.7z` 是因为 Windows GCC
  工具链通常只以该格式发布，解压时会校验条目路径，归档无法写到解压目录之外；
- checksum 的 `value`、`url`、`attestation` 必须且只能设置一个；长度必须符合算法。
  `attestation` 要求 `algorithm = "sha256"`，因为 GitHub attestation 的 subject 带的
  就是 SHA-256 摘要；其 `repo` 必须是纯粹的 `owner/repo`——该值会成为 sigstore 的证书
  身份策略，因此会被校验而不是原样透传。使用 attestation 时摘要一定会被验证，即使全局
  `attestations` 设置为 `off`；
- versions/archive URL 只接受 HTTP(S)，checksum URL 额外可基于 `{archive_url}`；
- 允许的模板变量按位置为 `{id}`、`{version}`、`{os}`、`{arch}`、`{arch_llvm}`、
  `{libc}`、`{file}`、`{archive_url}`；不支持的变量会失败；`[env]` 取值仅接受
  `{install_path}`、`{version}`、`{id}`；
- `{arch}` 渲染 osdk 的短 token（`x64`、`arm64`、`x86`、`arm`），`{arch_llvm}` 渲染
  LLVM target triple 的 CPU 部分（`x86_64`、`aarch64`、`i686`、`armv7`）。编译器与
  工具链归档通常按 triple 命名发布，这类情况用 `{arch_llvm}`；遵循 Node 式命名的
  runtime 用 `{arch}`；
- `[env]` 变量名不能是 `PATH` 或动态加载器变量，取值必须是相对路径、不含 `..`
  且锚定在安装根内；
- schema 使用严格未知字段拒绝，因此不能加入 hook 或 install script。

声明式 backend 只描述数据，不能执行自定义代码；安装仍经过统一下载、checksum、
受限解压和 CAS 物化管线。`osdk lock` 记录 artifact receipt 后，后续不带参数的
`osdk install` 会先使用其中锁定的 URL、文件名、checksum 与可选子目录，再考虑当前
插件模板。因此即使版本/checksum 端点不可用，或本地插件定义后来发生变化，已缓存产物
仍能离线重装。
