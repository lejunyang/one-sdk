# 安装

osdk 提供 Windows、macOS 和 Linux 的预编译二进制。安装包同时包含主程序
`osdk` 和负责启动已激活工具的 `osdk-shim`。

## Linux 与 macOS

运行一键安装脚本：

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

默认安装到 `~/.local/bin`。请确认该目录已加入 `PATH`：

```bash
export PATH="$HOME/.local/bin:$PATH"
```

建议将这行加入 `~/.bashrc` 或 `~/.zshrc`。

## Windows

在 PowerShell 中运行：

```powershell
$env:OSDK_DOWNLOAD_BASE_URL = "https://gh-proxy.com/https://github.com"
irm https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

这些示例既代理 Raw 安装脚本，也通过 `OSDK_DOWNLOAD_BASE_URL` 代理脚本后续
下载的 GitHub Release 二进制和 `SHA256SUMS`。

默认安装到 `%LOCALAPPDATA%\Programs\osdk\bin`。如果安装器提示该目录不在
`PATH`，请将它加入当前用户的 `PATH`。

## 自定义安装

通过管道运行适合默认安装；需要传参时，应先下载脚本。

### Unix 参数

```bash
curl -sSfL \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh \
  -o install.sh

sh install.sh \
  --base-url https://gh-proxy.com/https://github.com \
  --version 0.1.0 \
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

运行 `sh install.sh --help` 查看完整帮助。

### PowerShell 参数

```powershell
Invoke-WebRequest `
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1

.\install.ps1 `
  -BaseUrl https://gh-proxy.com/https://github.com `
  -Version 0.1.0 `
  -InstallDir "$HOME\bin" `
  -Repository lejunyang/one-sdk `
  -Target x86_64-pc-windows-msvc
```

PowerShell 参数为 `-Version`、`-InstallDir`、`-Repository`、`-BaseUrl`、
`-Target` 和 `-SkipVerify`，也支持上表中的环境变量。

::: tip 安装校验
两个安装器默认下载 Release 中的 `SHA256SUMS` 并验证归档。只有在你已经通过
其他可信渠道验证文件时，才应跳过校验。
:::

## 从源码构建

需要 Rust 1.88 或更新版本：

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

请把两个文件放在同一目录，并将该目录加入 `PATH`。

在中国大陆可以先配置 rustup 镜像：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf \
  https://rsproxy.cn/rustup-init.sh | sh -s -- -y
```

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

也可以使用 shell 激活，不依赖固定 shim：

```bash
eval "$(osdk activate bash)" # 也支持 zsh、fish、powershell
```

[继续阅读详细功能 →](/guide/features)
