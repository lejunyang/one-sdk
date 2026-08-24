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

## Embedded Aube installation

[`npm_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/npm_package.rs)
creates an osdk-owned synthetic project for each package/version.
[`aube_host.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/aube_host.rs)
embeds Aube through its library API instead of spawning `npm install -g`. The
host disables Aube runtime switching, self engine checks, and self-update; osdk
owns Node, version selection, and lifecycle orchestration.

Install and cache paths are logically isolated by canonical backend and version:

```text
<installs>/npm/<package>/<version>/
  project/package.json
  project/aube-lock.yaml
  project/node_modules/.bin/...
  .osdk-tool.json
  .osdk-complete

<cache>/aube/npm/<package>/<version>/cache/
<cache>/aube/npm/<package>/<version>/store/
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

The default `BuildPolicy::Deny` passes `ignore_scripts = true` to Aube, so root
and transitive lifecycle/build scripts do not run. `allow_builds` comes from a
CLI string or structured `[tools]` entry:

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

## Schema 2, graph sidecars, and frozen restoration

The current write format for `osdk.lock` is schema 2. Each npm tool records its
request, exact version, options, and `npm` sidecar metadata; it does not write a
generic `artifact` table:

```toml
[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
node_version = "24.1.0"
lock_format = "aube-v9"
sha256 = "<64 lowercase hex characters>"
graph = "osdk.lock.d/npm/<sha256>.yaml"
```

For npm tools, `osdk lock` is more than metadata resolution. The CLI first
ensures managed Node is installed, then calls `prepare_lock_graph`, which uses
Aube's lockfile-only mode and isolated cache/store to resolve the complete graph.
The original UTF-8 bytes of the installed `aube-lock.yaml` are addressed by
SHA-256 and atomically written to `osdk.lock.d/npm/<sha256>.yaml`; the main lock
stores only the five fields above. Both the main lock and each graph sidecar are
currently limited to 16 MiB. Sidecars are written and read back for validation
before the main lock is atomically replaced. Commit `osdk.lock` and
`osdk.lock.d/` together.

Argument-free `osdk install` first validates package/backend identity, the exact
same-platform Node version, `aube-v9`, the 64-character lowercase SHA-256, and
the path uniquely derived from that digest. It rejects symlinks in the sidecar
path, reads at most 16 MiB, and validates UTF-8 plus SHA-256 over the actual
bytes. Only then is the graph passed as a private backend option, restored as
`aube-lock.yaml`, and installed in Aube frozen mode.

A schema 1 lock without npm entries remains readable and upgrades on its next
successful write. A schema 1 lock containing any `npm:*` entry, including the
old inline graph representation, cannot be consumed, merged, or saved and must
be regenerated instead of being falsely migrated to the sidecar format.

Offline use requires valid schema 2 main-lock metadata, a committed graph
sidecar that passes validation, and an Aube cache/store containing every package
referenced by that graph; the locked managed Node must also be available. Missing
any part fails without network fallback. An existing install is reused only
after full identity and layout validation, not merely because `.osdk-complete`
exists.

## Inventory, shims, and conflict rejection

Before completing an install, the backend scans the synthetic project's entire
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

Unit and contract tests cover namespaced/scoped parsing, build policy, sidecar
identity/format/digest and exact-byte round trips, plus rejection of missing,
tampered, oversized, and symlinked sidecars. They also cover schema 1 npm
migration rejection, offline rejection without a graph, bin path confinement,
inventory scanning, managed Node injection at shim runtime, and reshimming
multiple versions of one dynamic backend. Cross-platform changes remain subject
to the repository's Linux workspace tests and full Windows GNU Wine suite.
