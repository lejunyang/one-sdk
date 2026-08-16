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
