# npm Developer Tools

osdk can manage command-line packages published to an npm registry as
independent developer tools. Requests use the `npm:<package>` namespace, while
installation, pinning, one-shot execution, inspection, upgrade, and removal use
the common osdk command surface.

::: tip Distinguish the two names first
`npm@11.5.2` installs the npm package manager. `npm:prettier@3` installs the
Prettier tool from the npm registry. To manage the registry package literally
named `npm` as a dynamic tool, use `npm:npm`.
:::

## Quick start

Install Prettier and pin it for the current project:

```bash
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .
```

`use` installs managed Node first when necessary, installs the package, creates
shims for commands in its private `.bin`, and writes `"npm:prettier" = "3"` to the
nearest project configuration. Add `--global` (or `-g`) for a user-level pin.
To run it once without changing configuration:

```bash
osdk exec --tool npm:prettier@3 -- prettier --check .
```

## Package and version syntax

```text
npm:<package>[@VERSION]
npm:@<scope>/<package>[@VERSION]
```

Both unscoped and scoped packages are supported:

```bash
osdk install npm:prettier@3
osdk install 'npm:@antfu/ni@0.21.12'
osdk exec -t 'npm:@antfu/ni@0.21.12' -- ni
```

For a scoped package, the first `@` belongs to the scope and the final `@`
separates the version. Shell quoting is usually not required, but single quotes
protect the request from other command wrappers. Omitting the version selects
the latest stable release; prefixes such as `3` and `3.6` select the highest
matching stable release.

## Complete lifecycle

The following examples cover the common command surface available to npm tools:

```bash
# Install or pin
osdk install npm:prettier@3
osdk use npm:prettier@3

# Run once without writing a project pin
osdk exec -t npm:prettier@3 -- prettier --check .

# Inspect
osdk list npm:prettier
osdk current npm:prettier
osdk where npm:prettier
osdk where npm:prettier@3.6.2

# Check or upgrade the project, or target one tool
osdk outdated
osdk outdated npm:prettier@3
osdk upgrade
osdk upgrade npm:prettier@3

# Remove an exact version; --yes is useful for automation
osdk --yes uninstall npm:prettier@3.6.2

# Rebuild shims for every installed tool
osdk reshim
```

| Command | npm tool behavior |
| --- | --- |
| `use` | Installs if needed, creates shims, and saves a project or user-level version; preserves the version prefix you typed |
| `install` | Installs explicit requests directly; with no arguments and no `-o`, prefers the current-platform `osdk.lock` |
| `exec` | Installs if needed and runs the package's actual exported bin in an exact environment without writing a pin |
| `list` | Lists installed versions; its argument is the versionless `npm:<package>` backend id |
| `current` | Shows the request selected by current-directory configuration; it does not prove that version is installed |
| `where` | Prints a matching install directory; pass an exact version to avoid selection ambiguity |
| `uninstall` | Removes an install and reconciles shims; automation should use an exact version and global `--yes` |
| `outdated` | Re-resolves the target and reports when that exact version is not installed; does not read the old lock |
| `upgrade` | Re-resolves and installs, then refreshes the host lock; does not use the old lock as resolution input |
| `reshim` | Rebuilds command entry points from installed inventories; takes no tool argument |

`osdk list-remote npm:prettier [FILTER]` also lists stable registry versions.

## Project configuration and build scripts

Use a string for a simple version pin and a structured `[tools]` entry for
installation policy. Quote TOML keys that contain `:` or a scoped package name:

```toml
[tools]
node = "22"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
allow_builds = ["@scope/native-tool", "esbuild"]
```

npm tool lifecycle/build scripts are **fully disabled by default**.
`allow_builds` accepts three policies:

| Configuration | Effect |
| --- | --- |
| Omitted or `false` | Deny build scripts for every dependency; the default and recommended policy |
| `["pkg-a", "pkg-b"]` | Allow build scripts only for the named packages |
| `true` | Allow build scripts throughout the graph; dangerous and intended only after full review |

For a one-shot command, use `-o allow_builds=esbuild,sharp`, or explicitly use
the dangerous `-o allow_builds=true`. Prefer an array in team configuration so
the allowlist remains easy to review.

## Managed Node and source selection

Dynamic npm tools always run on Node managed by osdk; they do not depend on a
system Node that happens to be on PATH. If the request does not include Node,
osdk adds the Node selected for the current project, or `node@latest` when the
project has no declaration. Node installs before the npm tool. Pin Node
explicitly when the team needs a repeatable runtime.

Each `npm:<package>` backend uses the common `sources.selection = "auto"`
strategy across npmmirror and the official npmjs source by default. Candidates
are probed concurrently and ranked by throughput and time to first byte; the
result is reused for its configured TTL. The cache is bound to a fingerprint of
the candidates, so URL, order, priority, enabled-state, or authentication-header
changes cannot reuse an incompatible ranking.

`Source.headers` here applies only to metadata requests and probes performed by
osdk. Headers are bound to the configured index/download origin, retained across
same-origin redirects, and removed after a cross-origin redirect. Aube 2.1
package fetches do not accept arbitrary `Source.headers`; configure private npm
registry authentication through Aube/npm's native trusted configuration or
environment path.

```bash
osdk source list npm:prettier
osdk source test npm:prettier
osdk --refresh-sources install npm:prettier@3
osdk --source npm install npm:prettier@3
```

This selects the **download source for the tool itself**. It is separate from
the `[registries.npm]` preflight performed before a project's `npm install`; see
[JavaScript Package Managers](./package-managers#registry-preflight).

## Locking and offline reinstall

For every dynamic npm tool, schema 2 `osdk.lock` records the package, exact Node
version, graph format, SHA-256, and content-addressed path. The complete Aube
graph is written beside it as `osdk.lock.d/npm/<sha256>.yaml`. The graph carries
transitive dependency integrity, and generating it never runs lifecycle scripts.
Commit both `osdk.lock` and `osdk.lock.d/` to the repository.

Prepare the main lock, graph sidecar, and Aube package cache/store before going
offline:

```bash
# Online: generate the complete graph and download packages referenced by it
osdk lock
osdk install

# Later, retain osdk.lock, osdk.lock.d/, and the same cache; reinstall frozen
osdk --offline install
```

Neither the main lock nor its sidecar means package contents are cached. Offline
installation fails explicitly if the sidecar is missing, corrupt, larger than
16 MiB, or any required cached content is absent. Explicit
`osdk --offline install npm:prettier@3.6.2` bypasses the project lock; use
argument-free `install` to restore from it. See [Reproducible Lockfiles](./lockfiles)
for the wider lock semantics.

## Command discovery and conflicts

After installation, osdk discovers commands under the synthetic project's
`node_modules/.bin` and records an inventory; those bins may come from the root
package or a transitive dependency. An empty `.bin`, an unresolved command
target, or a target escaping the install root is rejected. If different
backends expose the same command name, shim publication and runtime routing fail
closed instead of choosing by scan order. At runtime, selecting exactly one
owner in current configuration resolves the dispatch ambiguity. CLI generation
and `reshim` currently still remove the ambiguous shim and error when multiple
installed owners exist. Multiple versions of one backend use normal current-
version selection.

For the embedded Aube install, graph-sidecar validation, cache layout, and routing
algorithms, see [npm tool implementation](./implementation/npm-tools).
