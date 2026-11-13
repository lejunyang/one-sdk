# Node.js

Configuration reference: [applicable tool_order sources](../config/tool-selection#node), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#node).

```bash
osdk install node@20
osdk install node@20.19.0 -o corepack=true
osdk lock node@20 -o arch=arm64
```

| Option | Values | Effect |
| --- | --- | --- |
| `arch` | `x64`, `arm64`, `x86`, `arm` | Select the Node artifact architecture; default is the host |
| `corepack` | `true|1|yes|on` or `false|0|no|off` | Run the target Node's `corepack enable --install-directory <node-bin>` |

Without an explicit `corepack` option, `[settings.node].corepack` applies and
defaults to `false`. Failure removes the incomplete installation. The Node shim
owns `node` and `corepack`; independent npm or the matching Node installation
provides coordinated npm/npx routing. The runtime environment supplies shared
`npm_config_cache` only when the user has not set it.

Bundled and independently managed npm default to `data/global/npm`, including
direct invocation without shell activation. Installing or reusing a version sets
this default; run `osdk reshim` after upgrading osdk to update existing installs.
This does not move old packages. User `.npmrc`, environment and `--prefix` retain
npm’s native precedence; shim execution additionally injects the managed
`npm_config_prefix`. After running original npm directly, use `osdk reshim` to
publish new commands.

`arch` can generate another architecture's lock section, but osdk has no
download-only cross-architecture mode; actual installation rejects a Node
artifact that cannot execute on the host.

Node merges version indexes from every reachable source while retaining the
higher-ranked source's LTS metadata for duplicates. A responsive but stale
mirror therefore cannot hide a release present on a later source; the archive
download then verifies each `SHASUMS256.txt` candidate in the same order.

## Move legacy Node-global packages into the shared prefix

`osdk doctor` lists legacy packages inside each Node version, their exact versions
and paths, and duplicates in the shared prefix. Preview first, then explicitly
select the source package version to retain:

```bash
osdk node migrate-packages --from 24.18.1 --to-global
osdk node migrate-packages --from 24.18.1 --to-global --package agentbuddy
osdk node migrate-packages --from 24.18.1 --to-global --package agentbuddy --apply
```

Choose either `--to-global` or `--to`. Repeat `--package NAME` to migrate only
selected source packages. A differing shared version requires explicit
`--package` selection; osdk never chooses by version order. Without `--apply`,
only package metadata is read and no npm process runs. npm/Corepack themselves
and packages with install scripts or gyp markers are excluded; reinstall those
separately after checking runtime compatibility.

Apply reinstalls exact versions with scripts disabled, requiring an available
registry and suitable npm login configuration. Do not run concurrent global
installs. Failed installation or artifact verification leaves both live copies
intact. Success removes the corresponding source packages and rebuilds shims;
copies in other Node versions need separate migration. Shared packages linked
outside the prefix require manual handling. If source cleanup fails, the shared
installation remains and the previous shared prefix backup location is reported.

Node also supports global-package migration:

```text
osdk node migrate-packages --from VERSION --to VERSION [--apply]
```

```bash
# Plan only
osdk node migrate-packages --from 20.19.0 --to 22.17.0

# Apply the plan
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

Both versions must be installed, managed Node versions that contain npm. osdk
enumerates the source with `npm ls -g --depth=0 --json --long`, skips npm itself,
packages already present on the target, and packages declaring
`hasInstallScript=true` or `gypfile=true`. `--apply` installs exact versions; on
failure, it restores the target's original global-package set.
