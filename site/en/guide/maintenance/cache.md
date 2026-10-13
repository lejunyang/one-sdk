# Caches and Cleanup

This page explains osdk's cache layers, the commands that manage them, and how deletions are confirmed. See [Directories and Storage](../config/storage) for locations.

## Cache layers and commands

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| Command | Current behavior |
| --- | --- |
| `cache status [--json]` | Inspect download usage, policy, retries and eligible entries |
| `cache trim [--dry-run] [--json]` | Reclaim registered downloads by policy; exclude CAS/native stores |
| `cache prune` | Delegate unused-cache pruning to uv; no preview |
| `cache dir` | Print cache root, downloads, CAS, and downstream-cache root |
| `cache env` | Print all supported downstream native-cache environment mappings |
| `cache clean` | Remove download copies and uv/pip native caches; preserve CAS and installs |
| `prune --dry-run` | Currently print a dry-run notice without computing or listing candidate objects |
| `prune` | Use SDK-install and model-snapshot manifests as roots and remove unreferenced CAS objects |

`cache clean` preserves CAS, installations, models and remote/source metadata. Within `<cache>/pkg`, only uv/pip are cleaned. GC refuses to continue when it encounters a corrupt
manifest, preventing deletion of objects that may still be referenced.

`osdk container cache status` is separate: it queries storage owned by Docker
Engine or a Buildx builder through supported native aggregate interfaces. See
[Container Runtimes, Registries, and Native Operations](../system/containers).

When the user has not set them, the general shell hook also maps
`npm_config_cache`, `PIP_CACHE_DIR`, `UV_CACHE_DIR`, `GOMODCACHE`, `GOCACHE`,
`CARGO_HOME`, and `GRADLE_USER_HOME`. uv reads neither `pip.conf` nor the `PIP_*`
settings, so it needs its own `UV_CACHE_DIR` (`<cache>/pkg/uv`); `PIP_CACHE_DIR`
does not cover it. There is no Maven `M2_HOME`/`maven.repo.local` redirection.
Direct Rust shims and `exec` override the general `<cache>/pkg/cargo` mapping
with `<data>/cargo`.
Cargo developer-tool providers do not use either location as their build cache:
each install gets a stage-private `HOME`, `CARGO_HOME`, target, and install root.
Only an eligible controlled `cargo-binstall` executable is discovered at
`<data>/cargo/bin`; temporary source/build workspace is removed before publication.
Go developer-tool providers force their module and build caches to
`<cache>/pkg/go-mod` and `<cache>/pkg/go-build`. Their `HOME`, `GOPATH`, `GOBIN`,
and temp directories are stage-private and removed before publication.

## Download-copy retention

By default, osdk retains at most **2 GiB** of its own SDK archives and model download copies. Copies unused by an install/pull for **7 days** expire when maintenance runs. The budget never blocks a larger download or installation: active temporary files may exceed it; an oversized completed copy is removed immediately after verification, content-store publication and backend post-processing succeed. Maintenance runs before/after installs and pulls, or via `cache trim`; there is no background timer.

```toml
[settings.download_cache]
max_size_bytes = 2147483648  # default 2 GiB; 0 = retain none after success; "unlimited" = no size limit
max_age_secs = 604800       # default 7 days; 0 = disable age eviction
```

Size also accepts integer unit strings such as `"2GiB"` and `"2GB"` (binary GiB, decimal GB).

```bash
osdk config set --global download_cache.max_size_bytes 0
osdk config set --global download_cache.max_age_secs 0
osdk --download-cache-max-size 2GiB model sync qwen
osdk cache status --json
osdk cache trim --dry-run --json
osdk cache trim
```

`OSDK_DOWNLOAD_CACHE_MAX_SIZE` and `--download-cache-max-size` override size; CLI wins. `config get` prints canonical bytes or `unlimited`. Total-budget eviction uses the last install/pull access, not filesystem atime. Zero does not disable temporary downloads. Failed/interrupted files and `.partial`/validator state survive for retry, protected for 10 minutes after last use/partial write; afterward size/age policy may reclaim them. `status` separates completed copies, retry files, protected retry bytes, and unregistered legacy files.

Install/pull transactions in this version hold a cross-process shared lease through publication/rollback. Cleanup is deferred while any transaction is active, reports `busy` and actual excess, and is retried after each successful transaction; interrupted/busy cleanup can wait for another install/pull or explicit `cache trim`. Offline mode skips automatic pre-install and failure cleanup; successfully consumed copies still obey the retention budget, including zero. Existing downloads not yet registered by an install/pull are reported as `unmanaged_bytes` and are never silently migrated or removed.

The budget excludes CAS, installed SDKs/models, index metadata and native package-manager stores. On Unix `allocated_bytes` estimates allocated blocks; the reclaimable upper bound excludes multiply linked inodes. Reflink sharing can further reduce actual reclamation. Logical path sizes are not disk-space savings. Known HTTP lengths receive a separate free-space preflight; unknown lengths, concurrent writers, extraction and CAS copies can still exhaust disk space.

When both snapshot manifests and CAS blobs remain intact, an offline model pull verifies and rebuilds the recorded file set even after download copies and remote metadata are removed. Offline aliases use the latest recorded matching snapshot, cannot check server updates, and cannot invent unrecorded files. Missing/corrupt blobs or manifests fail explicitly without network access. Repairs restore absent files; extra or edited data is preserved and reported instead of overwritten. Publication/current-record failures roll back the prior snapshot. Removing a model or GC of its only blobs prevents cold recovery. SDK archive reinstallation still needs retained archives or network; native managers retain their own offline-dependency contracts.

## Deletion and confirmations

```text
osdk uninstall|rm TOOL@VERSION
osdk cache clean
osdk prune [--dry-run]
osdk model remove NAME
```

`uninstall`, `cache clean`, and non-dry-run `prune` require confirmation. An
interactive terminal displays a prompt; non-interactive use must pass `--yes`,
set `OSDK_YES=true`, or configure `settings.yes=true`, otherwise it fails.
`--quiet` hides progress but never grants consent. `model remove` currently asks
for no confirmation.
For an osdk-owned dynamic tool, uninstall derives the complete configured
identity and removes only its fingerprinted root; another identity with the same
backend and version remains installed.
