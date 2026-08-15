# Installation

osdk provides prebuilt binaries for Windows, macOS, and Linux. Each archive
contains the main `osdk` program and `osdk-shim`, which launches active tools.

## Linux and macOS

Run the one-line installer:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

The default destination is `~/.local/bin`. Make sure it is on `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Add that line to `~/.bashrc` or `~/.zshrc` to persist it.

## Windows

Run this in PowerShell:

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

The default destination is `%LOCALAPPDATA%\Programs\osdk\bin`. Add it to your
user `PATH` if the installer reports that it is missing.

## Customize the installation

Piping is convenient for the defaults. Download the script first when passing
arguments.

### Unix options

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh

sh install.sh \
  --version 0.1.0 \
  --install-dir "$HOME/bin" \
  --repository lejunyang/one-sdk \
  --target x86_64-unknown-linux-gnu
```

| Option | Environment variable | Purpose |
| --- | --- | --- |
| `--version` | `OSDK_VERSION` | Version with or without `v`; defaults to `latest` |
| `--install-dir` | `OSDK_BIN_DIR` | Binary destination |
| `--repository` | `OSDK_REPOSITORY` | GitHub `owner/repo` |
| `--base-url` | `OSDK_DOWNLOAD_BASE_URL` | GitHub or mirror base URL |
| `--target` | `OSDK_TARGET` | Override automatic platform detection |
| `--skip-verify` | `OSDK_SKIP_VERIFY=1` | Skip SHA-256 verification; not recommended |

Run `sh install.sh --help` for the complete help text.

### PowerShell options

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1

.\install.ps1 `
  -Version 0.1.0 `
  -InstallDir "$HOME\bin" `
  -Repository lejunyang/one-sdk `
  -Target x86_64-pc-windows-msvc
```

PowerShell accepts `-Version`, `-InstallDir`, `-Repository`, `-BaseUrl`,
`-Target`, and `-SkipVerify`, plus the environment variables in the table.

::: tip Verification
Both installers download `SHA256SUMS` from the release and verify the archive
by default. Skip verification only if you have authenticated the artifact
through another trusted channel.
:::

## Build from source

Rust 1.88 or newer is required:

```bash
git clone https://github.com/lejunyang/one-sdk.git
cd one-sdk
cargo build --locked --release
```

The binaries are written to:

```text
target/release/osdk
target/release/osdk-shim
```

Place both files in the same directory and add that directory to `PATH`.

In mainland China, you may want to configure a rustup mirror first:

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf \
  https://rsproxy.cn/rustup-init.sh | sh -s -- -y
```

## Verify the installation

```bash
osdk --version
osdk doctor
```

`doctor` displays data, cache, store, and installation directories along with
the link mode and backend status.

## First use

```bash
# Install Node.js
osdk install node@20

# Set it as the global default and generate shims
osdk use -g node@20

# Check the effective version
node --version
```

You can use shell activation instead of relying on fixed shims:

```bash
eval "$(osdk activate bash)" # zsh, fish, and powershell are supported too
```

[Continue to the feature reference →](/en/guide/features)
