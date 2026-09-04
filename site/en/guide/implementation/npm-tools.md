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
uses the second `@` as the version separator. Request parsing and inventory
identity both normalize package names to lowercase. Empty
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
the remaining tools concurrently. The managed npm subprocess runs against only
that managed Node; neither installation nor shim execution treats a system Node
on PATH as an implicit dependency.

`use` adds two scope-specific branches before that legacy flow. A non-global
`npm:*` request inspects the nearest `package.json` and, when present, mutates
that real project. A global request ignores project state and installs under an
osdk-owned prefix. If local discovery finds no `package.json`, execution falls
back to the original isolated backend path.

Before resolving or installing an osdk-owned dynamic npm tool,
[`identity_options`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs)
accepts only `installer` and `allow_builds` as public identity inputs. Internal
`__osdk_*` fields injected while replaying a lock are deliberately excluded. The
installer value is canonicalized, while `allow_builds` normalizes false-like
values by omitting the default deny policy, true-like values to `true`, and a
package list to a lowercase, sorted, deduplicated comma-separated value. An
`installer=auto` default is omitted as well. Unknown public keys fail before
installation. Those normalized material options feed the complete canonical
install identity together with tool, exact version, platform, scope, dependencies,
and materials; its domain-separated BLAKE3 `install_id` uses the `b3-v2:` format.

## Installer planning and one-shot delegation

[`npm_tools.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm_tools.rs)
plans the complete installer before mutation. `auto` resolves three signals in a
fixed order: a declared `packageManager`, then the installer that owns the
incumbent native lock, then the configured default. Current supported inputs are
pnpm v9 and npm lock v2/v3. A `packageManager` declaration is checked against the
lock owner; conflicting or multiple locks fail closed. Explicit
`installer=npm|pnpm` bypasses the declaration but not lock compatibility.

The last-resort default comes from `settings.npm.default-installer`
(`OSDK_NPM_DEFAULT_INSTALLER`) and is modelled as its own enum rather than
reusing the planner's installer type, so `auto` can never be configured as its
own fallback and adding a future backend touches one declaration. Because the
default is consulted only after both project signals, changing it never migrates
a project that declares an installer or already owns a lockfile.

Initial discovery chooses a candidate installer without mutation. After taking
the per-project npm lock, osdk reopens the manifest and native lock and replans
from the original requested installer (`auto` or an explicit choice), so a
concurrent lock-owner change cannot leave a stale concrete plan. The concrete
plan selected under that lock is then fixed for invocation. Native project
delegation uses an exact managed npm or pnpm executable, a managed Node, a
preflighted registry environment, and one subprocess invocation. Any non-zero
exit is returned as-is; there is no fallback replay through another native
manager. Project dependency-section selection is also fixed before invocation:
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

## Isolated and global npm execution

Isolated `install` and `exec` use
[`npm_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/npm_package.rs)
and
[`native_npm.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/native_npm.rs),
which runs the managed Node's own `npm-cli.js` as a subprocess. That path creates
one osdk-owned synthetic project per package/version; osdk owns Node, version
selection, and lifecycle orchestration.

Running npm as a subprocess rather than linking a resolver in-process is a
deliberate boundary. `osdk` is a multi-threaded Tokio runtime, and a library that
mutates the process working directory or environment would corrupt unrelated
worker threads. A child process cannot; it also keeps a hung or crashed installer
from taking the CLI down with it.

The child's environment is cleared and rebuilt from a minimal allowlist rather
than inherited. `NPM_CONFIG_CACHE` points at the osdk-owned cache, and both
`NPM_CONFIG_USERCONFIG` and `NPM_CONFIG_GLOBALCONFIG` point at an osdk-owned
empty `.npmrc`, so an ambient user or global `npmrc` cannot redirect the registry
or re-enable lifecycle scripts. On Windows osdk invokes `node` with the resolved
`npm-cli.js` path instead of `npm.cmd`, avoiding the shell wrapper entirely.
Proxy variables are forwarded deliberately; a 30-minute timeout and 4 MiB
stdout/stderr capture limits bound a runaway install.

Global `use` runs the selected manager's real global-add inside an
osdk-controlled prefix. npm writes the standard prefix layout, which osdk then
adapts:

```text
<global-install>/lib/node_modules/<package>/    # Unix
<global-install>/node_modules/<package>/        # Windows

<cache>/npm/v1/cache/
<store>/npm/
```

osdk locates the selected root package in that tree, clears the native bin
directory, and reconstructs relocatable launchers solely from the selected
package's declared `bin` entries. It then validates package identity, exact
version, target containment, every launcher, inventory, and completed state
before promoting the staged root and publishing shims.

The npm cache and store are shared across isolated, project, and global
operations, while each project or controlled global install retains its own
native lock. `npm install --global` does not write a lockfile, so no native-lock
identity is recorded for that scope. npm and pnpm global delegates pass through
their native offline flags when installation is required.

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
cross-origin redirect. The managed npm/pnpm delegates receive only a registry
override inside an otherwise empty osdk-owned configuration, so package fetches
do not forward `Source.headers`. Global managed-tool installation currently rejects native
authenticated, scoped, private, TLS-customized, or proxy registry pass-through
because that state cannot be copied into the isolated prefix without widening
the credential boundary; use an anonymous configured registry for this path.
Registry preflight in `package_registry.rs`
applies to npm/pnpm/Yarn/Bun/Deno commands that users run later, evaluates each
invocation independently, and must not be described as using this TTL cache.

## Build-script policy and structured configuration

For isolated and global installs, the default `BuildPolicy::Deny` disables root
and transitive lifecycle/build scripts by passing `--ignore-scripts` to the
managed npm subprocess.
`allow_builds` comes from a CLI string or structured `[tools]` entry:

- false values and an empty value remain deny;
- a package array is recorded in the installation identity, but npm has no
  per-package equivalent of pnpm's `onlyBuiltDependencies` -- `--ignore-scripts`
  is all-or-nothing -- so under npm the allowlist **fails closed to deny** rather
  than silently allowing the whole graph. Select pnpm when a genuine per-package
  allowlist is required;
- a true value drops `--ignore-scripts`, explicitly allowing scripts throughout
  the dependency graph.

Because the allowlist is still part of the material option identity, changing it
re-fingerprints the install even where npm cannot enforce it; the recorded intent
therefore stays accurate if the package is later reinstalled under pnpm.

Regardless of the eventual install policy, `osdk lock` uses
`ignore_scripts = true`, `run_root_lifecycle = false`, and
`lockfile_only = true` during graph generation. Structured tool entries support
strings, booleans, and string arrays. During layered configuration, a complete
higher-precedence tool entry replaces the lower entry with the same name; its
fields are not merged. `use -o allow_builds=esbuild,sharp` normalizes and
persists a string array, while true/false becomes a boolean, keeping generated
project configuration structured.

## Lock schema 4, compatible npm metadata, and native graph ownership

The current write format for `osdk.lock` is lock schema 4, retaining schema 3's
npm metadata model. Each npm tool records its
request, exact version, options, and `npm` metadata. It does not write a
generic `artifact` table, and it no longer stores any graph payload or graph
path in the main lock:

```toml
[platforms.linux-x64.tools."npm:prettier".npm]
package = "prettier"
installer = "npm"
scope = "project"
node_version = "24.1.0"      # optional

[platforms.linux-x64.tools."npm:prettier".npm.native_lock]
kind = "npm"
format = "package-lock-v3"
sha256 = "<64 lowercase hex characters>"
```

Public `installer` and `allow_builds` options remain in the lock's `options`
table and are replayed into the request; internal `__osdk_*` metadata is never
serialized there. This is separate from `.osdk-install.json` schema 1 below. The
older “schema 2 sidecar” terminology in this section means lock schema 2's npm
graph sidecar, not a dynamic install-identity format.

On write, the CLI extracts npm metadata from the installed tool or declared
private options: package name, installer, scope, an optional exact Node
version, and optional native-lock owner/format/SHA-256. Project-aware `use`
hashes the native lock beside the real `package.json`. The native lock remains
owned by the chosen package-manager operation. Global pnpm installs retain their
native lock under the controlled install root and record its identity in the user
lock; npm's real global mode creates no dependency lock, so that identity is
absent. The native
payload itself is never persisted in
`osdk.lock`. The main lock is currently limited to 16 MiB, and schema 4 writes
only atomically replace the main lock.

This is an intentional limitation: the compatible npm metadata does not capture the
transitive dependency graph and cannot reconstruct it by itself. The real
project's native lock, or the pnpm native lock in a controlled global install
directory, remains the graph source of truth. npm global installs have
no equivalent graph lock.

Argument-free `osdk install` reading a schema 3 or 4 lock reinjects that metadata as
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

Legacy lock-schema-2 sidecars remain frozen-read compatible. When an older sidecar entry is
read, osdk still validates the package, Node version, `package-lock-v3`, 64-character
lowercase SHA-256, canonical sidecar path, and non-symlink sidecar directory and
file, then rereads the full UTF-8 payload with the 16 MiB bound and recomputes
its digest. Only after that validation does the graph become a compatibility
input to the backend. A later successful write migrates the entry to lock schema 4
metadata only; the existing sidecar file is not deleted automatically.

## Isolated/global install identity, shims, and conflict rejection

For an isolated install, the backend scans the synthetic project's complete
`node_modules/.bin`, so recorded bins may come from the root package or
transitive dependencies. For a global install, normalization instead resets the
manager-produced bin directory and recreates launchers only for the selected
root package's declared bins. Both paths write `.osdk-install.json` schema 1. Its
nested `identity` contains `tool`, exact `version`, `platform`, `scope`, canonical
`material_options`, `dependencies`, `materials`, and `install_id`. `install_id` is
the domain-separated canonical `b3-v2:` identity and is also used in the physical
root. Relative executable paths are validated against that root; backend-specific
observations such as graph integrity and native-lock hashes live in a separate
receipt and never become alias ownership. A bin name must be a single filename and its resolved canonical target
must remain inside the root; missing bins, duplicate names, traversal, or a
tampered identity are rejected. Scans do not follow symlinks and bound traversal
depth, manifest count, and file size.

Dynamic roots are fingerprinted beneath the backend and exact version, with
isolated and global npm remaining separate namespaces. Consequently, multiple
same-backend/version identities can coexist in one scope. Reuse and lifecycle
commands derive the exact identity first and never treat another fingerprint as a
version-compatible fallback.
In the current schema-4 lock bridge, the exact managed Node dependency is part
of the install ID. A legacy frozen-graph digest participates when it is already
an input before installation. Compact native-lock hashes are currently injected
both during replay and after a fresh install, so they remain validated receipt
evidence rather than a path selector. Unlocked observed graph/SRI data follows
the same rule.

`.osdk-tool.json` is legacy detection metadata only. Neither its old schema 1 nor
schema 2 authorizes reuse, activation, shim execution, `where`, uninstall, or
`reshim`; there is no compatibility contract between those dynamic-inventory
schemas. A legacy install must be reinstalled to publish `.osdk-install.json`
schema 1.

The CLI and shim derive a `bin name -> backend owner` map from exact install
identity records. For an active dynamic request, reuse, activation, shim dispatch,
`where`, uninstall, and `reshim` select the fingerprinted root whose complete
`b3-v2:` identity matches the request. A missing, legacy, or mismatched identity
fails closed rather than exposing its bin paths or selecting a sibling root.
Bin-owner resolution is a separate check:

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

Unit and contract tests cover namespaced/scoped parsing, option normalization,
canonical `b3-v2:` install identity, schema-1 identity validation, installer planning,
dependency-section retention, one-shot native delegation, compact lock metadata,
global-prefix arguments, native-lock identity, curated generation publication
and revalidation, raw-project-bin exclusion, inventory scanning, and shim
conflict behavior. Compatibility tests retain legacy lock-schema-2 sidecar
validation, legacy `.osdk-tool.json` detection without execution, and
lock-schema-1 npm-migration rejection boundaries. Cross-platform changes remain
subject to the repository's Linux workspace tests and full Windows GNU Wine
suite.
