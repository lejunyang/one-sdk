# 安装

osdk 提供 Windows、macOS 和 Linux 的预编译二进制。每个 Release 安装包会安装两个
同目录程序：主 CLI `osdk`，以及负责启动已激活工具的 `osdk-shim`。请始终把两个程序放在
同一目录。

## Linux 与 macOS

运行一键安装脚本：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

默认安装到 `~/.local/bin`。安装器随后会引导完成 shell 配置，见
[安装后的 shell 配置](#安装后的-shell-配置)。

## Windows

在 PowerShell 中运行：

```powershell
$env:OSDK_DOWNLOAD_BASE_URL = "https://gh-proxy.com/https://github.com"
irm https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

这些示例既代理 Raw 安装脚本，也通过 `OSDK_DOWNLOAD_BASE_URL` 代理脚本后续
下载的 GitHub Release 二进制和 `SHA256SUMS`。

默认安装到 `%LOCALAPPDATA%\Programs\osdk\bin`。

## 安装后的 shell 配置

二进制就位后，安装器会：

1. 检测本机已存在的 shell，列出各自的启动文件路径，让你批量选择要配置哪些；
2. 依次询问 `OSDK_CONFIG_DIR`、`OSDK_DATA_DIR`、`OSDK_CACHE_DIR`，每项都给出
   默认值，直接回车即接受；
3. 校验每个目录：必须是绝对路径、能够创建、并且实际可写。不满足时说明原因并
   重新询问；
4. 向每个选中的 shell 写入一段带标记的配置块，内容包括上述环境变量、把二进制
   目录加入 `PATH`，以及调用 `osdk activate`。

默认值与 osdk 自身推导的位置一致，因此接受默认不会改变任何东西的落盘位置，只是
把它显式写出来。各平台默认值见
[存储、Shell 与扩展](./storage-shell#目录布局与覆盖)。

支持 bash、zsh、fish 和 PowerShell。Windows 上 Windows PowerShell 与 PowerShell 7
使用各自独立的 profile，因此分别列出。

配置块形如：

```text
# >>> osdk initialize >>>
...
# <<< osdk initialize <<<
```

重复运行安装器会**替换**这个块，而不是追加第二份；块以外的内容原样保留，原文件
也会先备份为 `<启动文件>.osdk-backup`。删除整个块即可移除集成。

### 在当前 shell 立即生效

写入启动文件只对新开的 shell 生效。Windows 安装器会顺带激活运行它的那个会话；
Unix 上子进程无法修改父 shell，因此提供 `--print-activation`——它把激活代码写到
stdout、其余输出转到 stderr，于是可以直接 eval：

```bash
eval "$(sh install.sh --print-activation)"
```

也可以在安装完成后手动激活当前 shell：

```bash
eval "$(osdk activate bash)"        # zsh 同理
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

### 无人值守安装

每个交互项都有对应参数，传了参数就不再询问：

```bash
sh install.sh --shells bash,zsh \
  --config-dir "$HOME/.config/osdk" \
  --data-dir "$HOME/.local/share/osdk" \
  --cache-dir "$HOME/.cache/osdk"

sh install.sh --accept-defaults     # 配置检测到的全部 shell，全部使用默认目录
sh install.sh --no-modify-shell     # 只安装二进制，不改动任何启动文件
```

```powershell
.\install.ps1 -Shells pwsh -ConfigDir "D:\osdk\config" `
  -DataDir "D:\osdk\data" -CacheDir "D:\osdk\cache"

.\install.ps1 -AcceptDefaults
.\install.ps1 -NoModifyShell
```

Unix 提示读的是 `/dev/tty` 而不是 stdin，因此 `curl ... | sh` 这种管道用法仍然可以
交互。没有终端且未传任何 shell 相关参数时，安装器不会改动任何启动文件。

## 自定义安装

通过管道运行适合默认安装；需要传参时，应先下载脚本。

### Unix 参数

```bash
curl -sSfL \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh \
  -o install.sh

sh install.sh \
  --base-url https://gh-proxy.com/https://github.com \
  --version 0.0.1 \
  --install-dir "$HOME/bin" \
  --repository lejunyang/one-sdk \
  --target x86_64-unknown-linux-gnu
```

| 参数 | 环境变量 | 作用 |
| --- | --- | --- |
| `--version` | `OSDK_VERSION` | 版本号，可带或不带 `v`；默认 `latest` |
| `--install-dir` | `OSDK_BIN_DIR` | 二进制安装目录 |
| `--repository` | `OSDK_REPOSITORY` | GitHub 的 `owner/repo` |
| `--base-url` | `OSDK_DOWNLOAD_BASE_URL` | GitHub 或下载镜像根地址 |
| `--target` | `OSDK_TARGET` | 覆盖自动识别的平台目标 |
| `--skip-verify` | `OSDK_SKIP_VERIFY=1` | 跳过 SHA-256 校验，不推荐 |
| `--shells` | `OSDK_SETUP_SHELLS` | 要配置的 shell：`all`、`none` 或逗号分隔列表 |
| `--no-modify-shell` | — | 等价于 `--shells none` |
| `--config-dir` | — | 写入为 `OSDK_CONFIG_DIR` 的值 |
| `--data-dir` | — | 写入为 `OSDK_DATA_DIR` 的值 |
| `--cache-dir` | — | 写入为 `OSDK_CACHE_DIR` 的值 |
| `-y`, `--accept-defaults` | `OSDK_ACCEPT_DEFAULTS=1` | 不询问，全部接受默认值 |
| `--print-activation` | — | 把当前 shell 的激活代码输出到 stdout |

`OSDK_CONFIG_DIR`、`OSDK_DATA_DIR`、`OSDK_CACHE_DIR` 这三个环境变量只把默认值换成
你已有的设置，**不会**跳过询问；要跳过请使用上表中对应的参数。

运行 `sh install.sh --help` 查看完整帮助。

### PowerShell 参数

```powershell
Invoke-WebRequest `
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1

.\install.ps1 `
  -BaseUrl https://gh-proxy.com/https://github.com `
  -Version 0.0.1 `
  -InstallDir "$HOME\bin" `
  -Repository lejunyang/one-sdk `
  -Target x86_64-pc-windows-msvc
```

PowerShell 参数为 `-Version`、`-InstallDir`、`-Repository`、`-BaseUrl`、
`-Target`、`-SkipVerify`、`-Shells`、`-NoModifyShell`、`-ConfigDir`、`-DataDir`、
`-CacheDir` 和 `-AcceptDefaults`，也支持上表中的环境变量。

::: tip 安装校验
两个安装器默认下载 Release 中的 `SHA256SUMS` 并验证归档。只有在你已经通过
其他可信渠道验证文件时，才应跳过校验。
:::

## 从源码构建

需要 Rust 1.91.1 或更新版本；仓库默认固定 Rust 1.98.0：

```bash
git clone https://github.com/lejunyang/one-sdk.git
cd one-sdk
cargo build --locked --release
```

构建结果位于：

```text
target/release/osdk
target/release/osdk-shim
```

请把两个文件放在同一目录，并将该目录加入 `PATH`。`osdk-shim` 由 osdk 生成的 shim 间接
调用，通常不需要由用户直接运行。

如果已经安装 Rust，也可以从 crates.io 安装主命令：

```bash
cargo install osdk-cli --locked
```

`osdk-shim` 是独立 package。需要完整的日常安装时仍推荐上面的 Release 安装器，
它会把 `osdk` 和 `osdk-shim` 两个同版本程序一起安装。

在中国大陆可以先配置 rustup 镜像：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf \
  https://rsproxy.cn/rustup-init.sh | sh -s -- -y
```

## 保持 osdk 更新

装好之后，osdk 可以自行更新，不需要再跑一遍安装脚本：

```bash
# 只查看有什么可用版本，不做任何改动
osdk self upgrade --dry-run

# 下载最新发行版并替换当前安装
osdk self upgrade
```

`osdk` 与 `osdk-shim` 始终一起替换，下载内容会与发行版校验和比对。下载源会像工具
下载一样做测速，因此当 GitHub 镜像更快时会自动走镜像：

```bash
osdk source test self          # 实测各个候选源
osdk source pin self ghproxy   # 固定使用镜像
osdk --source github self upgrade   # 只对这一次生效
```

要安装指定版本（包括退回到更早的版本）：

```bash
osdk self upgrade --version 0.0.1
```

::: tip
`cargo install osdk-cli` 只安装主命令，这样装出来的环境里没有 `osdk-shim` 可供
更新。请改用 Release 安装脚本，或用 `cargo install` 自行管理。
:::

## 验证安装

```bash
osdk --version
osdk doctor
```

`doctor` 会显示数据目录、缓存目录、内容存储、安装目录、链接模式和后端状态。

## 第一次使用

```bash
# 安装 Node.js
osdk install node@20

# 设为全局默认并生成 shims
osdk use -g node@20

# 检查实际版本
node --version
```

也可以使用 shell 激活，不依赖固定 shim。安装器已经为选中的 shell 写好了这一步，
手动接入时：

```bash
eval "$(osdk activate bash)" # 也支持 zsh、fish、powershell
```

[继续阅读详细功能 →](/guide/features)
