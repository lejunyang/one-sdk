# osdk — one SDK manager

**English** · [简体中文](README.zh-CN.md) ·
[Documentation](https://lejunyang.github.io/one-sdk/en/) ·
[Releases](https://github.com/lejunyang/one-sdk/releases)

osdk gives Windows, macOS, and Linux projects one CLI for language runtimes,
package managers, developer tools, and model snapshots. Use it to:

- install and switch complete project toolchains with one command style;
- keep platform-aware project locks that teammates and CI can reuse;
- choose responsive SDK mirrors and dependency registries automatically;
- work from downloaded metadata and artifacts when the network is unavailable;
- manage Hugging Face and ModelScope snapshots alongside development tools;
- inspect storage, caches, active versions, and environment health in English or
  Chinese.

Start with the [getting-started guide](site/en/guide/getting-started.md), or see
the [complete feature overview](site/en/guide/features.md).

## Install

Linux and macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

The installers download the latest release and verify it against
`SHA256SUMS`. To choose a version or destination, download the script first:

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

If GitHub downloads are slow, route both the installer and release downloads
through a trusted proxy:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

See [Installation](site/en/guide/installation.md) for PATH setup, installer
options, source builds, and verification.

## Quick start

```bash
# Install several runtimes; downloads can run concurrently.
osdk --jobs 4 install node@20 python@3.12 go@1.22

# Choose a user-wide default.
osdk use -g node@20

# Pin a version for the current project.
osdk use python@3.12

# See what this directory will use.
osdk current

# Enable automatic per-directory switching.
eval "$(osdk activate bash)"

node --version
python --version
```

Activation also supports zsh, fish, and PowerShell. Run `osdk --help` or
`osdk <command> --help` whenever you need the full command reference.

## Scenario: make a project toolchain reproducible

Pin tools in the repository, resolve them, and install the matching lock for
the current platform:

```bash
osdk use node@20
osdk use python@3.12
osdk use go@1.22
osdk lock
osdk install
```

Check for newer matching versions or run a command without changing project
pins:

```bash
osdk outdated
osdk upgrade
osdk exec --tool node@20 -- node --version
```

For immutable Rust reproduction, pin an explicit or dated toolchain. Floating
rustup channels such as `stable`, `beta`, and `nightly` remain floating when
written to the lock.

osdk can also follow existing `.tool-versions`, `.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`, and Node
version declarations in `package.json`.
Data-only declarative backends use the same locked artifact URL, checksum,
download cache, and offline reinstall path as built-in archive backends.
For osdk-owned dynamic `npm:<package>` and `github:owner/repo` installs, options that select
the installer, build policy, asset, platform, or layout are part of the
installation identity. osdk records that identity in `.osdk-install.json`
schema 1 and places each `b3-v2:` identity under its own fingerprinted install
root, so multiple identities of the same backend and version can coexist.
Reuse, activation, shims, `where`, `uninstall`, and `reshim` all select the exact
configured identity. Older `.osdk-tool.json` manifests are detected only as
legacy state and are never reused or executed.

Guides: [Project toolchains](site/en/guide/projects.md) ·
[Lockfiles and repeatable environments](site/en/guide/lockfiles.md)

## Scenario: use package managers with an available registry

Install npm, pnpm, or Yarn independently, or let an exact
`package.json#packageManager` selection join the project toolchain:

```bash
osdk install npm@11.5.2
osdk install pnpm@9.15.0
osdk install yarn@4.9.1
```

Before a package-manager process starts, osdk can select a healthy configured
registry for npm, pnpm, Yarn, Bun, and Deno. Inspect the current choice with:

```bash
osdk registry test
osdk registry test pnpm
```

Explicit registry flags, environment variables, private registries, and native
package-manager configuration remain under your control.

Guide: [Package managers and registry selection](site/en/guide/package-managers.md)

## Scenario: add an npm-published developer tool

Prefix a package with `npm:` to distinguish it from the npm package manager. In
a Node project, `use` adds the package to the nearest `package.json`, keeps an
existing dependency section (or defaults to `devDependencies`), and makes its
local command available after shell activation. Activation exposes only an
osdk-curated generation of the configured packages' validated commands, never
the project's entire `node_modules/.bin`:

```bash
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .

# Force a particular installer when the automatic choice is not wanted.
osdk use npm:eslint@9 -o installer=pnpm

# Install a user-wide tool without changing the current project.
osdk use --global 'npm:@antfu/ni@0.21.12' -o installer=aube
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

Project `use` respects the nearest `package.json` and exactly one compatible
existing native lock. Automatic selection prefers Aube when that lock is
compatible; `installer=aube`, `installer=npm`, and `installer=pnpm` select one
explicitly. Shell activation exposes only validated commands from packages
selected by the trusted project configuration, never the project's entire
`node_modules/.bin`. Without a `package.json`, local `use` retains the isolated
osdk-managed installation and shim behavior. The generated `.osdk/npm-bin/`
directory is local derived state and should normally be ignored by version
control. Commit the package manager's native lock alongside `osdk.lock`; the
latter does not replace the transitive dependency graph.

Global npm, pnpm, and Aube choices each run that manager's real global-add
operation inside an osdk-controlled prefix, leaving the ambient Node
installation untouched. Release installs include the `osdk-aube` companion
needed for Aube global mode. Aube 2.1 needs network access for a new or repaired
global install, although an already complete exact install with matching options
can be selected again offline without launching Aube; choose npm or pnpm when
the install itself must use a native offline mode.
`where --global` and `uninstall --global` explicitly target the user-wide npm
installation whose exact configured identity matches. Without that flag, the
existing project/isolated behavior is preserved; uninstall removes only the
selected identity root, while sibling identities of the same package version
remain installed. A global uninstall also removes its user config, lock entry,
and now-unowned shims. Project-managed npm dependencies and their curated
`.osdk/npm-bin` generation remain separate from these osdk-owned roots.

Guide: [npm developer tools](site/en/guide/npm-tools.md)

## Scenario: work in each language ecosystem

The same command style applies across ecosystems, while backend-specific
capability notes live in their guides. Runtime-specific commands cover the
workflows that need them.

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

### Java and JVM tools

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

Guide: [Runtime and ecosystem workflows](site/en/guide/runtimes.md)

## Scenario: pin a model snapshot

Pull selected files from Hugging Face or ModelScope, verify the local snapshot,
and obtain its path:

```bash
export HF_TOKEN=... # optional for private or gated repositories

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

Enable provider endpoint and cache variables for activated shells when model
tools should share the osdk environment:

```bash
osdk model env enable
osdk model env list
osdk model env disable huggingface
```

Guide: [Model snapshots](site/en/guide/models.md)

## Scenario: control sources, offline use, and trust

Let osdk rank available sources, pin a preferred mirror, add a trusted internal
source, or override the source for one command:

```bash
osdk source list node
osdk source test node
osdk source pin node tuna
osdk source add node --id mycorp \
  --download-url https://mirror.corp/node/ \
  --index-url https://mirror.corp/node/index.json
osdk --source official install go@1.22
```

After a successful online download, require cache-only operation with
`--offline`. Tighten artifact policy when your environment requires it:

```bash
osdk --offline install node@20
osdk --require-checksums install github:sharkdp/fd
osdk --attestations required install github:cli/cli@latest
```

Review and explicitly trust project configuration that changes sources or
execution behavior:

```bash
osdk --yes trust ./osdk.toml
osdk trust list
osdk untrust ./osdk.toml
```

Guide: [Sources, offline use, and security](site/en/guide/sources-security.md)

## Scenario: inspect caches and reclaim storage

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
osdk prune --dry-run
osdk --yes prune
```

`cache clean` removes downloaded archives. `prune` reclaims unreferenced shared
content; `prune --dry-run` does not delete anything.

Guide: [Storage, caches, and shell integration](site/en/guide/storage-shell.md)

## Scenario: diagnose an environment or switch language

```bash
osdk doctor
osdk current
osdk where node
osdk config path
osdk config list
osdk --lang zh doctor
OSDK_LANG=en osdk --help
osdk completions bash > osdk.bash
```

osdk localizes commands, help, prompts, errors, and diagnostics in English and
Chinese. `--lang` overrides the locale for one command; `OSDK_LANG` sets the
session preference.

Guide: [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)

## Support matrix

| Area | Supported |
| --- | --- |
| Platforms | Windows, macOS, Linux |
| Runtimes | Node.js, Python, Java JDK/JRE, Go, Rust, Deno, Bun |
| Package and JVM tools | npm, pnpm, Yarn, Maven, Gradle, Kotlin |
| Other developer tools | npm packages through `npm:<package>` and public GitHub Releases through `github:owner/repo` |
| Model providers | Hugging Face, ModelScope |
| Project inputs | `osdk.toml`, `.tool-versions`, common ecosystem version files |
| Shells | Bash, zsh, fish, PowerShell |
| CLI languages | English, Chinese |

## Documentation

- [Feature overview](site/en/guide/features.md)
- [Getting started](site/en/guide/getting-started.md)
- [Project toolchains](site/en/guide/projects.md)
- [Lockfiles and repeatable environments](site/en/guide/lockfiles.md)
- [Runtime and ecosystem workflows](site/en/guide/runtimes.md)
- [Package managers and registry selection](site/en/guide/package-managers.md)
- [npm developer tools](site/en/guide/npm-tools.md)
- [Model snapshots](site/en/guide/models.md)
- [Sources, offline use, and security](site/en/guide/sources-security.md)
- [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)
- [Implementation docs](site/en/guide/implementation/index.md)

Contributions are welcome through
[issues](https://github.com/lejunyang/one-sdk/issues) and pull requests.

## License

MIT
