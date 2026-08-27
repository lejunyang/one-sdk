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
"npm:prettier" = { version = "3", installer = "aube" }
```

The exact managed Node version and concrete installer are recorded together. An
existing Node tool entry keeps its other options. osdk automatically trusts the
exact generated `osdk.toml` content because activation can expose project code;
editing that file changes its trust identity and requires review and
`osdk trust` again.

## Installer selection

Before changing the project, osdk reads the nearest `package.json` and the
recognized native locks beside it. With no native lock, or with exactly one
lock whose format Aube can read, automatic selection chooses Aube. Current
compatible formats are Aube v9, pnpm v9, and npm `package-lock.json` /
`npm-shrinkwrap.json` v2 or v3. When the one existing npm or pnpm lock is too
new or otherwise unsupported by Aube, osdk delegates once to the native manager
that owns it.

`package.json#packageManager` (then `devEngines.packageManager`) identifies the
declared owner. A declaration and the one existing native lock must agree; two
or more recognized lockfiles are rejected as ambiguous before anything is
changed. Automatic mode accepts declared Aube, npm, or pnpm; another manager
requires an explicit supported installer choice.

Choose an installer explicitly when required:

```bash
osdk use npm:prettier@3 -o installer=aube
osdk use npm:prettier@3 -o installer=npm
osdk use npm:prettier@3 -o installer=pnpm
```

An explicit choice overrides the package-manager declaration, but it still must
be compatible with the incumbent lock: Aube will not consume an unsupported
format, and npm or pnpm will not overwrite the other native manager's lock. The
installer is selected before mutation and is invoked at most once. A failed
Aube/npm/pnpm operation is returned directly; osdk never replays it through a
different installer.

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
# Aube is the default global installer.
osdk use --global npm:prettier@3

# Use a managed native package manager explicitly.
osdk use -g npm:eslint@9 -o installer=npm
osdk use -g 'npm:@antfu/ni@0.21.12' -o installer=pnpm
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

osdk installs the selected Node and, when requested, npm or pnpm. Each installer
then runs its real global-add operation in an osdk-controlled prefix: npm uses
`install --global --prefix ...`, pnpm uses `add --global`, and Aube uses
`add --global` through the packaged sibling `osdk-aube` helper. The helper gives
Aube an isolated home and prefix while reusing osdk's shared Aube store and
cache. osdk adapts the resulting native global layout, validates the selected
package and its declared commands, and publishes only those commands through
osdk shims. None of the three modes writes to the ambient Node installation or
the current project.

The selected version and installer are written to the user configuration, and
basic package, scope, Node, installer, and optional native-lock identity are
written to `$OSDK_CONFIG_DIR/osdk.lock`. npm global installs do not produce a
dependency lock. pnpm's `pnpm-lock.yaml` and Aube's `aube-lock.yaml` remain in
their controlled install directories.

::: warning Aube global offline support
Aube 2.1 cannot create or repair a global installation in osdk's offline mode.
An already complete exact installation can be selected again offline only when
its installer and build-policy options also match. Select npm or pnpm when the
installation itself must use their native global offline mode. The shared Aube
store and cache still avoid duplicate downloads during supported online installs.
:::

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
retains its isolated-first compatibility behavior. Plain `uninstall` removes
only the isolated installation; both commands require `--global` to target the
user-wide installation. A package added to a real project remains owned by that
project and its package manager. Activated commands come from the curated
generation, whose launchers target the configured package's validated declared
files under `node_modules/<package>`.
`osdk list-remote npm:prettier [FILTER]` lists stable registry versions.

## Build-script policy

Local project `use` always disables lifecycle scripts, for Aube as well as the
native npm and pnpm delegates. For isolated and global installs, scripts are
also disabled by default. Reviewed packages can opt in with a structured tool
entry or a one-shot option:

```toml
[tools."npm:@scope/native-tool"]
version = "1.2.3"
installer = "aube"
allow_builds = ["@scope/native-tool", "esbuild"]
```

| Configuration | Isolated/global effect |
| --- | --- |
| Omitted or `false` | Deny all dependency build scripts |
| `["pkg-a", "pkg-b"]` | Allow only named packages where the selected installer supports an allowlist |
| `true` | Allow scripts throughout the graph; use only after full review |

The one-shot form is `-o allow_builds=esbuild,sharp`. Native npm cannot enforce
a package allowlist and accepts only false or true; Aube and pnpm support the
named-package form.

## Option changes and reinstallation

For osdk-owned isolated and global installs, `installer` and `allow_builds` are
part of the installation identity, not hints that may be ignored after a version
match. Changing either option at the same package version prevents reuse of the
existing installation. Activation and shims also refuse to run an install whose
recorded options differ from the active configuration.

Installs created before option identity was recorded remain discoverable, but
they cannot be reused or executed. Re-run the same `install` or global `use`; the
npm workflow rebuilds or replaces that version and records its current identity.
An offline rebuild still needs the native lock or graph and warmed cache/store
that the selected installer normally requires. Do not edit `.osdk-tool.json` by
hand.

Physical install directories are still keyed by package, exact version, and
isolated/global scope rather than by options. Two option variants of the same
package version therefore cannot coexist in one scope; switching options replaces
that version's installation.

## Sources and shared storage

Dynamic npm version metadata uses the normal npm source selection across
npmmirror and npmjs. This is distinct from the `[registries.npm]` preflight used
before a managed native npm or pnpm process starts. Project delegates retain the
documented explicit registry precedence. Global delegates run with isolated
configuration inside the osdk-controlled prefix and currently reject native
private/authenticated/scoped registry pass-through; configure an anonymously
reachable `[registries.npm]` endpoint for that scope.

All Aube-backed npm tools—across project, global, package, version, and scope—
share these osdk-owned paths:

```text
$OSDK_CACHE_DIR/aube/v1/cache
$OSDK_STORE_DIR/aube
```

The shared layout avoids redownloading the same package content while each real
project or controlled global install retains its own native lock.

## What `osdk.lock` guarantees

Project-aware `use` writes a compact lock-schema-3 `osdk.lock` entry containing the
package, resolved version, concrete installer, scope, exact Node version, and
the public options plus the native lock's kind, format, and SHA-256. The user lock
for global tools uses the same metadata-only model; npm global simply has no
native-lock identity.

::: warning Graph limitation
The metadata in `osdk.lock` does **not** itself capture or reconstruct the
transitive npm dependency graph. The installer's native lock remains the source
of that graph: the real project's `aube-lock.yaml`, `package-lock.json`,
`npm-shrinkwrap.json`, or `pnpm-lock.yaml`, or the Aube/pnpm lock retained in a
controlled global install directory. An npm global install has no dependency
lock, so its transitive selection is not reproducible from the user
`osdk.lock` alone.
:::

Commit `package.json`, the native project lock, `osdk.toml`, and `osdk.lock` for
a project workflow. Legacy lock-schema-2 graph sidecars remain readable for
compatibility, but current lock-schema-3 writes do not create a new sidecar or
embed its payload.
This lock schema is independent of `.osdk-tool.json` inventory schema 2: the
inventory gates local reuse, while `osdk.lock` schema 3 records options and npm
replay metadata.

For installer planning, metadata validation, native-prefix isolation, and the
activation safety checks, see [npm tool implementation](./implementation/npm-tools).
