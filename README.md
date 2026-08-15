# osdk — one SDK manager

[Website](https://lejunyang.github.io/one-sdk/) ·
[中文文档](https://lejunyang.github.io/one-sdk/) ·
[English docs](https://lejunyang.github.io/one-sdk/en/)

A single cross-platform CLI (Windows/macOS/Linux) that manages many language
SDKs and their versions: **node, npm, pnpm, yarn, java, python, rust, go, deno, bun** — with
three things existing single-purpose managers (nvm/fnm/uv/sdkman/rustup) don't do
together:

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

Downloads retry transient failures and safely resume validated partial files
with HTTP `Range`/`If-Range`. Use `--offline` after a successful online run to
resolve metadata and reinstall artifacts entirely from the osdk cache.

Project version files are honored (walk-up): `osdk.toml`, `.tool-versions`
(asdf-compatible), and idiomatic files (`.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`).

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

## Dedup & caches

```bash
osdk doctor                     # dirs, same-filesystem check, link mode, backends
osdk prune                      # GC store objects no install references
osdk cache dir                  # shared cache/store locations
osdk cache env                  # the downstream package-cache redirections
```

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
| java         | Foojay Disco API (Temurin default), multi-vendor                 |
| rust         | isolated rustup bootstrap + toolchain home, mirror-selected and sha256 verified |
| pnpm         | official npm platform package, npm SRI verified                  |
| yarn         | `yarn` / `@yarnpkg/cli-dist` npm packages, npm SRI verified      |
| deno         | official `@deno/<platform>` npm package, npm SRI verified        |
| bun          | official `@oven/bun-<platform>` npm package, npm SRI verified    |
| npm          | ships with node                                                  |
| `github:owner/repo` | any GitHub release: host-matching asset auto-picked, archives extracted or bare binaries installed |

### GitHub-release tools

Install arbitrary tools published as GitHub releases:

```bash
osdk use -g github:sharkdp/fd          # latest release, host asset auto-picked
osdk install github:cli/cli@2.62.0     # a specific tag
osdk list-remote github:sharkdp/fd     # available release tags
```

Only the generic `github:owner/repo` backend requires the GitHub Releases API.
Set `GITHUB_TOKEN` (or `OSDK_GITHUB_TOKEN`) to raise its API rate limit.

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
time. The upstream `sigstore` 0.14 verifier does not yet verify the Rekor
Merkle inclusion proof or Signed Entry Timestamp (SET); do not treat this mode
as complete transparency-log proof verification.

## Architecture

- `crates/osdk-core` — library: `Backend` trait, pipeline
  (download→verify→extract→CAS ingest→materialize), CAS store + link modes,
  source selection, config/dirs, shim + activation.
- `crates/osdk-cli` — the `osdk` binary.
- `crates/osdk-shim` — a tiny launcher; each shim resolves the active version
  from the cwd and execs the real binary.

## Development

```bash
cargo test --workspace         # unit tests
cargo clippy --workspace --all-targets   # lints (CI runs with -D warnings)
cargo fmt --all --check        # formatting

# Windows is validated by cross-compiling from Linux:
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y mingw-w64
cargo build --workspace --target x86_64-pc-windows-gnu
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy + tests on
ubuntu/macos/windows, plus a dedicated Linux→Windows cross-build that guards the
`#[cfg(windows)]` paths (shim `.cmd`/bash generation, junctions, volume
detection). A separate Rust 1.88 job checks the declared MSRV against the
locked dependency graph.

## License

MIT
