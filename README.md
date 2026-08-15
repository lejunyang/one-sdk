# osdk — one SDK manager

A single cross-platform CLI (Windows/macOS/Linux) that manages many language
SDKs and their versions: **node, npm, pnpm, yarn, java, python, rust, go** — with
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

## Install (from source)

Rust is required. In mainland China, use a mirror (the official
`static.rust-lang.org` is often unusably slow):

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y
cargo build --release        # binaries: target/release/{osdk,osdk-shim}
```

## Quick start

```bash
osdk install node@20            # install (auto-picks fastest mirror)
osdk use -g node@20             # install + set global default + generate shims
osdk use node@18                # pin in the current project (osdk.toml)
node --version                  # runs the active version via shim

# shell activation (alternative to shims — per-directory PATH + env)
eval "$(osdk activate bash)"    # add to ~/.bashrc  (zsh|fish|powershell too)
```

Project version files are honored (walk-up): `osdk.toml`, `.tool-versions`
(asdf-compatible), and idiomatic files (`.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`).

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
| python       | astral-sh/python-build-standalone (latest-release.json + SHA256SUMS; no GitHub API) |
| java         | Foojay Disco API (Temurin default), multi-vendor                 |
| rust         | delegated to rustup (self-contained home + mirror)               |
| pnpm         | standalone binary (GitHub release, mirror/proxy failover)        |
| yarn         | classic standalone bundle (berry → corepack)                     |
| npm          | ships with node                                                  |
| `github:owner/repo` | any GitHub release: host-matching asset auto-picked, archives extracted or bare binaries installed |

### GitHub-release tools

Install arbitrary tools published as GitHub releases:

```bash
osdk use -g github:sharkdp/fd          # latest release, host asset auto-picked
osdk install github:cli/cli@2.62.0     # a specific tag
osdk list-remote github:sharkdp/fd     # available release tags
```

Set `GITHUB_TOKEN` (or `OSDK_GITHUB_TOKEN`) to raise the API rate limit.

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
detection).

## License

MIT
