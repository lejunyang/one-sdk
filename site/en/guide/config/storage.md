# Directories and Storage

All osdk state lives under three overridable roots: data, cache, and configuration. This page lists the default locations and derived directories, and how installed files are materialized from the content-addressed store (CAS). See [Caches and Cleanup](../maintenance/cache) for inspecting and reclaiming cache space.

## Directory layout and overrides

| Purpose | Linux default | Environment variable |
| --- | --- | --- |
| Data root | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| Content store | `<data>/store` | `OSDK_STORE_DIR` |
| SDK installations | `<data>/installs` | `OSDK_INSTALL_DIR` |
| Cache root | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| Configuration directory | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

Derived directories are:

```text
<data>/models                model snapshots
<data>/shims                 command shims
<data>/rustup                isolated RUSTUP_HOME
<data>/cargo                 isolated Rust CARGO_HOME; controlled cargo-binstall location
<data>/plugins               declarative backends
<cache>/downloads            SDK/model downloads
<cache>/tmp                  extraction scratch space
<cache>/remote               metadata and proof cache
<cache>/sources              source probe rankings
<cache>/pkg                  downstream tools' native caches
```

The configuration directory also holds two derived states:
`<config>/trusted-configs.toml` records project-configuration trust (see
[Project Configuration Trust](./trust)), and
`<config>/projects/<root-id>/` holds the `project.toml`, `osdk.toml`, and
`osdk.lock` of each external project binding (see
[External Project Configuration](../project/external-config)).

`OSDK_BIN_DIR` belongs to the osdk executable installer; it is not an SDK state
directory and must not be confused with `OSDK_INSTALL_DIR`.

The installer asks for `OSDK_CONFIG_DIR`, `OSDK_DATA_DIR` and `OSDK_CACHE_DIR`
after installing, and writes the chosen values into the startup files of the
shells you select, so these three rarely need to be set by hand. See
[Shell setup](../start/installation#shell-setup).

## CAS and materialization

After verification and extraction, SDK and model files enter the BLAKE3 CAS at
paths such as `store/<first-2>/<next-2>/<full-hash>`. Installation/snapshot
manifests record relative paths, hashes, Unix modes, and symlink targets before
materializing objects into a version directory.

`settings.link_mode` and `OSDK_LINK_MODE` accept:

| Mode | Behavior |
| --- | --- |
| `auto` | On one filesystem, try hardlink, reflink, then copy; across filesystems, try reflink then copy |
| `hardlink` / `hard` | Prefer hardlink and fall back to copy |
| `reflink` / `clone` / `cow` | Prefer copy-on-write cloning and fall back to copy |
| `copy` | Regular byte copy |
| `symlink` / `sym` | Explicit symbolic links; failure does not fall back, and `auto` never selects this |

The SDK/model CAS does not contain package managers' native caches; those formats
are incompatible. See [Native cache semantics](../tools/package-managers#native-cache-semantics).
