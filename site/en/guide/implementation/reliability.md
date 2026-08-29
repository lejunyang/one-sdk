# Reliability, concurrency, and commit semantics

osdk combines bounded concurrency, source probing and failover, resumable downloads, cross-process locks, content-addressed storage, and completion markers. These mechanisms do not form one database-style transaction around an entire command. The atomicity and failure boundaries differ by layer.

## Concurrency model

`settings.jobs` bounds tool installations and model-file downloads within one command. Its default is the detected logical parallelism capped at 8, or 4 when detection fails. A configured or environment value of zero is ignored, and execution still applies `max(1)`. See [`config/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs), [`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs), and [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs).

Multi-tool installation first applies a Node-first barrier. If requests contain
npm, pnpm, Yarn, or a dynamic `npm:<package>` tool without Node, the CLI injects
Node. Every Node request completes serially and receives its shims before the
remaining requests enter `buffer_unordered(jobs)`, preventing an npm tool
from racing its managed runtime. Completion order after the barrier is
unspecified. Shims are generated in that order; only returned resolution records
are sorted by backend name afterward. A failed task makes the batch return an
error, but completed independent installs are not rolled back. This is a
dependency barrier plus bounded concurrency and per-item commits, not an
all-or-nothing batch transaction.
Cargo tools add an independent Rust-first barrier. The CLI requires exactly one
exact managed Rust request, completes it before scheduling the dependent
`cargo:` requests, and binds the resolved Rust version into those identities.

Source speed tests probe all candidates concurrently and are not bounded by `jobs`. Each probe defaults to a 1500 ms deadline, reads at most about 1 MB, and scores time to first byte plus throughput. Successful rankings are cached for 6 hours by default. `auto` uses probe ranking, `ordered` uses configured priority, and a pin moves one source first while retaining the others as fallback. See [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs).

## Downloads, retries, and caches

[`download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs) streams into a sibling `.partial` and renames it to the final path only after a successful flush. `.partial.json` records the URL, ETag, and Last-Modified value. Resume uses `Range` plus `If-Range` only when the partial belongs to the current URL and has a validator. Otherwise the old partial is removed and the transfer restarts. A server that ignores the range causes a truncating restart, an incorrect `206` range fails, and `416` removes partial state before one full request.

Each URL gets at most three attempts. Retries are limited to rate limiting, 5xx, request timeout, interruption/connectivity, and reqwest request/body/decode errors. Backoff is 400 ms after the first failure and 800 ms after the second. Other 4xx responses, digest failures, extraction failures, and policy failures are not retried. After a URL exhausts its attempts, the install pipeline advances through candidate sources in order.

The shared HTTP client has a 15-second connect timeout, 30-second idle-pool timeout, and a 10-redirect limit; it does not set one total request deadline. Online metadata failures may fall back to an existing stale cache. Offline mode reads caches only and fails on a miss. Direct and proxied GitHub transports share a cache identity based on the canonical upstream URL. See [`http/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/http/mod.rs).

Boundary: rename prevents a successful download from exposing a partially written final file, but durability is not guaranteed because files and directories are not `sync_all`ed. Partial metadata is not atomically written. A cache hit skips transfer, while the active checksum or attestation policy still determines whether its bytes are accepted.

## SDK install commits

Archive installation in [`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) takes an exclusive cross-process lock per `tool@version`. While holding it:

1. an install with `.osdk-complete` is considered complete; an attestation requested by this run is still rechecked against the cached artifact and merged into its evidence;
2. an old install directory without the marker is treated as residue and removed;
3. download, checksum/attestation, extraction, CAS ingestion, materialization, and receipt writing run in sequence;
4. `.osdk-complete` is written last, and readers count only marked directories as installed.

A failure does not write the marker, so the next run can clean and rebuild. The SDK tree is nevertheless materialized directly into its final directory, not assembled completely and renamed as a unit. An interruption can therefore leave partial files until the next cleanup. The marker is the commit criterion, not an atomic directory replacement. Backend post-processing runs after the core pipeline returns, so a later post-processing failure can occur after the core marker exists.

Bare-binary installation also writes its receipt and marker last and leaves incomplete state for a later cleanup on failure. However, the current [`install_single_binary`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) does not acquire the per-`tool@version` lock used by the archive path. Normal callers avoid duplicate work through bounded scheduling, but same-version bare-binary installs in separate processes do not have the archive path's serialization guarantee.

Cargo developer tools use a separate native commit protocol. An exact
identity-qualified lock covers candidate validation, a unique sibling stage,
provider execution, and publication. The publisher rejects symlinks and reserved
metadata, inventories and hashes binaries, writes the native receipt, dynamic
inventory, completion marker, and adjacent metadata seal, then exposes the tree
with a no-replace directory rename. A failed or dropped unpublished stage is
removed. Reuse revalidates the seal, inventory, receipt, binary SHA-256 values,
and exact managed Rust version/platform plus bounded build-critical runtime identity; see [Cargo developer tool implementation](./cargo-tools).

## CAS and materialization

[`store/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs) names objects by their BLAKE3 content hash. A new object is renamed into place when possible; the cross-filesystem path copies to a temporary object and then renames, keeping ordinary readers from seeing a partial CAS object. If another publisher has already won the race, the winner is retained and temporary state is removed. Materialization defaults to `auto`: hardlink, reflink, then copy on the same filesystem; reflink then copy across filesystems. Symlinking into the store is explicit opt-in only. See [`store/link.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs).

Boundary: concurrent CAS publication relies on atomic rename and checking whether the destination exists; there is no per-object lock, and temporary-name uniqueness is primarily the process ID. An extreme same-process race ingesting the same new hash should not be described as a strict transaction. GC scans install and model manifests and refuses to continue when a manifest is corrupt, avoiding silent deletion of referenced objects. Current GC call sites take no global lock, so GC is not strictly isolated from concurrent installation or publication.

## Model snapshots

Model files are downloaded with the same `jobs` bound. Provider-supplied size and SHA-256 are checked; when no SHA-256 is supplied, osdk computes one for its local manifest. Paths must be non-empty relative paths containing only normal components, rejecting absolute paths and `.`/`..`. See [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs).

[`ModelStore::publish`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) derives a snapshot key from content identity and takes a cross-process lock for that snapshot. It completes CAS materialization, manifests, and the complete marker in a hidden temporary directory, then renames that directory to the final snapshot. Failure removes the temporary directory. `current.json` also uses a temporary file plus rename. Different snapshots have different locks, so two revisions of one model may publish concurrently; both can succeed and the last `current.json` update becomes current.

## Lockfile and trust-store writes

`osdk.lock` stores independent platform resolutions and artifact identities. It is written as a same-directory `tmp-<pid>` followed by rename; the trust store and JSON current pointer use similar publication. This provides a temporary-write boundary, but there is no fsync/durability, portable replace-atomicity, or cross-process read-modify-write guarantee. Concurrent writers may produce last-writer-wins behavior, collide on temporary names, or encounter an existing-destination error. Rename is not multi-process transaction isolation. See [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) and [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs).

A project config's trust identity combines its canonical path with a BLAKE3 of normalized TOML. A file containing only `tools` and `aliases` does not require explicit trust; top-level `settings`, `sources`, `registries`, or other keys do. Trust commands load only user-level configuration so project configuration cannot influence the decision to trust itself. Moving the file or changing effective TOML invalidates the record. `OSDK_TRUSTED_CONFIG_PATHS` trusts by canonical path prefix and is broader, so it should be scoped carefully.

## Key tests

- [`pipeline/download.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/download.rs): Range/If-Range resume and proof that an interrupted transfer never publishes the final artifact.
- [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs): two concurrent archive installs commit one complete result, failures do not write the marker, and offline cache/checksum gates work.
- [`model/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) and [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs): immutable snapshots, tamper detection, selection identity, and offline reconstruction.
- [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs): cross-platform merging, restoration of saved version strings and options, the floating-Rust-channel boundary, and persistence of model digests and verification evidence.
- [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs): canonical paths, content changes, symlinks, and invalidation after repository moves.
