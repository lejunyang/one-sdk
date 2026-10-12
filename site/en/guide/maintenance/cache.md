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
| `cache dir` | Print cache root, downloads, CAS, and downstream-cache root |
| `cache env` | Print all supported downstream native-cache environment mappings |
| `cache clean` | Remove and recreate only `<cache>/downloads` |
| `prune --dry-run` | Currently print a dry-run notice without computing or listing candidate objects |
| `prune` | Use SDK-install and model-snapshot manifests as roots and remove unreferenced CAS objects |

`cache clean` does not remove the CAS, installations, models, remote/source
metadata, or `<cache>/pkg`. GC refuses to continue when it encounters a corrupt
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
