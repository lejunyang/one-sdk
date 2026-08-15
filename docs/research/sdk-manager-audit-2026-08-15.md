# SDK manager market and reliability audit

Date: 2026-08-15

Project: `github.com/lejunyang/one-sdk`

## Scope

This audit covers:

- Every SDK backend currently implemented by `osdk`.
- Dependencies on the GitHub REST API and available alternatives.
- Feature comparisons with multi-runtime and language-specific SDK managers.
- Test-suite comparisons and missing regression coverage.
- A prioritized implementation plan that preserves `osdk`'s differentiators.

The repository implements nine built-in backends:

- Node.js, including the npm bundled with Node.js
- Go
- Python
- Java
- Rust
- pnpm
- Yarn
- Deno
- Bun

It also implements a dynamic `github:owner/repo` backend. Although the README
lists ten SDKs, npm is not independently versioned or installed.

## Compared projects

The comparison used the following upstream revisions:

| Project | Revision |
| --- | --- |
| mise | `194818dc78ea9fbc5356d283c6f98cf9cde85ec9` |
| proto | `4c6e8279eb9e566cf8244689d8b1c2dc90123b11` |
| aqua | `d8348d08c7f7865202d37b794ef6369dfe044c55` |
| asdf | `f87a31efc67c041af49f0d3c566840e4f30a12cb` |
| vfox | `346ea08046c4c6e56e29ea8d361946258518ca01` |
| fnm | `86adc9676ceb2a509b21e75e74048b93c89f097d` |
| uv | `f1a42680ff5272232d65748acf338b19778dde24` |
| rustup | `82a0191173cc43c9732baa28c2c017c06d510466` |
| SDKMAN CLI | `f02e5de113ea46a95e5e2fd795eabe6f2b7d4095` |

The comparison is intentionally scoped:

- mise, proto, aqua, asdf, and vfox are used for cross-tool architecture.
- fnm, uv, rustup, and SDKMAN are used for language-specific behavior.
- Task runners, secret managers, and unrelated environment features are not
  automatically treated as missing SDK-manager functionality.

## Existing differentiators

`osdk` already has three useful capabilities that are not commonly combined:

1. A BLAKE3 content-addressed store that deduplicates files across installed
   SDK versions.
2. Shared downstream package caches for npm, pnpm, Yarn, pip, Go, Cargo, and
   Gradle.
3. Multiple download sources with throughput probing, ranking, pinning, and
   failover.

These should remain first-class behavior while reliability and compatibility
features are added.

## GitHub API dependency matrix

### Hard dependencies

#### Bun

Remote version listing and non-exact version resolution use:

`https://api.github.com/repos/oven-sh/bun/releases`

The release archive and checksum are also downloaded from GitHub release
assets.

Recommended replacement:

- List versions from the `bun` npm packument.
- Resolve the platform package from `@oven/bun-<os>-<arch>`.
- Verify the package using npm Subresource Integrity.
- Keep GitHub release assets only as a fallback.

The official npm packages were verified during the audit. They expose the same
version, platform-specific binaries, and SHA-512 SRI metadata.

#### Dynamic `github:owner/repo`

The generic backend always requests GitHub release metadata to:

- list versions,
- find a release for an exact version,
- enumerate release assets, and
- auto-select an asset for the host platform.

There is no universal API-free replacement because release asset names are not
available through Git.

Recommended improvements:

- cache release metadata with a stale-cache fallback,
- support pagination,
- allow an explicit asset URL template or asset pattern,
- allow a static release catalog endpoint,
- use `git ls-remote --tags` only when an explicit asset template makes direct
  download construction possible.

### Partial dependencies

#### Deno

`latest` resolution uses `https://dl.deno.land/release-latest.txt`, and exact
versions can be downloaded directly from the canonical Deno CDN. However,
complete remote listing and prefix resolution currently use the GitHub
Releases API.

Recommended replacement:

- Use the `deno` npm packument for version discovery.
- Download and verify `@deno/<platform>` packages with npm SRI.
- Retain `dl.deno.land` as a direct binary fallback.

The official npm platform packages were verified during the audit.

#### Python

The latest python-build-standalone release uses
`latest-release.json` plus `SHA256SUMS`, avoiding the GitHub API. Historical
release discovery still calls the GitHub Releases API. Historical downloads
also use GitHub release assets unless a proxy is configured.

Recommended replacement:

- maintain a versioned static download catalog containing version, platform,
  URL, and checksum,
- embed a known-good catalog in the binary,
- optionally refresh it from a configurable catalog URL,
- keep a stale local catalog for offline and degraded-network operation,
- support an independent artifact mirror.

This mirrors the robust design used by uv's
`crates/uv-python/download-metadata.json`.

### No GitHub API dependency

| Backend | Metadata and artifact source |
| --- | --- |
| Node.js | Node.js distribution indexes and mirrors |
| Go | `go.dev/dl` JSON metadata and mirrors |
| Java | Foojay Disco API and vendor redirects |
| Rust | rustup distribution servers and mirrors |
| pnpm | npm/npmmirror platform packages with SRI |
| Yarn | npm/npmmirror packages with SRI |
| npm | Bundled with the selected Node.js version |

Avoiding the GitHub API does not necessarily avoid GitHub domains. A direct
`releases/download/<tag>/<asset>` URL is not rate-limited like the API, but it
still depends on GitHub network reachability.

## Documentation drift

The following statements do not match the implementation:

- Python and Deno are documented as having no GitHub API dependency, but their
  historical version discovery still uses it.
- pnpm is documented as a GitHub release download, but the implementation uses
  npm platform packages.
- Yarn Berry is documented as using Corepack, but the implementation installs
  `@yarnpkg/cli-dist` directly.
- The CI comment calls Python, pnpm, Yarn, and the dynamic GitHub backend
  GitHub-backed, but omits Bun and overstates pnpm/Yarn.

## Feature gaps

### P0: correctness and reliability

#### Source configuration is not uniformly honored

- pnpm and Yarn use fixed registry constants in `npm.rs`, so `source add`,
  `source pin`, and one-shot source overrides do not control their downloads.
- Deno and Bun hard-code remote-list APIs instead of consistently using the
  selected source's index URL.

#### Declared settings are not implemented

- `--jobs` and `settings.jobs` exist, but installs run sequentially.
- `verify_signatures` exists, but is not read by verification code.
- `--yes` is stored, but there is currently no prompt flow that uses it.

#### Download resume is not implemented

The downloader advertises resume support but removes the partial file before
every attempt. It does not send a `Range` request or validate ETag or
Last-Modified metadata.

#### Rust lifecycle is incomplete

- osdk requires rustup to be preinstalled instead of bootstrapping its isolated
  rustup home.
- uninstall removes only osdk's marker directory; the rustup-managed toolchain
  remains installed.

#### Verification policy is permissive

Most backends continue installation when no checksum can be found. Mature
binary managers expose a policy for required checksums and, where available,
signature, provenance, or artifact-attestation verification.

### P1: core SDK-manager capabilities

| Capability | Mature implementations | osdk |
| --- | --- | --- |
| Extensible backend registry | asdf/mise plugins, proto WASM PDK, vfox Lua, aqua registry | Built-ins plus simplified GitHub backend |
| Reproducible lockfile | mise, proto, aqua package definitions | Missing |
| Offline/prefer-offline mode | mise, uv, SDKMAN | Only Python has stale metadata fallback |
| Outdated and upgrade commands | mise, proto, SDKMAN, uv, rustup | Missing |
| One-shot command execution | mise, proto, fnm | Missing |
| Completion and aliases | mise, proto, asdf, fnm | Missing |
| Project configuration trust | mise | Missing |
| Parallel installation | mise, proto | Declared but sequential |

### Backend-specific gaps

#### Node.js

Compared with fnm:

- no `package.json` `engines.node` resolution,
- no architecture override,
- no version aliases,
- no `exec`,
- no Corepack enable option,
- no global-package migration,
- no real shell integration tests.

#### Python

Compared with uv:

- CPython only,
- no PyPy, GraalPy, or Pyodide,
- no free-threaded or other build variants,
- no prerelease controls,
- no embedded and remotely refreshable full download catalog,
- no dedicated find or upgrade behavior.

#### Java

Compared with SDKMAN and mise:

- JDK only, with no JRE package mode,
- no offline metadata,
- no outdated or upgrade flow,
- no broader JVM candidate registry such as Maven, Gradle, or Kotlin.

#### Rust

Compared with rustup:

- no rustup bootstrap,
- no update/check flow,
- no standalone component or target management,
- no override or linked toolchains,
- incorrect uninstall lifecycle.

#### pnpm, Yarn, and npm

- no `package.json#packageManager` detection,
- npm cannot be selected independently of Node.js,
- configured sources do not consistently control registry requests.

#### Deno and Bun

- no complete API-independent version catalog,
- no canary/nightly channel support,
- no upgrade flow.

#### Generic GitHub backend

Compared with mise and aqua:

- no pagination beyond the first release page,
- no asset regex or URL template,
- no explicit output rename,
- no multiple binaries or additional assets,
- no platform override,
- no static metadata service,
- no SLSA, Sigstore, or GitHub artifact attestations,
- only one trusted minisign key is currently registered.

## Test audit

The project currently has 62 inline Rust tests across 22 source files. Existing
coverage is strongest for:

- version parsing and active-version resolution,
- configuration lookup,
- i18n,
- archive extraction,
- checksum, SRI, and minisign primitives,
- content-addressed storage and garbage collection,
- hardlink and copy behavior.

The primary gap is the absence of CLI integration, backend contract, and
end-to-end tests.

### Missing test layers

- No subprocess tests for `osdk-cli` or `osdk-shim`.
- No backend-specific tests for Node.js, Go, pnpm, or Rust.
- Java, GitHub, Deno, Bun, and Yarn only have minimal pure-function coverage.
- No mock HTTP tests for 403, 429, 5xx, timeouts, stale mirrors, malformed
  metadata, missing checksums, or source failover.
- No common `list -> resolve -> install -> execute -> uninstall` backend
  contract.
- No concurrent install, lock contention, failed-install cleanup, real resume,
  cross-filesystem, or corrupt-manifest tests.
- No shim tests for stdin, stdout, stderr, exit codes, signal forwarding,
  recursion, executable conflicts, or Windows wrappers.
- Windows cross-compilation proves that code compiles but does not execute the
  Windows shim and activation paths.

### Useful competitor patterns

- proto has command integration tests for lockfiles, concurrent installation,
  plugins, and shims, plus per-SDK end-to-end scripts.
- mise tests GitHub asset matching, download resume, failure cleanup, offline
  operation, locking, concurrency, and Windows behavior.
- uv has snapshot integration suites for Python install, find, list, pin,
  uninstall, and upgrade.
- rustup uses a local fake distribution server and CLI golden tests.
- fnm runs alias, directory-switching, execution, and Corepack tests on Linux,
  macOS, and Windows.
- SDKMAN uses WireMock and behavior-driven tests for service failures,
  checksums, upgrades, and project configuration.

## Implementation plan

### Completed in the first remediation pass

1. Replaced Bun and Deno GitHub version discovery with official npm packuments
   and platform packages.
2. Added a generated Python version-to-PBS-release index and made the Astral
   release mirror the primary artifact source.
3. Made npm-registry source configuration effective for Bun, Deno, pnpm, and
   Yarn.
4. Implemented real bounded concurrent installation.
5. Implemented resumable downloads with `Range`, `If-Range`, and validator
   metadata.
6. Completed isolated rustup bootstrap and delegated toolchain uninstall.
7. Connected signature verification settings to the generic GitHub backend.
8. Added a URL-keyed stale metadata cache and strict `--offline` mode.
9. Added integration testing with isolated directories, a local HTTP
   server, generated fixture archives, and subprocess assertions.

### Remaining roadmap

1. Expand the verification policy with additional provenance mechanisms.

## Test isolation requirements

Tests must not use real user installations or caches:

- set every `OSDK_*_DIR` to a per-test temporary directory,
- set isolated `HOME`, `XDG_*`, `RUSTUP_HOME`, and `CARGO_HOME` where relevant,
- use a local mock HTTP server for normal CI,
- mark live upstream smoke tests as ignored or scheduled,
- never invoke the user's global rustup, Node.js, Python, Java, or package
  manager state,
- run command tests with a scrubbed environment and explicit allowlist.
