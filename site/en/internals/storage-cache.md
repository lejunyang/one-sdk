# Storage, CAS, Cache, and Cleanup Implementation

osdk separates persistent installation state, content-addressed objects, and disposable caches. Directory resolution is in [`dirs.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/dirs.rs), CAS behavior in [`store/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs), and link selection in [`store/link.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs).

## Directory boundaries

The default layout is below. The data and cache roots are overridable, while store and installs also have dedicated overrides; the remaining paths are derived subdirectories:

- `<installs>/<tool>/<version>`: materialized fixed-backend SDKs; `<installs>` defaults to `<data>/installs` and can be overridden by `OSDK_INSTALL_DIR`.
- `<installs>/<dynamic-tool>/<version>/b3-v2-<digest>`: fingerprinted roots for osdk-owned isolated dynamic tools.
- `<installs>/npm-global/<package>/<version>/b3-v2-<digest>`: fingerprinted roots for `use --global npm:<package>`. Older version-only roots containing `.osdk-tool.json` are detected as legacy state but never reused or executed.
- `<data>/models/<name>/snapshots/<snapshot>`: materialized model snapshots.
- `<data>/store/<aa>/<bb>/<blake3>`: CAS shared by SDK and model files.
- `<data>/shims`: command shims.
- `<cache>/downloads`: downloaded tool archives and checksum/source/attestation sidecars.
- `<cache>/tmp`: installation extraction scratch space.
- `<cache>/remote` and `<cache>/sources`: remote metadata and source-probe caches.
- `<cache>/pkg`: native downstream package-manager and model-client caches.
- `<cache>/npm/v1/cache` and `<data>/store/npm`: the cache/store shared by npm-backed isolated, project, and global npm tools; each real project or controlled install root still keeps its own native lock.

`Dirs::ensure` creates the core tree during CLI initialization. Store and installs default to the same data volume so hardlinks work. `OSDK_STORE_DIR` may put the store on another volume, which can force materialization to fall back to reflink or copy.
Each dynamic root contains `.osdk-install.json` schema 1. Its nested `identity`
contains `tool`, `version`, `platform`, `scope`, `material_options`,
`dependencies`, `materials`, and canonical `b3-v2:` `install_id`. This identity
drives reuse, activation, shim dispatch, `where`, uninstall, and `reshim`; sibling
identities may coexist. Project-managed npm state under `.osdk/npm-bin` remains a
separate project-owned layout.
For Cargo developer tools, the fingerprint also binds the exact managed Rust
tree and registry/Git materials. Installation uses a sibling stage with private
`home`, `cargo-home`, `target`, and `tmp` directories; those workspaces are
deleted before only the validated `bin` output and native metadata are published.
For Go developer tools, the fingerprint binds the exact managed Go runtime,
selected proxy/module root, tags, and allowlisted build environment. Private
home/GOPATH/temp directories stay in the sibling stage, while module and build
caches are shared under `<cache>/pkg/go-mod` and `<cache>/pkg/go-build`.

## SDK pipeline and locking

Archive backends enter [`pipeline::run_with_attestation`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs#L129) in this order:

1. Fixed backends acquire a blocking exclusive process lock at `installs/<tool>/.locks/<version>.lock`. Dynamic backends acquire an identity-qualified lock and keep it through backend-specific finalization.
2. Under the lock, check `.osdk-complete`. A complete install is reused, although requested attestation is still reverified and receipt evidence merged.
3. Remove a stale partial directory for that version.
4. Reuse or download the `<cache>/downloads/...` archive and verify checksum/attestation.
5. Extract under `<cache>/tmp/...-<pid>`.
6. Ingest regular files into CAS, materialize the install tree, and write `.osdk-manifest.json`.
7. Write the artifact receipt, then write `.osdk-complete` last.

The lock covers download, verification, extraction, CAS writes, materialization, and the complete marker. Fixed backends serialize the same **tool/version**; dynamic backends serialize only the same full install identity, so sibling identities can proceed independently. [`FileLock`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lock.rs) is held until the function returns and released on drop. Delegate backends that invoke an upstream manager do not necessarily enter this shared archive pipeline, so this lock is not an unconditional guarantee for every backend.

## CAS writes and materialization

Each regular file is addressed by its BLAKE3 content hash under a two-level fan-out path. SDK `ingest_file` first tries to rename the extracted source to the final object. Across filesystems it copies to `.tmp` and then renames. Concurrent ingests rely on final-object existence and rename races rather than per-object locks. One implementation detail is that the SDK path shares the `<hash>.tmp` name: a failed final rename is ignored, and if no winner actually produced the object, the code proceeds until materialization reports a missing object or I/O failure rather than returning a dedicated ingest-race error.

Models use [`ModelStore::publish`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs#L174). It acquires `models/<name>/.locks/<snapshot>.lock`, verifies size and optional SHA-256, uses `ingest_preserve` so downloaded sources remain intact, materializes into a PID-scoped temporary snapshot, writes both CAS and model manifests plus a completion marker, renames into the final snapshot, and updates `current.json` through a temporary file plus rename. This lock serializes one model snapshot, not a model-wide or global store operation. Different snapshots of one model can publish concurrently and race with last-writer-wins behavior on `current.json`; model removal takes no corresponding model-wide lock and can also race with publication. These renames have no fsync/durability or portable replace-atomicity guarantee.

[`materialize`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/link.rs#L132) uses hardlink, then reflink, then copy in `auto` mode on one filesystem. Across filesystems it tries reflink and then copy. Explicit hardlink and reflink modes also fall back to copy. Symlink is opt-in only. The manifest records each regular file's CAS hash and mode, plus symlink targets, and supplies GC reachability.

## No cross-manager package CAS

The CAS deduplicates verified, extracted SDK files and model files. It does **not** parse, ingest, or deduplicate project dependency packages across npm, pnpm, Yarn, Bun, Deno, pip, Go, Cargo, Maven, or Gradle. npm-backed `npm:<package>` operations share `<cache>/npm/v1/cache` and `<data>/store/npm`, while pnpm uses its downstream cache/store paths; none of those package contents enter the BLAKE3 SDK CAS.
Cargo developer-tool source and build data is likewise stage-private and is not
promoted to a shared Cargo cache or the CAS; the final fingerprinted root retains
only published binaries and osdk metadata.
Go module and build data use the Go-owned caches above and likewise never enter
the BLAKE3 SDK CAS; a published Go-tool root retains only validated binaries and
osdk metadata.

[`cache_env`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/cache/mod.rs#L23) and backend `exec_env` methods only redirect each manager's native cache to a separate child of `<cache>/pkg`, including npm, the pnpm store, Yarn, Bun, and Deno. Variables are injected only when the user has not set them, except that a value managed by a previous osdk hook can be refreshed. Sharing a parent directory does not unify content protocols: there is no cross-manager package CAS or cross-manager blob deduplication.

## `cache clean` removes downloads only

After confirmation, `clean` in [`commands::cache`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L1126) removes only the complete `<cache>/downloads` tree and then best-effort recreates that directory. It does not clean:

- the `<data>/store` CAS;
- `<data>/installs` or `<data>/models`;
- downstream manager/client caches under `<cache>/pkg`;
- metadata in `<cache>/remote`, probe results in `<cache>/sources`, or `<cache>/tmp`.

“Cache clean” therefore does not mean “clear all caches.” Non-interactive execution requires confirmation through `--yes`, `OSDK_YES=true`, or `settings.yes = true`; refusal leaves data untouched.

## GC, removal, and the concurrency caveat

[`Cas::gc_roots`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/store/mod.rs#L207) is a mark-and-sweep pass. It recursively scans every `.osdk-manifest.json` under installs and models, collects hashes into a live set, then walks the store and removes regular files outside that set. A corrupt manifest fails the entire GC closed before deletion. Directory traversal errors are skipped. Files whose names end exactly in `.tmp` are opportunistically deleted before liveness checking; other temporary forms such as `.tmp-<pid>` receive no special protection and are handled as ordinary unreferenced files.

`osdk prune`, post-SDK-uninstall cleanup, and post-model-remove cleanup call this function directly. Uninstall removes the install tree before GC, and model removal does the same for its model tree. The current implementation has **no claim/lease mechanism and acquires no global GC lock**. Although the module comment in [`lock.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lock.rs) mentions guarding store GC, no GC call site actually uses `FileLock`. It is therefore incorrect to claim that claim GC is globally serialized, or that GC is mutually exclusive with installation/publication of a different version or model.

This leaves a real race window: another process can create a reference between GC's live-set scan and deletion, and an installer can create CAS objects before writing its manifest, allowing concurrent GC to regard those objects as unreachable. Per-version and per-snapshot locks do not close that global window. Production guidance should treat concurrent `prune`/removal and installation as uncoordinated operations.

Finally, GC removes object files only, leaving empty fan-out directories. Individual deletion failures are silently skipped and do not increase the removed-object count, but their sizes may already be included in the reported byte total.
