# npm Developer Tools

osdk accepts command-line packages from an npm registry through the
`npm:<package>` namespace. `osdk use` is context-aware: it can add a tool to a
real Node project, install it as a user-wide global tool, or retain the original
isolated osdk installation when there is no Node project.

::: tip Distinguish the two names first
`npm@11.5.2` installs the npm package manager. `npm:prettier@3` selects the
Prettier package from the npm registry. To select the registry package literally
named `npm`, use `npm:npm`.
:::

## How `use` chooses the scope

| Command and context | Installation target | Configuration and lock |
| --- | --- | --- |
| `osdk use npm:prettier@3` below a `package.json` | The nearest real Node project | `osdk.toml`, `osdk.lock`, the native package lock, and a local curated command generation |
| `osdk use --global npm:prettier@3` | An osdk-controlled global prefix | User `config.toml` and user `osdk.lock`; the current project is ignored |
| Local `use` with no `package.json` in the ancestor chain | The legacy isolated osdk install | The normal project pin plus an osdk-owned synthetic project and shim |

The nearest regular `package.json` is a hard project boundary. osdk does not
fall through a malformed or symlinked nearer manifest to an outer project.
Explicit `osdk install npm:...` and `osdk exec --tool npm:...` continue to use
the isolated managed-tool flow; project mutation is specific to local `use`.

## Add a tool to a Node project

Run `use` anywhere below the project root, then enable shell activation:

```bash
cd my-app/packages/web/src
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .
```

osdk resolves and installs a managed Node first, then adds Prettier to the
nearest project. If the package already appears in `dependencies`,
`devDependencies`, `optionalDependencies`, or `peerDependencies`, it remains in
that section. A new package defaults to `devDependencies`. A package present in
both peer and development dependencies retains both roles. Project adds always
disable lifecycle scripts.

After the installer returns, osdk verifies the installed package name and exact
version, its declared `bin` targets, and the corresponding launchers under
`node_modules/.bin`. A missing executable, an identity mismatch, or a path that
escapes the package fails closed.

It then publishes only the configured package's declared, validated commands in
an immutable osdk-owned generation:

```text
.osdk/npm-bin/generations/<sha256>/bin/
```

The generated launchers still execute the package's real declared targets under
`node_modules/<package>`; the package manager's broad `node_modules/.bin` is
used for post-install validation but is never placed on PATH by osdk. As more
npm tools are configured, a new generation retains only selections whose exact
configured specs still match the project. Conflicting command names fail closed.

The operation also writes or updates a structured project selection similar to:

```toml
[tools]
node = "22.17.0"
"npm:prettier" = { version = "3", installer = "npm" }
```

The exact managed Node version and concrete installer are recorded together. An
existing Node tool entry keeps its other options. osdk automatically trusts the
exact generated `osdk.toml` content because activation can expose project code;
editing that file changes its trust identity and requires review and
`osdk trust` again.

## Installer selection

Before changing the project, osdk reads the nearest `package.json` and the
recognized native locks beside it. Automatic selection consults three signals in
a fixed order and stops at the first one that answers:

1. `package.json#packageManager`, then `devEngines.packageManager`, as the
   project's own statement of which installer owns the tree.
2. The installer that owns an existing recognized lock, so a project keeps the
   manager that already wrote its lockfile.
3. The configured default, which applies only when the project states nothing.

Recognized lock formats are npm `package-lock.json` / `npm-shrinkwrap.json` v2
or v3, and pnpm v9. A declaration and the one existing native lock must agree;
two or more recognized lockfiles are rejected as ambiguous before anything is
changed. Automatic mode accepts declared npm or pnpm; another manager requires
an explicit supported installer choice.

The fallback in step 3 ships as npm and is configurable:

```toml
# config.toml
[settings.npm]
default-installer = "npm"   # or "pnpm"
```

`OSDK_NPM_DEFAULT_INSTALLER` overrides it for one invocation. Because it is only
consulted last, changing it never takes a project away from the installer it
declares or already has a lockfile for.

Choose an installer explicitly when required:

```bash
osdk use npm:prettier@3 -o installer=npm
osdk use npm:prettier@3 -o installer=pnpm
```

An explicit choice overrides the package-manager declaration, but it still must
be compatible with the incumbent lock: npm and pnpm will not overwrite each
other's lock. The installer is selected before mutation and is invoked at most
once. A failed npm or pnpm operation is returned directly; osdk never replays it
through a different installer.

## Project activation and trust boundary

For a trusted npm-bearing project configuration, the shell hook prepends only
the active curated `.osdk/npm-bin/generations/<sha256>/bin`. It never adds the
raw project `node_modules/.bin`. Activation is read-only and enables the curated
directory only when all of these checks pass:

- the trusted `osdk.toml` belongs to the same root as the nearest regular
  `package.json`;
- `.osdk/npm-bin/current` is a valid regular JSON pointer to a generation whose
  schema, platform, content-derived ID, and manifest agree;
- all owned `.osdk/npm-bin` directories remain non-symlink directories inside
  the project, and the generation contains exactly its declared files;
- every published package/spec still matches trusted project configuration,
  and its installed name, exact version, declared targets, and curated
  launchers all revalidate;
- the curated generation does not provide `node`, which could replace the
  selected managed runtime.

If the pointer, generation, or any target is absent, stale, modified, or unsafe,
the hook omits the entire curated directory. When valid, the curated commands
precede osdk shims and managed runtime paths. A successful `use` publishes a
generation—building new content through staging—and atomically replaces the
`current` pointer; older completed generations may remain as disposable local
state.

Add the narrow derived directory to an ignore file at that package root:

```text
/.osdk/npm-bin/
```

For a repository-root rule that should cover nested workspace packages, use
`**/.osdk/npm-bin/`. Prefer either narrow rule to ignoring all of `.osdk/`, so
future project metadata under that directory can still be committed deliberately.

## Install a global npm tool

Global scope ignores the current project's manifest, declaration, and locks:

```bash
# The configured default installer is used; npm out of the box.
osdk use --global npm:prettier@3

# Use a managed native package manager explicitly.
osdk use -g npm:eslint@9 -o installer=npm
osdk use -g 'npm:@antfu/ni@0.21.12' -o installer=pnpm
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

osdk installs the selected Node and the requested package manager. Each
installer then runs its real global-add operation in an osdk-controlled prefix:
npm uses `install --global --prefix ...` and pnpm uses `add --global`. osdk
adapts the resulting native global layout, validates the selected package and
its declared commands, and publishes only those commands through osdk shims.
Neither mode writes to the ambient Node installation or the current project.

The selected version and installer are written to the user configuration, and
basic package, scope, Node, installer, and optional native-lock identity are
written to `$OSDK_CONFIG_DIR/osdk.lock`. npm global installs do not produce a
dependency lock, so no native-lock identity is recorded for them. pnpm's
`pnpm-lock.yaml` remains in its controlled install directory.

`where --global` resolves only against global npm installations and ignores a
project selection. `uninstall --global` removes the canonical global root and
any explicitly global legacy root, then removes the matching user config and
user-lock entries plus shims that no other installed tool owns. These metadata
changes and the install removal are serialized and rolled back together on a
failure.

## Package and version syntax

```text
npm:<package>[@VERSION]
npm:@<scope>/<package>[@VERSION]
```

Both unscoped and scoped packages are supported:

```bash
osdk use npm:prettier@3
osdk install 'npm:@antfu/ni@0.21.12'
osdk exec -t 'npm:@antfu/ni@0.21.12' -- ni
```

For a scoped package, the first `@` belongs to the scope and the final `@`
separates the version. Shell quoting is usually not required, but single quotes
protect the request from other command wrappers. Omitting the version selects
the latest stable release; prefixes such as `3` and `3.6` select the highest
matching stable release.

## Other lifecycle commands

```bash
# Isolated install or one-shot execution; neither edits package.json.
osdk install npm:prettier@3
osdk exec --tool npm:prettier@3 -- prettier --check .

# Inspect current selection and osdk-owned isolated/global installs.
osdk current npm:prettier
osdk list npm:prettier
osdk where npm:prettier@3.6.2

# The default lifecycle stays project/isolated; opt in to global explicitly.
osdk --yes uninstall npm:prettier@3.6.2
osdk where --global npm:prettier@3.6.2
osdk --yes uninstall --global npm:prettier@3.6.2
osdk reshim
```

Plain `where` follows the explicit/current configuration scope and otherwise
retains its isolated-first compatibility behavior. Within that scope, both
commands select the exact configured install identity; plain `uninstall` removes
only that isolated identity, while `--global` targets the matching user-wide
identity. Other identities of the same package version remain installed. A
package added to a real project remains owned by that project and its package
manager. Activated commands come from the curated
generation, whose launchers target the configured package's validated declared
files under `node_modules/<package>`.
`osdk list-remote npm:prettier [FILTER]` lists stable registry versions.

## Build-script policy

Local project `use` always disables lifecycle scripts, for both the native npm
and pnpm delegates. For isolated and global installs, scripts are also disabled
by default. Reviewed packages can opt in with a structured tool entry or a
one-shot option:

```toml
[tools."npm:@scope/native-tool"]
version = "1.2.3"
installer = "pnpm"
allow_builds = ["@scope/native-tool", "esbuild"]
```

| Configuration | Isolated/global effect |
| --- | --- |
| Omitted or `false` | Deny all dependency build scripts |
| `["pkg-a", "pkg-b"]` | Allow only named packages where the selected installer supports an allowlist |
| `true` | Allow scripts throughout the graph; use only after full review |

The one-shot form is `-o allow_builds=esbuild,sharp`. npm has no per-package
equivalent of pnpm's `onlyBuiltDependencies` -- its `--ignore-scripts` is
all-or-nothing -- so a named-package allowlist is recorded but treated as deny
under npm rather than silently allowing the whole graph. Use `true` to allow
scripts under npm, or select pnpm when a genuine per-package allowlist is
required.

## Option changes and reinstallation

For osdk-owned isolated and global installs, `installer` and `allow_builds` are
material options in the installation identity, not hints that may be ignored
after a version match. `.osdk-install.json` schema 1 stores a nested `identity`
object containing `tool`, `version`, `platform`, `scope`, `material_options`,
`dependencies`, `materials`, and `install_id`. The `install_id` is a canonical
`b3-v2:` digest of that identity.

The physical root includes this fingerprint, so two identities of the same npm
package and exact version can coexist in one scope. Reuse, activation, shim
dispatch, `where`, `uninstall`, and `reshim` derive the same exact identity from
the active request and select only its root; they never fall back to a sibling
identity merely because its version matches.

Older `.osdk-tool.json` files are scanned only to detect and report legacy
installs. They cannot authorize reuse or execution, regardless of whether their
old inventory schema is 1 or 2; there is no dynamic-inventory schema-1/schema-2
compatibility path. Reinstall to create `.osdk-install.json` schema 1. An offline
reinstall still needs the native lock or graph and warmed cache/store required by
the selected installer. Do not edit either identity file by hand. Project-managed
npm packages remain owned by the real project and its curated `.osdk/npm-bin`
generation, not by these fingerprinted install roots.

## Sources and shared storage

Dynamic npm version metadata uses the normal npm source selection across
npmmirror and npmjs. This is distinct from the `[registries.npm]` preflight used
before a managed native npm or pnpm process starts. Project delegates retain the
documented explicit registry precedence. Global delegates run with isolated
configuration inside the osdk-controlled prefix and currently reject native
private/authenticated/scoped registry pass-through; configure an anonymously
reachable `[registries.npm]` endpoint for that scope.

All npm-backed tools—across project, global, package, version, and scope—share
these osdk-owned paths:

```text
$OSDK_CACHE_DIR/npm/v1/cache
$OSDK_STORE_DIR/npm
```

The shared layout avoids redownloading the same package content while each real
project or controlled global install retains its own native lock.

## What `osdk.lock` guarantees

Project-aware `use` writes a compact npm metadata entry in the current
lock-schema-4 `osdk.lock`, containing the
package, resolved version, concrete installer, scope, exact Node version, and
the public options plus the native lock's kind, format, and SHA-256. The user lock
for global tools uses the same metadata-only model; npm global simply has no
native-lock identity.

::: warning Graph limitation
The metadata in `osdk.lock` does **not** itself capture or reconstruct the
transitive npm dependency graph. The installer's native lock remains the source
of that graph: the real project's `package-lock.json`, `npm-shrinkwrap.json`, or
`pnpm-lock.yaml`, or the pnpm lock retained in a controlled global install
directory. An npm global install has no dependency
lock, so its transitive selection is not reproducible from the user
`osdk.lock` alone.
:::

Commit `package.json`, the native project lock, `osdk.toml`, and `osdk.lock` for
a project workflow. Legacy lock-schema-2 graph sidecars remain readable for
compatibility, but current lock-schema-4 writes do not create a new sidecar or
embed its payload.
This lock schema is independent of `.osdk-install.json` schema 1: the install
identity selects local storage and lifecycle operations, while `osdk.lock` schema
4 records the schema-3-compatible npm options and replay metadata. `.osdk-tool.json` is legacy detection
metadata only.

For installer planning, metadata validation, native-prefix isolation, and the
activation safety checks, see [npm tool implementation](./implementation/npm-tools).
