# npm Developer Tool Implementation

This page describes the internal boundaries of the dynamic `npm:<package>`
backend. See [npm Developer Tools](../npm-tools) for user commands and
configuration examples. This backend is related to, but distinct from, the
standalone `npm` CLI backend and the registry preflight for project dependency
commands.

## Identity, resolution, and lifecycle orchestration

[`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs)
recognizes `npm:` first. For an unscoped package, the `@` after its name
separates the version. For a scoped package it first parses `@scope/name`, then
uses the second `@` as the version separator. Request parsing preserves package
name casing, while inventory identity is normalized to lowercase; use npm's
conventional lowercase package spelling to avoid an identity mismatch. Empty
names, scope-only names, extra path segments, backslashes, colons, and whitespace
are rejected. Bare `npm` continues to map to the built-in npm CLI backend and
cannot be shadowed by the dynamic backend.

[`Registry::get`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/registry.rs)
constructs `NpmPackageBackend` on demand. The common lifecycle needs no special
Clap subcommand: `use`, `install`, `exec`, `outdated`, and `upgrade` pass the
same `ToolRequest` to the backend, while `list`, `current`, `where`, `uninstall`,
and `reshim` recover dynamic backends from configuration and disk inventory.

When a request set contains an npm tool but no Node,
[`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs)
adds the Node selected for the current directory, or `latest` when none is
declared. Installation orchestration completes Node serially before scheduling
the remaining tools concurrently. Aube's runtime selector points only to that
managed Node; neither installation nor shim execution treats a system Node on
PATH as an implicit dependency.

`use` adds two scope-specific branches before that legacy flow. A non-global
`npm:*` request inspects the nearest `package.json` and, when present, mutates
that real project. A global request ignores project state and installs under an
osdk-owned prefix. If local discovery finds no `package.json`, execution falls
back to the original isolated backend path.

## Installer planning and one-shot delegation

[`npm_tools.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm_tools.rs)
plans the complete installer before mutation. `auto` selects Aube for a new
project or any supported incumbent native lock. Current supported inputs are
Aube/pnpm v9 and npm lock v2/v3. A known unsupported npm or pnpm format selects
its native owner instead. A `packageManager` declaration is checked against the
lock owner; conflicting or multiple locks fail closed. Explicit
`installer=aube|npm|pnpm` bypasses the declaration but not lock compatibility.

Initial discovery chooses a candidate installer without mutation. After taking
the per-project npm lock, osdk reopens the manifest and native lock and replans
from the original requested installer (`auto` or an explicit choice), so a
concurrent lock-owner change cannot leave a stale concrete plan. The concrete
plan selected under that lock is then fixed for invocation. Native project
delegation uses an exact managed npm or pnpm executable, a managed Node, a
preflighted registry environment, and one subprocess invocation. Any non-zero
exit is returned as-is; there is no fallback replay through Aube or another
native manager. Project dependency-section selection is also fixed before invocation:
existing production, optional, peer, or development placement is preserved and
a missing package defaults to development dependencies. All project-add paths
disable lifecycle scripts.

## Real-project publication and activation

After a project installer succeeds, osdk verifies the installed package's name
and exact version, parses its declared bin entries, confines each canonical
target to the package directory, and checks the package-manager launcher. It
does not put the raw `node_modules/.bin` on PATH. Instead, it builds an immutable
curated generation containing only the declared bins of npm packages selected
in project configuration:

```text
<project>/.osdk/npm-bin/
  current
  publish.lock
  generations/<sha256>/
    manifest.json
    bin/<activated curated launchers>

<project>/node_modules/<configured-package>/<declared target>
<project>/node_modules/.bin/<source launcher, validation only; never activated>
```

The generation ID is SHA-256 over its schema, platform, sorted selections, and
sorted bin records. On Unix the curated entries are relative symlinks to the
canonical declared package targets; on Windows they are constrained `.cmd`
wrappers invoking managed Node. Duplicate command names across selected
packages fail closed (case-insensitively on Windows). Generation construction
uses a staging-directory rename, and the JSON `current` pointer is written
through an atomically replaced temporary file. Existing selections are carried
forward only while their configured specs still match. A later transaction
failure restores the previous pointer; completed unreferenced generations may
remain and there is currently no stale-generation garbage collection.

After publication, osdk atomically updates `osdk.toml` with exact Node plus the
structured npm selection, writes compact native-lock metadata to project
`osdk.lock`, and trusts the exact generated config. It does not create an
osdk-private npm tool install for this branch. `.osdk/npm-bin/` is derived local
state and should normally be ignored with `/.osdk/npm-bin/` at the package root
or `**/.osdk/npm-bin/` for nested workspace packages, rather than ignoring every
possible future `.osdk` file.

At each shell activation, osdk first requires an npm selection from a trusted
project config at the same canonical root as the nearest regular
`package.json`. It then revalidates the `current` pointer, schema/platform and
content-derived generation identity, owned non-symlink directories, exact file
set, configured specs, installed package identities and versions, declared
targets, and every curated launcher. Missing or invalid state is silently
omitted from the activation delta. A valid curated bin directory is prepended
ahead of osdk shims and managed runtimes, unless it contains a `node` command,
in which case the entire generation is omitted.

## Embedded Aube installation

For isolated and global Aube installs,
[`npm_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/npm_package.rs)
creates an osdk-owned synthetic project for each package/version.
[`aube_host.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/aube_host.rs)
embeds Aube through its library API instead of spawning `npm install -g`. The
host disables Aube runtime switching, self engine checks, and self-update; osdk
owns Node, version selection, and lifecycle orchestration.

Install paths for isolated and global tools remain separated by canonical
backend and version. Aube's cache and store, however, are deliberately shared
across packages, versions, and project/global scopes:

```text
<installs>/npm/<package>/<version>/
  project/package.json
  project/aube-lock.yaml
  project/node_modules/.bin/...  # isolated/global source bins exposed via osdk shims
  .osdk-tool.json
  .osdk-complete

<cache>/aube/v1/cache/
<store>/aube/
```

Actual paths use the platform-safe tool-ID mapping, so scoped packages retain
nested components. The install publishes `.osdk-complete` only after the package
directory, `.bin` directory, resolvable discovered commands, and inventory have
all been written. Failure paths attempt to remove the incomplete install root.
Root package metadata must provide parseable SHA-256 or SHA-512 SRI; otherwise
installation fails. Aube's graph carries integrity for the full transitive set.

## Automatic source selection and cache identity

The dynamic npm backend reuses the npm CLI backend's artifact sources, which are
npmmirror and the official npmjs registry by default. They pass through the
common [`ranked_source_list`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs):

1. a pin comes first, with the other sources retained for failure fallback;
2. `ordered` follows priority;
3. the default `auto` mode probes concurrently and ranks throughput first, with
   time to first byte as the secondary factor;
4. results are reused for `cache_ttl`, while `--refresh-sources` forces a probe.

The source-probe cache uses schema 2 and stores a BLAKE3 fingerprint of the
candidate set. The fingerprint covers order, ID, kind, index/download URLs,
priority, enabled state, credential forwarding, and header names; header values
are represented only by hashes. A changed candidate set therefore cannot reuse
an incompatible ranking, and credentials are not persisted in clear text.
Offline mode can reuse a compatible cached order; with none, it keeps static
source order and performs no probe.

osdk's own npm metadata requests and source probes honor explicit
`Source.headers`, but only for the configured index/download origin. Headers
survive same-origin redirects and are permanently removed after the first
cross-origin redirect. Aube 2.1's embedded API cannot safely accept arbitrary
source headers, so Aube package fetches do not forward `Source.headers`; an
authenticated registry must use Aube/npm's native trusted configuration or
environment path. Registry preflight in `package_registry.rs` applies to
npm/pnpm/Yarn/Bun/Deno commands that users run later, evaluates each invocation
independently, and must not be described as using this TTL cache.

## Build-script policy and structured configuration

For isolated and global installs, the default `BuildPolicy::Deny` passes
`ignore_scripts = true` to Aube, so root and transitive lifecycle/build scripts
do not run. `allow_builds` comes from a CLI string or structured `[tools]` entry:

- false values and an empty value remain deny;
- a package array becomes a comma-separated request option and then
  `package.json#aube.allowBuilds` in the synthetic project;
- a true value sets Aube's `dangerously_allow_all_builds`, explicitly allowing
  scripts throughout the dependency graph.

Regardless of the eventual install policy, `osdk lock` uses
`ignore_scripts = true`, `run_root_lifecycle = false`, and
`lockfile_only = true` during graph generation. Structured tool entries support
strings, booleans, and string arrays. During layered configuration, a complete
higher-precedence tool entry replaces the lower entry with the same name; its
fields are not merged. `use -o allow_builds=esbuild,sharp` normalizes and
persists a string array, while true/false becomes a boolean, keeping generated
project configuration structured.

## Schema 3, metadata-only locks, and native graph ownership

The current write format for `osdk.lock` is schema 3. Each npm tool records its
request, exact version, options, and `npm` metadata. It does not write a
generic `artifact` table, and it no longer stores any graph payload or graph
path in the main lock:

```toml
[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "aube"
scope = "project"
node_version = "24.1.0"      # optional

[platforms.linux-x64.tools."npm:prettier".npm.native_lock]
kind = "aube"
format = "aube-v9"
sha256 = "<64 lowercase hex characters>"
```

On write, the CLI extracts npm metadata from the installed tool or declared
private options: package name, installer, scope, an optional exact Node
version, and optional native-lock owner/format/SHA-256. Project-aware `use`
hashes the native lock beside the real `package.json`. Global Aube and pnpm
installs retain their native locks under the controlled install root and record
their identity in the user lock; npm's real global mode creates no dependency
lock, so that identity is absent. The payload itself is never persisted in
`osdk.lock`. The main lock is currently limited to 16 MiB, and schema 3 writes
only atomically replace the main lock.

This is an intentional limitation: schema 3 metadata does not capture the
transitive dependency graph and cannot reconstruct it by itself. The real
project's native lock, or the Aube/pnpm native lock in a controlled global
install directory, remains the graph source of truth. npm global installs have
no equivalent graph lock.

Argument-free `osdk install` reading a schema 3 lock reinjects that metadata as
private options and validates package/backend identity, installer/scope
consistency, any exact same-platform Node version, and any recorded native-lock
format/SHA-256 against the owning installer's constraints. The main lock no
longer provides a graph/path field, so this path restores metadata rather than
a sidecar path.

A schema 1 lock without npm entries remains readable and upgrades on its next
successful write. A schema 1 lock containing any `npm:*` entry, including the
old inline graph representation, cannot be consumed, merged, or saved and must
be regenerated instead of being falsely migrated to the current metadata-only
format.

Schema 2 sidecars remain frozen-read compatible. When an older sidecar entry is
read, osdk still validates the package, Node version, `aube-v9`, 64-character
lowercase SHA-256, canonical sidecar path, and non-symlink sidecar directory and
file, then rereads the full UTF-8 payload with the 16 MiB bound and recomputes
its digest. Only after that validation does the graph become a compatibility
input to the backend. A later successful write migrates the entry to schema 3
metadata only; the existing sidecar file is not deleted automatically.

## Isolated/global inventory, shims, and conflict rejection

For isolated and global installs, the backend scans the synthetic project's entire
`node_modules/.bin` and writes the tool ID, exact version, relative bin paths,
and stable metadata to `.osdk-tool.json`; those bins may come from the root
package or transitive dependencies. A bin name must be a single filename and its
resolved canonical target must remain under the install root. Missing bins,
duplicate names, path traversal, and corrupt inventories are rejected. Inventory
scans do not follow symlinks and bound traversal depth, manifest count, and file
size.

The CLI and shim derive a `bin name -> backend owner` map from inventory:

- one owner routes directly;
- at runtime, multiple owners with exactly one selected in current configuration
  route to that owner;
- runtime dispatch refuses when multiple candidates remain; CLI shim generation
  and rebuild always fail closed across multiple installed backend owners,
  removing the ambiguous managed shim and returning an error;
- multiple versions of one backend are not an owner conflict and use normal
  current-directory version selection;
- coordinated `npm`/`npx` routing between Node and the standalone npm backend is
  the sole exception.

The fail-closed boundary is therefore **shim publication and command routing**.
Package contents may already have been materialized; the conflict must not be
described as a globally rolled-back installation transaction.

## Main verification points

Unit and contract tests cover namespaced/scoped parsing, installer planning,
dependency-section retention, one-shot native delegation, compact lock metadata,
global-prefix arguments, native-lock identity, curated generation publication
and revalidation, raw-project-bin exclusion, inventory scanning, and shim
conflict behavior. Compatibility tests retain the schema 2 sidecar validation
and schema 1 npm-migration rejection boundaries. Cross-platform changes remain
subject to the repository's Linux workspace tests and full Windows GNU Wine
suite.
