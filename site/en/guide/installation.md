# Installation

osdk provides prebuilt binaries for Windows, macOS, and Linux. Each release
archive installs two sibling programs: the main `osdk` CLI and `osdk-shim`,
which launches active tools. Keep both in the same directory.

## Linux and macOS

Run the one-line installer:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

The default destination is `~/.local/bin`. The installer then walks you through
shell setup, described in [Shell setup](#shell-setup) below.

## Windows

Run this in PowerShell:

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

The default destination is `%LOCALAPPDATA%\Programs\osdk\bin`.

## Shell setup

Once the binaries are in place, the installer:

1. detects the shells present on your system, lists each one's startup file,
   and lets you select which of them to configure;
2. asks for `OSDK_CONFIG_DIR`, `OSDK_DATA_DIR` and `OSDK_CACHE_DIR`, proposing
   a default for each that you can accept by pressing Enter;
3. validates every directory: it must be absolute, creatable, and actually
   writable. An unusable answer is explained and asked again;
4. writes a marked block into each selected shell's startup file, exporting
   those variables, putting the binary directory on `PATH`, and invoking
   `osdk activate`.

The proposed defaults are the locations osdk derives on its own, so accepting
them relocates nothing — it only makes the layout explicit. The per-platform
defaults are listed in
[Storage, Shell, and Diagnostics](./storage-shell#directory-layout-and-overrides).

bash, zsh, fish, and PowerShell are supported. On Windows, Windows PowerShell
and PowerShell 7 keep separate profiles and are offered separately.

The block looks like this:

```text
# >>> osdk initialize >>>
...
# <<< osdk initialize <<<
```

Rerunning the installer **replaces** that block rather than appending a second
one. Everything outside it is preserved, and the original file is first backed
up as `<startup file>.osdk-backup`. Delete the whole block to remove the
integration.

### Activating the shell you are in

Writing a startup file only affects new shells. The Windows installer also
activates the session that launched it. On Unix a child process cannot modify
its parent shell, so `--print-activation` writes the activation code to stdout
and diverts everything else to stderr, making it safe to eval:

```bash
eval "$(sh install.sh --print-activation)"
```

You can also activate the current shell by hand after installing:

```bash
eval "$(osdk activate bash)"        # zsh works the same way
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

### Unattended installation

Every prompt has a matching flag, and passing it suppresses that prompt:

```bash
sh install.sh --shells bash,zsh \
  --config-dir "$HOME/.config/osdk" \
  --data-dir "$HOME/.local/share/osdk" \
  --cache-dir "$HOME/.cache/osdk"

sh install.sh --accept-defaults     # configure every detected shell, all defaults
sh install.sh --no-modify-shell     # install the binaries, touch no startup file
```

```powershell
.\install.ps1 -Shells pwsh -ConfigDir "D:\osdk\config" `
  -DataDir "D:\osdk\data" -CacheDir "D:\osdk\cache"

.\install.ps1 -AcceptDefaults
.\install.ps1 -NoModifyShell
```

Unix prompts read `/dev/tty` rather than stdin, so the piped `curl ... | sh`
form stays interactive. With no terminal and no shell-setup flags, no startup
file is modified.

## Customize the installation

Piping is convenient for the defaults. Download the script first when passing
arguments.

### Unix options

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh

sh install.sh \
  --version 0.0.1 \
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
| `--shells` | `OSDK_SETUP_SHELLS` | Shells to configure: `all`, `none`, or a comma-separated list |
| `--no-modify-shell` | — | Equivalent to `--shells none` |
| `--config-dir` | — | Value to export as `OSDK_CONFIG_DIR` |
| `--data-dir` | — | Value to export as `OSDK_DATA_DIR` |
| `--cache-dir` | — | Value to export as `OSDK_CACHE_DIR` |
| `-y`, `--accept-defaults` | `OSDK_ACCEPT_DEFAULTS=1` | Never prompt; accept every default |
| `--print-activation` | — | Print activation code for the current shell to stdout |

The `OSDK_CONFIG_DIR`, `OSDK_DATA_DIR` and `OSDK_CACHE_DIR` variables only
replace the proposed defaults with your existing setup; they do **not** suppress
their prompts. Use the flags above for that.

Run `sh install.sh --help` for the complete help text.

### PowerShell options

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1

.\install.ps1 `
  -Version 0.0.1 `
  -InstallDir "$HOME\bin" `
  -Repository lejunyang/one-sdk `
  -Target x86_64-pc-windows-msvc
```

PowerShell accepts `-Version`, `-InstallDir`, `-Repository`, `-BaseUrl`,
`-Target`, `-SkipVerify`, `-Shells`, `-NoModifyShell`, `-ConfigDir`, `-DataDir`,
`-CacheDir`, and `-AcceptDefaults`, plus the environment variables in the table.

::: tip Verification
Both installers download `SHA256SUMS` from the release and verify the archive
by default. Skip verification only if you have authenticated the artifact
through another trusted channel.
:::

## Build from source

Rust 1.91.1 or newer is required; the repository pins Rust 1.98.0 by default:

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

If Rust is already installed, you can install the primary command from
crates.io:

```bash
cargo install osdk-cli --locked
```

`osdk-shim` is a separate package. The Release installer above remains the
recommended complete installation because it installs same-version `osdk` and
`osdk-shim` programs together.

In mainland China, you may want to configure a rustup mirror first:

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf \
  https://rsproxy.cn/rustup-init.sh | sh -s -- -y
```

## Keep osdk up to date

Once osdk is installed, it updates itself; there is no need to rerun the
installer:

```bash
# Check what is available without changing anything
osdk self upgrade --dry-run

# Download the latest release and replace this installation
osdk self upgrade
```

`osdk` and `osdk-shim` are always replaced together, and the download is
verified against the release checksums. Downloads are speed-probed the same way
tool downloads are, so a GitHub mirror is used automatically when it is the
faster route:

```bash
osdk source test self          # measure the candidates
osdk source pin self ghproxy   # always use the mirror
osdk --source github self upgrade   # override once
```

To install a specific release, including going back to an earlier one:

```bash
osdk self upgrade --version 0.0.1
```

::: tip
`cargo install osdk-cli` installs only the main command, so an installation
made that way has no `osdk-shim` to update. Use the Release installer, or
`cargo install`, to manage it instead.
:::

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

You can use shell activation instead of relying on fixed shims. The installer
already wires this up for the shells you selected; to do it by hand:

```bash
eval "$(osdk activate bash)" # zsh, fish, and powershell are supported too
```

[Continue to the feature reference →](/en/guide/features)
