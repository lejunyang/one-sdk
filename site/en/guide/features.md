# Feature Reference

This page covers osdk commands and configuration by workflow.

## Installation and concurrency

Version requests use the consistent `<tool>@<version>` form:

```bash
osdk install node@20
osdk install python@3.12
osdk --jobs 4 install node@20 go@1.22 python@3.12
```

`latest`, `stable`, `lts`, and version prefixes resolve to exact available
versions. Multiple tools can install concurrently. Downloads retry transient
failures and safely resume when the server supports it.

Some backends accept options:

```bash
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk install java@21 -o distribution=zulu
```

## Project and global versions

```bash
osdk use node@20       # Write the current project's osdk.toml
osdk use -g node@20    # Write the user-level configuration
osdk current           # Show active versions for this directory
osdk where node        # Print an installation directory
```

Example project configuration:

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
```

osdk walks upward from the current directory and uses the nearest project
configuration, so subdirectories can inherit repository-wide versions.

### Node project versions, architecture, and Corepack

Node also reads `package.json#engines.node` and `devEngines.runtime`, selecting
the highest stable version matching the npm semver range. Priority is global
across the walk-up tree: `osdk.toml` > `.tool-versions` > `.nvmrc` >
`.node-version` > `package.json` > user-global config. Invalid ranges fail
explicitly.

```bash
osdk lock node@20 -o arch=arm64
osdk install node@20 -o corepack=true
```

`arch=x64|arm64|x86|arm` is saved under the target platform lock section. Since
osdk has no download-only mode, install rejects an architecture that cannot run
on the current host. Corepack can also be enabled with
`corepack = true` under `[settings.node]`; osdk invokes only the selected
Node's Corepack and does not leave a complete marker after failure.

Global package migration is a dry-run by default:

```bash
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

Migration excludes npm itself and packages with native builds or install
scripts. Apply uses only managed npm and restores the target's previous global
package set after failure.

## Python implementations, variants, and catalogs

`python@3.14` remains the CPython shorthand. Full requests include:

```bash
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

Implementation, exact version, and variant are persisted in the lockfile.
Regular, free-threaded, and debug CPython use distinct identities and can
coexist. Discover local interpreters with:

```bash
osdk python find
osdk python find pypy-3.11
```

Output is layered and deduplicated as managed, `PATH`, then system. The embedded
known-good catalog is pinned to a uv download-metadata commit and every entry
has SHA-256. A remote or local catalog must include a pinned digest:

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

Only a catalog whose digest, schema, and every artifact checksum validate can
replace last-good cache. Failure leaves old cache intact. Pre-release behavior
defaults to `if-explicit`, so `latest` never selects an RC unexpectedly:

```bash
osdk --prerelease never install python@3.15.0rc1
osdk --prerelease allow install python@latest
```

## Java JDK/JRE and JVM tools

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu
```

`package-type=jdk|jre` is persisted in the lockfile. JRE uses a
`jre-<version>` identity, so it can coexist with the same JDK version. Foojay
packages are filtered by type and host libc. Embedded Temurin LTS versions
8/11/17/21/25 resolve with an empty offline cache, and an existing locked
artifact installs while Foojay is unavailable.

Point metadata at a Foojay-compatible mirror or static endpoint:

```toml
[settings.java]
catalog_url = "https://mirror.example.test/disco/v3.0/packages"
```

JVM tools are managed as independent backends:

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

Each has an independent version, directory, and shim, with Apache SHA-512,
Gradle SHA-256, or Kotlin SHA-256 verification. Offline and lock behavior is
the same as other backends.

## Rust components, targets, and toolchains

All commands below use osdk's isolated `RUSTUP_HOME` and `CARGO_HOME`:

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust component remove rustfmt --toolchain stable
osdk rust component list --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust target remove x86_64-pc-windows-gnu --toolchain stable
osdk rust target list --toolchain stable
osdk rust check --repair
```

`check` parses isolated rustup status. Repair creates missing markers and
removes markers without a real toolchain. osdk project configuration remains
the default directory-selection mechanism; rustup override compatibility is
explicit:

```bash
osdk rust override import ./repo
osdk rust override export ./repo
osdk rust toolchain link local-dev /absolute/toolchain
```

Linked toolchains can execute locally but are explicitly rejected from
reproducible lock artifacts.

## packageManager and independent npm

```json
{
  "engines": { "node": ">=20 <23" },
  "packageManager": "npm@11.5.2"
}
```

osdk supports exact `npm|pnpm|yarn@version` values from `packageManager` and
`devEngines.packageManager`. Priority is `osdk.toml [tools]` >
`packageManager` > `devEngines.packageManager`. Missing versions, URL/hash
suffixes, and unsupported managers fail explicitly.

The independent npm backend downloads the registry `npm` package and verifies
SRI. Selecting any manager automatically adds managed Node; manager bins
precede exact Node on PATH, so launchers never use user-global Node. Both exact
versions are locked and support offline reinstall.

## Project configuration trust

Project `[tools]` pins and `[aliases]` are safe data and load without trust.
Other project-level sections may affect execution or download sources, so osdk
requires explicit trust before loading any of those fields:

```bash
osdk --yes trust                 # Trust the nearest osdk.toml
osdk --yes trust ./osdk.toml     # Trust a specific file
osdk trust list                  # Show active or stale records
osdk untrust                     # Revoke the nearest project config
```

Records bind the canonical path and a BLAKE3 hash of normalized TOML content.
Editing or moving the file invalidates trust; symlinks resolve to their real
target, and path traversal cannot create another identity. CI can provide an
OS path-list of reviewed files or directories through
`OSDK_TRUSTED_CONFIG_PATHS` instead of persisting local trust. Trust commands
load only user configuration, so an untrusted project cannot influence its own
approval.

## Shims and shell activation

`osdk use` generates shims. When you run `node`, `python`, or another tool, the
shim resolves the project or global version for the current directory.

You can also install a shell hook that updates `PATH` and tool-specific
environment variables when changing directories:

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

Undo activation in the current shell:

```bash
eval "$(osdk deactivate bash)"
```

Run `osdk reshim` to regenerate all shims after moving installation directories.

## Reproducible lockfiles

```bash
osdk lock
osdk install
osdk outdated
osdk upgrade
```

`osdk.lock` keeps separate Linux, macOS, and Windows sections with:

- the original request and exact resolved version;
- backend options;
- the actual artifact URL and filename;
- verified checksums;
- verified GitHub attestation evidence.

A no-argument `osdk install` uses the locked artifact identity for the current
platform without querying upstream metadata again. Lockfile evidence is an
audit record, not a reason to skip verification: a reinstall verifies the
cached archive again.

## Ephemeral execution

Run a command with selected tools without changing project pins:

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool python@3.12 -- python -c "print('ok')"
osdk exec --tool node@20 --tool pnpm@latest -- pnpm install
```

## Version aliases

```bash
osdk alias set node maintenance 20
osdk alias set node default maintenance
osdk alias list node
osdk use node@default
osdk alias unset node maintenance
```

Aliases can point to other aliases. osdk rejects cycles and reserved names such
as `latest`, `lts`, and `stable`.

## Sources and mirror selection

```bash
osdk source list node
osdk source test node
osdk source pin node tuna
osdk source unpin node
```

By default, osdk runs lightweight probes against candidate sources, caches the
ranking, and falls through when metadata requests or artifact downloads fail.

Add a corporate or private mirror:

```bash
osdk source add node \
  --id mycorp \
  --download-url https://mirror.example.com/node/ \
  --index-url https://mirror.example.com/node/index.json

osdk --source mycorp install node@20
osdk source remove node mycorp
```

The built-in Go sources are `go.dev`, Aliyun, and `golang.google.cn`.
`github:owner/repo` applies the same source order to GitHub Releases API
metadata, release assets, raw files, checksum/signature files, and attestation
bundles. The built-in `ghproxy` source rewrites all of these GitHub URLs through
`https://gh-proxy.com/`. `GITHUB_TOKEN` is sent only to the official
`api.github.com` host and is never forwarded to a third-party proxy.

## Content deduplication

After verification and extraction, every file is placed in a shared store by
its BLAKE3 content hash. Installation directories avoid duplicate copies using:

1. hardlinks;
2. reflinks;
3. regular copies.

```bash
osdk doctor
osdk prune --dry-run
osdk --yes prune
```

`prune` removes only store objects not referenced by any installed version.

## Downstream package caches

osdk provides shared cache environments for common package managers, avoiding
repeated dependency downloads across SDK versions:

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
```

This covers npm/pnpm/Yarn, pip, Go, Cargo, Gradle, and other ecosystems.
`cache clean` removes downloaded archives, not installed SDKs or the content
store.

`uninstall`, `cache clean`, and non-dry-run `prune` share one confirmation
policy. Interactive terminals show a localized prompt; non-interactive runs
fail instead of waiting on stdin. CI and scripts should pass `--yes`, set
`OSDK_YES=true`, or configure `yes = true` under `[settings]`. `--quiet` only
suppresses progress and never grants consent.

## Offline mode

Successful metadata requests and artifacts are cached by URL and tool version:

```bash
osdk install bun@latest
osdk uninstall bun@1.3.14
osdk --offline install bun@1.3.14
```

Offline mode prohibits network access. A cache miss fails explicitly instead
of silently going online; source probing and source refresh are disabled too.

## Integrity, signatures, and attestations

osdk verifies archives using SHA-256, SHA-512, BLAKE3, or npm SRI supplied by
the backend:

```bash
osdk --require-checksums install node@20
export OSDK_REQUIRE_CHECKSUMS=true
```

Strict mode rejects archives without a verifiable checksum. Signature
verification is enabled by default and can be explicitly disabled with
`OSDK_VERIFY_SIGNATURES=false`.

The generic GitHub Release backend also supports GitHub Artifact Attestations:

```bash
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

The policy is also available as `settings.attestations` or
`OSDK_ATTESTATIONS`:

- `off`: the default; do not query attestations;
- `if-available`: absence is allowed, but any discovered invalid bundle fails;
- `required`: a valid bundle must exist.

The implementation checks the Fulcio certificate chain and SCT, GitHub Actions
OIDC issuer and repository, artifact signature, DSSE subject digest, Rekor body
consistency, and signing time. Upstream `sigstore` 0.14 does not yet verify the
Rekor Merkle inclusion proof or Signed Entry Timestamp.

## Arbitrary GitHub Release tools

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

osdk chooses a Release asset matching the host OS and architecture, handling
both archives and bare binaries. Set `GITHUB_TOKEN` or `OSDK_GITHUB_TOKEN` to
raise the direct GitHub API rate limit. API metadata, raw files, release assets,
checksum files, and attestation bundles can all fail over through gh-proxy
without forwarding the token.

Release API pagination covers up to 1,000 releases. Complex releases can use
explicit rules:

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1
osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

Zero or multiple matches fail. Multi-binary materialization is atomic, and
Windows normalizes `.exe`.

Static catalog mode never calls GitHub API:

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

Schema 1 assets include URL, checksum, OS, architecture, and optional libc.
Digest, selected asset, and rules are persisted in the lockfile.

## Pre-release policy and channels

```bash
osdk install bun@canary
osdk install deno@beta
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

`if-explicit` is the default for Python, Bun, Deno, and GitHub. Explicit
`canary|nightly|beta` maps a dist-tag or prerelease tag, while the lock stores
the original channel and exact version. `never` rejects all prereleases; only
`allow` lets latest/prefix/range select one implicitly. Default lists remain
stable-only.

## Declarative backends

In addition to built-in backends, osdk can load schema 1 TOML backends from
isolated user configuration and data plugin directories. A declarative backend
describes a version index, platform mappings, download templates, executables,
and checksum rules while retaining the standard verification, extraction, and
content-store pipeline.

## Internationalization

The CLI automatically selects English or Chinese from `LC_ALL`,
`LC_MESSAGES`, or `LANG`. Override it with:

```bash
osdk --lang en install node@20
export OSDK_LANG=en
```

Or set it in the user configuration:

```toml
[settings]
lang = "en"
```

## Reliability contract

CI's local backend contract runs a shared resolve/install/execute/uninstall
lifecycle across every built-in backend and generic GitHub. Local HTTP fixtures
inject 403, 429, 5xx, timeout, disconnect, malformed metadata, and stale cache.
It also verifies concurrent install locks, no complete marker after failure,
corrupt receipts/manifests, cross-filesystem copy fallback, and shim I/O, exit
codes, recursion, and command conflicts. Public live smoke is only for upstream
drift detection.

## Completions and diagnostics

```bash
osdk completions bash
osdk completions zsh
osdk completions fish
osdk completions powershell
osdk doctor
osdk config path
osdk config list
```

## Directory overrides

| Purpose | Linux default | Environment variable |
| --- | --- | --- |
| Data root | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| Content store | `<data>/store` | `OSDK_STORE_DIR` |
| SDK installations | `<data>/installs` | `OSDK_INSTALL_DIR` |
| Download cache | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| Configuration | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

The content store and installation directory must share a filesystem to use
hardlinks. `osdk doctor` reports the active link mode and cross-filesystem
issues.
