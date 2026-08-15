# SDK manager first remediation report

Date: 2026-08-15

Project: `github.com/lejunyang/one-sdk`

Related audit: `docs/research/sdk-manager-audit-2026-08-15.md`

## Delivered changes

### GitHub API dependency reduction

- Bun now discovers versions from the official `bun` npm packument and installs
  official `@oven/bun-<platform>` packages with npm SRI verification.
- Deno now discovers versions from the official `deno` npm packument and
  installs official `@deno/<platform>` packages with npm SRI verification.
- Python now uses a generated version-to-python-build-standalone-release index.
  It selects assets from immutable per-release `SHA256SUMS` documents and uses
  `releases.astral.sh` as the primary artifact source.
- The generic `github:owner/repo` backend is now the only backend that requires
  the GitHub Releases API.

### Source correctness

- npm registry helpers now accept the backend's selected source list.
- Bun, Deno, pnpm, and Yarn honor custom and pinned sources for metadata and
  artifact resolution.
- Multiple registry tarball URLs are preserved for download failover.

### Installation reliability

- `--jobs` now performs bounded concurrent resolve/install work.
- Shim generation remains serialized after installation to avoid concurrent
  replacement of the same shim.
- Download cache paths are namespaced by tool and version.
- Interrupted downloads resume with `Range` and `If-Range` when an ETag or
  Last-Modified validator is available.
- Servers that ignore range requests safely restart from zero.
- Transient body/decode failures, 408, 429, and 5xx responses are retryable.
- Persisted checksum sidecars allow offline reinstalls to reverify cached
  artifacts.

### Rust lifecycle

- Rust no longer requires the user's global rustup.
- `rustup-init` is downloaded from the selected update root, SHA-256 verified,
  and installed into osdk's isolated `CARGO_HOME`.
- Toolchains live in osdk's isolated `RUSTUP_HOME`.
- The shim propagates backend execution environment variables, so `rustup`,
  `rustc`, and Cargo stay isolated at runtime.
- Uninstall delegates to isolated rustup and removes the real toolchain instead
  of deleting only the osdk marker.

### Offline mode

- Added global `--offline` and `OSDK_OFFLINE`.
- Metadata requests use a URL-keyed persistent cache.
- Online failures fall back to stale metadata.
- Malformed responses do not replace valid cached metadata.
- Offline mode never probes sources or refreshes source rankings.
- Cached artifacts can be reinstalled and verified offline.
- Missing metadata or artifacts fail with explicit offline cache-miss errors.
- Python remote version listing is fully static and works with an empty cache.

### Verification policy

- `verify_signatures` and `OSDK_VERIFY_SIGNATURES` now control signed manifest
  verification in the generic GitHub backend.
- Signature verification remains enabled by default.
- `--require-checksums` / `OSDK_REQUIRE_CHECKSUMS` rejects archives and bare
  binaries before extraction/materialization when no checksum is available.
- Locked or persisted cache checksums satisfy the required-checksum policy.
- npm-backed SDKs use SHA-512/SHA-256 SRI.
- Python uses upstream SHA-256 manifests.
- Rust bootstrap uses upstream SHA-256 sidecars.

### Tests and documentation

- Added local HTTP tests for source-driven npm metadata and multi-URL failover.
- Added an interrupted-transfer resume test.
- Added metadata cache online/offline tests.
- Added an offline cached-artifact pipeline test.
- Added Rust isolated-uninstall coverage.
- Added CLI subprocess tests with scrubbed environments and temporary
  `HOME`/`OSDK_*` directories.
- Updated README source descriptions, offline behavior, concurrency, resume,
  signature settings, and GitHub API scope.
- Updated CI comments and the original audit roadmap.

### Core project workflow

- Added a platform-aware `osdk.lock` format that preserves other platform
  entries while updating the current one.
- Installed tools contribute their exact artifact URL, filename, and verified
  checksum to the lock; locked installs consume those values directly and
  reject checksum tampering.
- No-argument `osdk install` consumes the matching platform lock.
- Added `osdk outdated` and `osdk upgrade`; upgrade refreshes the lock after
  installation.
- Added `osdk exec --tool ... -- <command>` for one-shot managed environments.
- Added shell completion generation for Bash, Zsh, Fish, Elvish, and
  PowerShell-compatible clap targets.
- Added user-defined, chainable version aliases with cycle detection and
  canonical backend names.
- Added reversible shell activation state and `osdk deactivate` for Bash, Zsh,
  Fish, and PowerShell.

## Isolated validation

All local tooling and runtime state were isolated:

- Rust toolchain: `/tmp/one-sdk-toolchain`
- Bun/Deno smoke: `/tmp/one-sdk-smoke`
- Rust smoke: `/tmp/one-sdk-rust-smoke`
- Python smoke: `/tmp/one-sdk-python-smoke`
- Offline smoke: `/tmp/one-sdk-offline-smoke`
- Final test state: `/tmp/one-sdk-final-test`

No user-global SDK manager state or cache was used for test installations.

### Automated validation

Passed:

- `cargo fmt --all --check`
- `cargo test --workspace`
  - 70 `osdk-core` tests
  - 2 lockfile unit tests
  - 8 CLI integration tests
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --workspace --target x86_64-pc-windows-gnu`
- `git diff --check`

### Live isolated smoke tests

Passed:

- Bun `1.3.14`: install, shim execution, uninstall.
- Deno `2.9.5`: install, shim execution, uninstall.
- Rust stable: automatic rustup bootstrap, toolchain install, `rustc` execution,
  isolated `rustup show home`, real toolchain uninstall.
- Python `3.12.14`: static version resolution, Astral artifact download,
  SHA-256 verification, `python3`/`pip3` execution, uninstall.
- Offline Bun: online cache warmup, uninstall, `--offline` reinstall, shim
  execution.
- Empty offline cache: explicit metadata cache-miss failure.
- Empty-cache offline Python list: static `3.14.x` versions returned without
  network access.
- P1 project workflow: offline static lock generation, lock consumption,
  outdated reporting, upgrade lock refresh, one-shot managed execution, and
  Bash completion generation.

## Remaining roadmap

The first remediation pass intentionally leaves larger product work for
separate changes:

1. Add broader provenance/attestation verification.
