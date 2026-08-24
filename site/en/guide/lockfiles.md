# Reproducible Lockfiles

Except for floating Rust channels, `osdk.lock` records resolved exact versions
and selected artifacts so the same platform can rebuild an environment without
querying upstream version indexes again. Rust locks for `stable`, `beta`, and
`nightly` preserve only the rustup channel name and may install a newer
toolchain later; use an explicit or dated Rust toolchain for immutable
reproduction. The lock is a reproducibility input and audit record, not
permission to skip verification.

## How commands interact with the lock

```text
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
osdk model pull NAME REFERENCE [OPTIONS]
```

| Invocation | Reads the existing lock? | Writes the lock? |
| --- | --- | --- |
| `install` with no tools and no `-o` | Yes; if a current-host platform section exists, use all tools in it | No |
| `install TOOL...` | No | No |
| `install -o KEY=VALUE`, even without a tool | No | No |
| `lock` | Loads the old file only to preserve other platforms and models | Yes; rebuilds the target platform's complete tool map |
| `outdated` | No; re-resolves configuration or explicit requests | No |
| `upgrade` | No; re-resolves configuration or explicit requests | Yes; rebuilds the host platform's tool map |
| `exec` | No | No |
| `model pull` | Does not use a model lock as input | Merges `[models]` by default; `--no-lock` disables this |
| `list`, `current`, `where` | No | No |

For `outdated`, the “current” column is the greatest installed version for that
backend. It checks whether the newly resolved exact target is installed; it does
not mean the directory's active version. `upgrade` installs the new resolution
and then refreshes the lock.

## Recommended workflow

```bash
# Resolve project declarations without installing
osdk lock

# Use the resolutions and artifacts from the current-platform lock
osdk install

# Re-resolve current declarations and report targets not yet installed
osdk outdated

# Install re-resolved targets and refresh the lock
osdk upgrade
```

Explicit `osdk install node@20` always follows the explicit request and ignores
the lock. Adding any backend option to a no-argument install also bypasses the
lock; prefer regenerating it with the same options first.

## Discovery and write locations

- For reads, osdk walks upward from the current directory and uses the nearest `osdk.lock`.
- For writes, a discovered project configuration determines the sibling lock path.
- Without project configuration, osdk reuses the nearest ancestor lock; if none exists, it creates one in the current directory.

In unusual nested layouts, the nearest readable lock and the project-determined
write path can differ. Keep `osdk.toml` and `osdk.lock` together at the project
root.

## Schema 2

```toml
schema = 2

[platforms.linux-x64.tools.node]
request = "20"
version = "20.20.0"

[platforms.linux-x64.tools.node.options]
arch = "x64"
corepack = "false"

[platforms.linux-x64.tools.node.artifact]
url = "https://example/node.tar.gz"
file_name = "node.tar.gz"
checksum = "sha256:..."       # optional
subdir = "dist"               # optional

[[platforms.linux-x64.tools.node.artifact.evidence]]
# Verified supply-chain evidence; fields depend on the evidence type

[platforms.linux-x64.tools."npm:prettier"]
request = "3"
version = "3.6.2"

[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "20.20.0"
lock_format = "aube-v9"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
graph = "osdk.lock.d/npm/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.yaml"

[models.qwen]
provider = "huggingface"
repository = "Qwen/Qwen2.5-7B-Instruct"
requested_revision = "main"
revision = "immutable-revision"
endpoint = "https://huggingface.co"
variant = "safetensors-fp16" # optional

[[models.qwen.files]]
path = "config.json"
size = 123
sha256 = "..."
```

Platform keys use `linux-*`, `macos-*`, or `windows-*` plus
`x64|arm64|x86|arm`; musl Linux adds `-musl`. Updating one platform preserves
other platform sections and top-level model entries. Internal `__osdk_*`
options are omitted from public `options`; non-npm backends that support generic
receipts store artifact identity separately.
Schema 2 requires an `npm` table for every `npm:<package>`. The main lock stores
only `package`, `node_version`, `lock_format`, `sha256`, and the canonical
`graph` path. The complete Aube graph lives at
`osdk.lock.d/npm/<sha256>.yaml` and carries transitive dependency integrity. An
npm tool entry cannot carry a generic `artifact` table. Commit the main lock and
`osdk.lock.d/` together.

A schema 1 lock without npm tools remains readable and safely upgrades on its
next successful write. A schema 1 lock containing any `npm:*` entry, including
the old inline graph form, cannot be consumed or migrated and must be regenerated;
this prevents old records without a verifiable sidecar from being mislabeled as
schema 2.

For Node, `lock -o arch=...` writes the target-architecture section. osdk has no
cross-architecture download-only mode, and installation rejects an artifact that
cannot run on the host. `upgrade -o arch=...` currently still writes the host
platform section, so do not use it to generate a cross-architecture lock.

## Stale project locks

osdk currently does not compare `osdk.toml` and `osdk.lock` timestamps or
content. If the nearest lock contains a current-platform section, no-argument
`install` uses that section completely even after project configuration changes.
An existing but empty tool map also does not fall back. Only a missing current-
platform section falls back to project discovery.

After changing project requests, run one of:

```bash
osdk lock       # refresh exact resolution only
# or
osdk upgrade    # install re-resolved versions and refresh the lock
```

Malformed TOML and unsupported schemas fail explicitly instead of silently
falling back to configuration. The main lock and each npm graph sidecar are
currently limited to 16 MiB. Writes atomically publish sidecars first, read and
validate every graph, and only then atomically replace the main lock.

## npm tool graphs and the offline boundary

For `npm:<package>`, `osdk lock` first ensures managed Node is installed, then
uses embedded Aube in lockfile-only mode to resolve the complete dependency
graph. This phase never runs lifecycle scripts. The original UTF-8 graph bytes
are addressed by SHA-256 at `osdk.lock.d/npm/<sha256>.yaml`; the main lock stores
only the package, exact Node version used for the graph, `aube-v9`, digest, and
the sidecar path uniquely determined by that digest.

When argument-free `osdk install` restores an npm tool from the lock, it first
checks that all fields exist, package and backend identities match, the Node
version matches the same-platform Node entry, and the format and path are
canonical. It then rejects symlinked paths, reads the sidecar with a 16 MiB
limit, validates UTF-8 and SHA-256 over its actual bytes, and installs from that
frozen graph. Offline reinstall also requires the same warmed Aube cache/store;
the sidecar fixes the graph but contains no package tarballs. The recommended
flow is:

```bash
# Online: generate the graph and warm all referenced package content
osdk lock
osdk install

# After removing it, rebuild offline with the same lock, sidecar, and cache
osdk --offline install
```

Explicit `osdk --offline install npm:prettier@3.6.2` does not read the project
lock and therefore cannot use its graph. A missing, corrupt, oversized, or
symlinked sidecar, or missing cached package content, fails explicitly. See
[npm Developer Tools](./npm-tools) for details.

## Verification boundaries for locked reinstalls

A lock can preserve the actual URL, filename, checksum, archive subdirectory,
and attestation evidence for non-npm backends that support generic artifact
receipts. No-argument installation restores their saved resolution, backend
options, and artifact identity. npm tools use the graph sidecar described above,
not a generic artifact receipt. For a floating Rust channel,
that resolution remains a channel name rather than an immutable release.

When an installation is missing or incomplete and the pipeline actually runs, a
checksum present in the lock is recomputed against downloaded or cached bytes.
With attestations enabled, lock evidence remains audit data; a cached or live
proof bundle is still required. If the lock has no digest/evidence and
`require_checksums=false`, installation may proceed without cryptographic
integrity verification. A normal `install` reuses an existing `.osdk-complete`
version earlier and runs only backend post-install checks, without rehashing its
artifact. See [Sources and Supply-chain Security](./sources-security#integrity-signatures-and-attestation).

A locally linked Rust toolchain has no reproducible artifact, so `osdk lock`
rejects it explicitly.

## Do not confuse three kinds of “stale”

| State | Current behavior |
| --- | --- |
| Project `osdk.lock` is behind configuration | No freshness detection; run `lock` or `upgrade` |
| `<installs>/<tool>/.locks/<version>.lock` file remains | This is an OS-level exclusion-lock path; process exit releases the lock, and an empty file does not mean it remains held |
| Installation directory lacks `.osdk-complete` | Treat as a partial failed installation; delete and rebuild it after acquiring the object lock |

Model snapshots use per-snapshot OS locks too. A `stale` entry from `trust list`
instead means a configuration path or content hash no longer matches.
