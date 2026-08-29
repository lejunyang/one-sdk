# Installation pipeline

`osdk` separates version resolution from artifact installation. The CLI owns request orchestration, concurrency, and shims; each backend builds an installation plan; the shared pipeline downloads, verifies, extracts, ingests into CAS, and commits the installation.

## CLI orchestration

[`install_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) merges `-o key=value` options and installs different tools with bounded concurrency from `[settings].jobs`. Each request enters [`install_one_without_shims`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs):
If the request set contains npm, pnpm, Yarn, or `npm:<package>` without Node,
the CLI first injects Node. Scheduling completes every Node request serially
before admitting the remaining requests to bounded concurrency, preventing a
dynamic npm tool from racing its runtime.
The same orchestration requires exactly one exact managed Rust request for any
`cargo:` tool. Rust resolves and installs before Cargo tools enter the concurrent
remainder, and its exact result is bound into each Cargo install identity.

1. optionally refresh source probes for SDK-installing calls; the `lock`, `outdated`, and `list-remote` resolution-only paths do not execute this flag;
2. find the backend, expand the version alias, and resolve an exact version;
3. if the complete marker exists, run only idempotent `ensure_post_install`;
4. otherwise call the backend's `install`;
5. after all installs finish, generate shims and sort results by backend name.

Except for that Node dependency barrier, different tools may run concurrently.
Pipeline or backend locks serialize fixed-backend writes to one `tool@version`
and dynamic writes to one complete install identity. If any member of a batch fails, `try_collect` returns the error
and the final shim-generation phase is not entered. The compatibility isolated
npm path used by explicit `install`/`exec` bypasses the archive CAS pipeline
below and uses embedded Aube with an isolated install root and the shared
osdk-owned Aube cache/store. Project-aware and global `use` can instead select
Aube, npm, or pnpm during planning; see
[npm developer tool implementation](./npm-tools).
Cargo developer tools also bypass the archive CAS pipeline. Their native
lifecycle holds an identity lock across a sibling stage, prefers a controlled
eligible `cargo-binstall`, falls back to `cargo install` only for exit 94, and
atomically publishes validated binaries; see
[Cargo developer tool implementation](./cargo-tools).

## Backend plans

The common contract is [`Backend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs). An archive backend normally calls `ranked_source_list`, then constructs an [`InstallPlan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) containing the tool, exact version, candidate URLs, filename, archive kind, optional checksum, strip-root flag, and optional safe subdirectory. [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs) is a representative implementation.

A request restored from the lockfile first uses [`locked_install_plan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs), carrying the locked URL, filename, checksum, and subdirectory without querying a release registry again. Built-in archive backends and data-only declarative backends share this path. pnpm, Yarn, Deno, Bun, and standalone npm are driven by npm registry packages and SRI. The generic GitHub backend may additionally require Sigstore/Rekor attestation. Rust is the major exception: it delegates toolchain installation to isolated rustup/Cargo homes and lets osdk maintain the completion marker and shims.

## Shared pipeline transaction

[`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) performs these steps:

1. Acquire `<tool>/<version>.lock` for a fixed backend. A dynamic backend instead holds an identity-qualified lock through post-processing, `.osdk-install.json` publication, and the completion marker.
2. If the pipeline is invoked directly and `.osdk-complete` exists, it returns early; when that invocation carries attestation, it reverifies and merges evidence. Normal CLI install usually short-circuits earlier in `install_one_without_shims` and runs only `ensure_post_install`.
3. Remove a stale install directory that has no completion marker.
4. Use a stable artifact-cache path; fail immediately on an offline cache miss.
5. Download URLs in plan order. A single URL gets up to three transient-error attempts before the pipeline advances to the next source.
6. Verify the supplied or previously persisted checksum, and verify attestation plus its authenticated digest when present. Fail when `require_checksums=true` and neither is available.
7. Extract into a scratch directory under the cache. An optional subdirectory must be relative, contain no `..` or platform prefix, and remain beneath scratch.
8. Ingest files into the BLAKE3 CAS, then materialize the install tree using reflink, hardlink, or copy according to configuration.
9. Write `.osdk-artifact.json`, then write `.osdk-complete` last. A version is listed as installed only after that final commit marker exists.

## Resume and caching

[`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs) writes to a sibling `.partial` file and atomically renames it on success. It sends `Range` plus `If-Range` only when partial metadata has an ETag or Last-Modified validator for the same URL. A server that ignores ranges, changes the object, or returns a mismatched `Content-Range` causes a safe restart or failure rather than blind concatenation. Sensitive headers apply only to the initial request and are not retained on cross-host redirects.
This paragraph concerns download headers carried by a generic artifact download
plan. `Source.headers` separately applies to osdk metadata/source probes under an
origin boundary. Aube-backed npm package fetches currently do not forward
arbitrary `Source.headers`. Project operations may use native trusted
configuration; global npm-tool installs reject authenticated/private native
pass-through while running in their isolated prefix.

An artifact-cache hit during an actual pipeline run or reinstall still runs the applicable checksum or attestation verification; mere cache-file existence is not trusted. Offline reinstall can reuse a persisted checksum, but it never falls back to the network when the artifact cache is absent. Ordinary `osdk install` reuses an already complete installation before entering the pipeline and does not revalidate its receipt, checksum, or installed bytes.

## Failure semantics and caveats

- Download failover covers artifact retrieval only. After one candidate downloads successfully, checksum, attestation, extraction, or materialization failure does not rerun the whole installation against another source.
- The pipeline removes stale install trees and scratch at the start of the next attempt and removes scratch after successful materialization; failed extraction or materialization may leave scratch temporarily. It keeps `.partial` downloads for validated resume.
- Checksums are policy-dependent unless the backend supplies one, attestation supplies an authenticated digest, or `require_checksums` is enabled. Guarantees therefore differ by backend.
- `ensure_post_install` may have additional side effects. On a fresh Node install, Corepack failure removes the installation tree; on the already-installed fast path, `ensure_post_install` may fail while the existing completion marker remains.
- Delegate backends such as Rust do not traverse the complete archive pipeline; Cargo developer tools also use their own native stage/receipt/seal transaction. Inspect those backends for their exact idempotency and verification boundaries.
- For backends with a generic artifact receipt, a locked artifact URL fixes artifact identity and enables metadata-free reinstall. Reinstallation applies any available or policy-required checksum/attestation checks; with no digest/evidence and `require_checksums=false`, it may proceed without cryptographic integrity verification. npm tools do not use generic artifact receipts. Current schema 4 retains scope, installer, and optional native-lock identity while leaving the dependency-graph payload under installer ownership. Schema 2 graph sidecars remain a compatibility-read path only; see [npm developer tool implementation](./npm-tools). Cargo native metadata likewise describes runtime/source/replay identity rather than a complete graph, so cold offline installation is unsupported; see [Cargo developer tool implementation](./cargo-tools).

Core coverage is in [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs), [`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs), [`backend/contract.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/contract.rs), and end-to-end [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs).
