# osdk — one SDK manager

[简体中文](README.zh-CN.md) · **English** ·
[Website](https://lejunyang.github.io/one-sdk/) ·
[中文文档](https://lejunyang.github.io/one-sdk/) ·
[English docs](https://lejunyang.github.io/one-sdk/en/)

A single cross-platform CLI (Windows/macOS/Linux) that manages many language
SDKs and their versions: **node, npm, pnpm, yarn, java, maven, gradle, kotlin,
python, rust, go, deno, bun** — with three things existing single-purpose
managers (nvm/fnm/uv/sdkman/rustup) don't do together:

1. **Cross-version content dedup.** A content-addressed store (blake3) keeps one
   copy of every identical file; each installed version is materialized from the
   store via hardlink / reflink / copy. Two node minors that share files cost
   disk once, not twice.
2. **Unified downstream package caches.** npm/pnpm/yarn/pip/go/cargo/gradle
   global caches are pointed at one shared root so different projects and SDK
   versions reuse already-downloaded dependencies.
3. **Multi-source with automatic fastest-mirror selection.** Every SDK ships an
   official source plus authoritative mirrors; `osdk` probes them and uses the
   fastest, with failover on both metadata and downloads. You can add custom
   sources or pin one.

## Install

Download the latest prebuilt release on Linux or macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

On Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

Both installers verify the release archive against `SHA256SUMS`. Download the
script first when passing custom options:

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh
sh install.sh --version 0.1.0 --install-dir "$HOME/bin"
```

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 -OutFile install.ps1
.\install.ps1 -Version 0.1.0 -InstallDir "$HOME\bin"
```

Use `--help` on Unix or `Get-Help .\install.ps1 -Detailed` on PowerShell for
the complete parameter list. `OSDK_VERSION`, `OSDK_BIN_DIR`,
`OSDK_REPOSITORY`, `OSDK_DOWNLOAD_BASE_URL`, and `OSDK_TARGET` provide
environment-based overrides.

### Build from source

Rust is required. In mainland China, use a mirror (the official
`static.rust-lang.org` is often unusably slow):

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y
cargo build --release        # binaries: target/release/{osdk,osdk-shim}
```

Maintainers publish a new binary release by bumping
`workspace.package.version` and pushing a commit to `main` whose commit message
contains the exact marker `[publish]`. A normal commit never runs the binary
release workflow.

## Quick start

```bash
osdk install node@20            # install (auto-picks fastest mirror)
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk use -g node@20             # install + set global default + generate shims
osdk use node@18                # pin in the current project (osdk.toml)
node --version                  # runs the active version via shim

# shell activation (alternative to shims — per-directory PATH + env)
eval "$(osdk activate bash)"    # add to ~/.bashrc  (zsh|fish|powershell too)
# later, remove the hook and restore PATH/env in the current shell:
eval "$(osdk deactivate bash)"
```

PowerShell activation guards its command-lookup callback against re-entry.
Windows shims also launch `.cmd` and `.bat` tools explicitly through `ComSpec`,
preserving batch arguments, stdio, and exit codes.

Downloads retry transient failures and safely resume validated partial files
with HTTP `Range`/`If-Range`. Use `--offline` after a successful online run to
resolve metadata and reinstall artifacts entirely from the osdk cache.

Project version files are honored (walk-up): `osdk.toml`, `.tool-versions`
(asdf-compatible), and idiomatic files (`.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`). Node also
reads `package.json#engines.node` and `devEngines.runtime` as npm semver ranges.
Priority is global across the walk-up tree: `osdk.toml` > `.tool-versions` >
`.nvmrc` > `.node-version` > `package.json` > user-global config.

## Node workflows

Override the Node artifact architecture while resolving a lock:

```bash
osdk lock node@20 -o arch=arm64
```

The target architecture is saved in the matching platform lock section.
Install and execution reject cross-architecture artifacts because osdk has no
download-only mode. Enable Corepack with `-o corepack=true` or persist
`corepack = true` under `[settings.node]`; osdk invokes only that installation's
Corepack and rolls the install back if enabling shims fails.

Migrate portable global npm packages between managed Node versions:

```bash
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

The default is a dry-run. npm itself and packages marked with native build or
install scripts are skipped. `--apply` uses the target Node's managed npm with
its bin directory first on `PATH`; on failure, the target's previous global
package set is restored.

## Python implementations and catalogs

The short form remains CPython:

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
osdk python find pypy-3.11
```

The full identity is
`python@<implementation>-<version>+<variant>`; implementation and variant are
persisted in `osdk.lock`, so regular and free-threaded CPython can coexist.
`python find` reports managed, `PATH`, and system interpreters in that order.

The built-in known-good catalog is derived from uv download metadata at a fixed
commit and every entry has a SHA-256. Configure a larger internal or refreshed
catalog only with both fields:

```toml
[settings.python]
catalog_url = "https://example.test/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

Local paths and `file://` URLs are supported. A new catalog replaces last-good
cache only after its exact digest, schema, implementation, variant, and every
artifact checksum validate; failure falls back to last-good, then built-in.
Pre-release policy is `if-explicit` by default:

```bash
osdk --prerelease never install python@3.15.0rc1
osdk --prerelease allow install python@latest
```

`latest` does not select a pre-release unless policy is `allow`; `never` rejects
pre-releases even when explicitly requested.

## Java runtimes and JVM tools

Java defaults to a Temurin JDK, while package type is explicit and locked:

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu -o package-type=jdk
```

JRE identities use the `jre-` prefix, so the same Java version can coexist as
JDK and JRE. Foojay results are filtered by runtime type and host libc. A
built-in Temurin LTS catalog (8, 11, 17, 21, and 25) resolves with an empty
offline cache; verified locked artifacts install without contacting Foojay.
Set a Foojay-compatible packages mirror or static endpoint when needed:

```toml
[settings.java]
catalog_url = "https://mirror.example.test/disco/v3.0/packages"
```

Maven, Gradle, and Kotlin are independent candidates rather than Java options:

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

Each has its own install identity, shims, built-in stable candidate, and
upstream SHA-512 or SHA-256 checksum. All use the shared offline/cache/lock
pipeline and never call a user-global Java installation during install.

## Rust lifecycle management

Rust remains delegated to rustup, but every lifecycle command injects osdk's
isolated `RUSTUP_HOME` and `CARGO_HOME`:

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust component remove rustfmt --toolchain stable
osdk rust component list --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust target remove x86_64-pc-windows-gnu --toolchain stable
osdk rust target list --toolchain stable
osdk rust check --repair
```

`check` prints isolated rustup update status; `--repair` reconciles real rustup
toolchains with osdk markers. osdk project pins remain the default directory
selection mechanism. Compatibility with rustup overrides is explicit:

```bash
osdk rust override import [path]
osdk rust override export [path]
osdk rust toolchain link local-dev /absolute/toolchain
```

Import writes the isolated rustup override to `osdk.toml`; export writes the
active osdk pin to isolated rustup. Linked toolchains expose their local `bin`
directory but are rejected from reproducible remote lock artifacts.

## Project package managers

osdk reads exact Corepack-style selections from `package.json#packageManager`
and `devEngines.packageManager`:

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "pnpm@9.15.0"
}
```

`npm`, `pnpm`, and `yarn` are supported. Priority is `osdk.toml [tools]` >
`packageManager` > `devEngines.packageManager`. Missing versions, unsupported
managers, URLs, and hash/build suffixes fail explicitly.

The npm backend installs the `npm` registry package independently from Node and
verifies npm SRI:

```bash
osdk install npm@11.5.2
osdk uninstall npm@11.5.2
```

Selecting npm/pnpm/Yarn automatically adds managed Node. Runtime PATH places
the package-manager bin first and exact managed Node second, never user-global
Node. Locks persist both exact versions and support metadata-free offline
reinstall.

## Reproducible projects and execution

Resolve the current project to exact, platform-specific versions:

```bash
osdk lock                         # writes/merges osdk.lock
osdk install                      # consumes the matching platform lock
osdk outdated                     # compare installed vs current resolution
osdk upgrade                      # install current resolutions + refresh lock
```

`osdk.lock` keeps independent sections for Linux, macOS, and Windows, including
the original request, exact resolved version, backend options, and—after the
tool has been installed—the exact artifact URL, filename, verified checksum,
and any authenticated Sigstore evidence. No-argument installs use the locked
artifact identity without re-querying an upstream release registry. Evidence
in the lock is an audit record, not a trust shortcut: an attestation-enabled
locked reinstall verifies the cached bundle against the cached artifact again.
An explicit `osdk install node@20` still honors the explicit request.

## Project configuration trust

Plain project `[tools]` pins and `[aliases]` are safe data and load without a
trust prompt. Any other project-level section can affect behavior or download
sources and must be explicitly trusted before osdk loads any of it:

```bash
osdk --yes trust                 # trust nearest osdk.toml
osdk --yes trust ./osdk.toml     # trust a specific file
osdk trust list                  # show active/stale trust records
osdk untrust                     # revoke nearest project config
```

Trust records bind both the canonical path and a BLAKE3 hash of normalized TOML
content. Editing or moving the file invalidates trust; symlinks resolve to
their real target, and path traversal cannot create another identity. CI may
set `OSDK_TRUSTED_CONFIG_PATHS` to an OS path-list of reviewed files or
directories instead of persisting local trust. Trust commands load only the
user configuration, so an untrusted project cannot influence its own approval.

Run a command with managed tools without changing project pins:

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool python@3.12 -- python -c "print('ok')"
```

Generate shell completions with `osdk completions bash|zsh|fish|powershell`.

Define reusable version aliases in the user config:

```bash
osdk alias set node default 20
osdk alias set node maintenance default
osdk alias list node
osdk use node@maintenance
osdk alias unset node maintenance
```

Aliases may point to another alias; cycles and reserved names such as `latest`,
`lts`, and `system` are rejected. Tool-name aliases are canonicalized, so
`osdk alias set nodejs default 20` stores the alias under `node`.

## Sources / mirrors

```bash
osdk source list node                 # show sources + which is pinned
osdk source test node                 # probe throughput, print ranking
osdk source pin node tuna             # always use a given source
osdk source add node --id mycorp \
  --download-url https://mirror.corp/node/ \
  --index-url    https://mirror.corp/node/index.json
osdk --source official install go@1.22 # one-shot override
```

The built-in Go sources are `go.dev`, Aliyun, and `golang.google.cn`.
`github:owner/repo` uses the same ordered source list for GitHub Releases API
metadata, release assets, raw files, checksum/signature files, and attestation
bundles. Its built-in `ghproxy` source rewrites all of these through
`https://gh-proxy.com/`. GitHub tokens are sent only to the official
`api.github.com` host and are never forwarded to third-party proxies.

## Dedup & caches

```bash
osdk doctor                     # dirs, same-filesystem check, link mode, backends
osdk prune --dry-run            # inspect GC without deleting
osdk --yes prune                # confirm GC non-interactively
osdk cache dir                  # shared cache/store locations
osdk cache env                  # the downstream package-cache redirections
osdk --yes cache clean          # remove downloaded archives
```

Destructive operations (`uninstall`, `cache clean`, and non-dry-run `prune`)
share one confirmation policy. Interactive terminals prompt with a localized
question. Non-interactive runs fail instead of waiting for input unless
`--yes`, `OSDK_YES=true`, or `settings.yes = true` explicitly confirms them.
`--quiet` never implies consent.

## Language (i18n)

osdk speaks English and Chinese. It auto-detects from your locale
(`LC_ALL`/`LC_MESSAGES`/`LANG`, e.g. `zh_CN.UTF-8` → Chinese) and localizes all
messages, errors, and `-h/--help`. Override precedence (highest first):

```bash
osdk --lang zh install node@20   # per-invocation flag
export OSDK_LANG=zh              # environment
# or in config.toml:  [settings]\n lang = "zh"
```

## Directories (override with env)

| Purpose            | Default (Linux)              | Override            |
|--------------------|------------------------------|---------------------|
| Data (installs)    | `~/.local/share/osdk`        | `OSDK_DATA_DIR`     |
| CAS store          | `<data>/store`               | `OSDK_STORE_DIR`    |
| Installs           | `<data>/installs`            | `OSDK_INSTALL_DIR`  |
| Cache (downloads)  | `~/.cache/osdk`              | `OSDK_CACHE_DIR`    |
| Config             | `~/.config/osdk/config.toml` | `OSDK_CONFIG_DIR`   |

Keep the store and installs on the same filesystem for hardlinks (osdk falls
back to copy across filesystems; `osdk doctor` warns).

## How each SDK is obtained

| SDK          | Mechanism                                                        |
|--------------|------------------------------------------------------------------|
| node         | official nodejs.org prebuilt archives, `SHASUMS256` verified     |
| go           | go.dev/dl JSON index, per-file sha256                            |
| python       | static PBS release index + Astral release mirror, `SHA256SUMS` verified (no GitHub API) |
| java         | Foojay JDK/JRE metadata + embedded Temurin LTS catalog           |
| maven        | Apache binary archive, SHA-512 verified                           |
| gradle       | Gradle distribution, SHA-256 verified                             |
| kotlin       | Kotlin compiler distribution, SHA-256 verified                    |
| rust         | isolated rustup bootstrap + toolchain home, mirror-selected and sha256 verified |
| pnpm         | official npm platform package, npm SRI verified                  |
| yarn         | `yarn` / `@yarnpkg/cli-dist` npm packages, npm SRI verified      |
| deno         | official `@deno/<platform>` npm package, npm SRI verified        |
| bun          | official `@oven/bun-<platform>` npm package, npm SRI verified    |
| npm          | independent npm registry package, npm SRI verified                |
| `github:owner/repo` | any GitHub release: host-matching asset auto-picked, archives extracted or bare binaries installed |

### GitHub-release tools

Install arbitrary tools published as GitHub releases:

```bash
osdk use -g github:sharkdp/fd          # latest release, host asset auto-picked
osdk install github:cli/cli@2.62.0     # a specific tag
osdk list-remote github:sharkdp/fd     # available release tags
```

Only the generic `github:owner/repo` backend requires the GitHub Releases API.
Set `GITHUB_TOKEN` (or `OSDK_GITHUB_TOKEN`) to raise the direct API rate limit.
The backend can fail over API metadata, raw files, release assets, checksums,
and attestation bundles through gh-proxy without forwarding the token.
Release listing follows pagination, up to 1,000 releases.

Override heuristic asset selection with explicit options:

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1

osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

Regex/template rules must match exactly one asset. `bin`/`bins` selects files
from archives; `rename` requires one selected binary. Missing files remove the
whole install rather than leaving a complete marker. Windows normalizes `.exe`.

Digest-pinned static catalogs bypass Releases API entirely:

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

Schema 1 catalog assets include `name`, `url`, `checksum`, `os`, `arch`, and
optional `libc`. The catalog digest, selected asset, and rules are locked.

## Pre-release channels

The shared `--prerelease never|if-explicit|allow` policy applies to Python,
Bun, Deno, and GitHub releases. The default is `if-explicit`:

```bash
osdk install bun@canary
osdk install deno@beta
osdk install github:owner/repo@1.2.0-beta.1
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

Explicit `canary`, `nightly`, and `beta` resolve npm dist-tags or matching
GitHub prerelease tags to exact versions. `never` rejects all prereleases;
only `allow` lets latest/prefix/range select one implicitly. Remote lists stay
stable-only. Locks retain the original channel and exact resolved version, so a
disappearing dist-tag does not break offline reproduction.

## Offline mode

Successful online metadata requests and downloaded archives are cached by URL
and tool version. Later commands can prohibit network access:

```bash
osdk install bun@1.3.14
osdk uninstall bun@1.3.14
osdk --offline install bun@1.3.14
```

An offline cache miss fails explicitly instead of silently attempting the
network. Source probing and source refresh are also disabled offline.

Signature verification is enabled by default where a trusted key is available.
Set `OSDK_VERIFY_SIGNATURES=false` only when intentionally opting out.
Set `OSDK_REQUIRE_CHECKSUMS=true` (or pass `--require-checksums`) to reject any
artifact for which neither upstream metadata nor a lock/cache receipt provides
a verifiable SHA-256/SHA-512/BLAKE3 value.

The generic `github:owner/repo` backend can additionally verify GitHub artifact
attestations:

```bash
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

The same policy is configurable as `settings.attestations` or
`OSDK_ATTESTATIONS=off|if-available|required`; the default is `off`.
`if-available` permits a release with no attestation, but a malformed,
mismatched, or cryptographically invalid bundle always fails. `required`
also fails when no bundle is available. Verified bundles are cached by
repository and artifact SHA-256, so `--offline` and locked reinstalls can
reverify them without trusting lockfile evidence.

Verification uses the embedded Sigstore public-good trust root, checks the
Fulcio certificate chain and SCT, GitHub Actions OIDC issuer and repository,
artifact signature, DSSE subject digest, Rekor body consistency, and signing
time. It also verifies the Rekor Signed Entry Timestamp against the trusted log
key, the signed checkpoint, the proof's root/tree-size binding, and the Merkle
path for the canonical log entry. All proof checks are offline and any missing,
tampered, or mismatched transparency material fails verification. Newly written
receipt and lock evidence uses the `sigstore-bundle+rekor` kind; legacy
`sigstore-bundle` evidence remains readable but is never used as a trust
shortcut.

## Architecture

- `crates/osdk-core` — library: `Backend` trait, pipeline
  (download→verify→extract→CAS ingest→materialize), CAS store + link modes,
  source selection, config/dirs, shim + activation.
- `crates/osdk-cli` — the `osdk` binary.
- `crates/osdk-shim` — a tiny launcher; each shim resolves the active version
  from the cwd and execs the real binary.

## Development

The offline backend contract is the primary correctness gate. It runs the same
resolve → install → execute → uninstall assertions for every built-in backend
and generic GitHub, plus real locked-fixture installs for each registry backend.
Local fault injection covers 403, 429, 5xx, timeout, connection interruption,
malformed metadata, stale cache, interrupted downloads, concurrent installs,
failed-marker cleanup, corrupt receipts/manifests, cross-filesystem copy
fallback, and shim stdio/exit-code/recursion/conflict behavior. The scheduled
live upstream smoke only detects ecosystem drift; it is not the correctness
proof.

```bash
cargo test --workspace         # unit tests
cargo clippy --workspace --all-targets   # lints (CI runs with -D warnings)
cargo fmt --all --check        # formatting

# Windows runtime matrix (run on Windows after building the binaries):
pwsh -File scripts/windows-runtime-smoke.ps1 -BinDir target/debug

# Windows cfgs are also validated by cross-compiling from Linux:
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y mingw-w64
cargo clippy --locked --workspace --all-targets \
  --target x86_64-pc-windows-gnu -- -D warnings

# Execute the complete Windows GNU test workspace through a pinned Wine build:
./scripts/windows-wine-tests.sh
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy + tests on
ubuntu/macos/windows. A separate native macOS terminal gate runs the interactive
PTY contract on both Apple Silicon and Intel runners. The Windows runner
additionally executes `.cmd`,
PowerShell, and Git Bash shims, PowerShell activation/deactivation, symlink
fallbacks, actual NTFS volume detection, stdio/arguments/exit codes, and
space/Chinese paths plus managed SDK state beyond the legacy 260-character
limit, entirely offline under temporary directories. The executable and working
directory stay below the shell's own process-launch limit.
Namespaced backend IDs such as `github:owner/repo` are also covered so cache,
lock, install, and extraction scratch paths remain valid on Windows. A dedicated
Linux job cross-lints every `#[cfg(windows)]` target and executes the full
Windows GNU workspace through a SHA-256-pinned Wine build before the native
Windows/MSVC runner finishes. A separate Rust 1.88 job checks the declared MSRV
against the locked dependency graph. CI jobs and the Windows runtime matrix have
hard time limits, and the runtime script emits one log group per shell contract
so a blocked process is visible instead of running indefinitely.

## License

MIT
