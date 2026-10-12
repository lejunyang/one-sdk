# Node.js

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

`arch` can generate another architecture's lock section, but osdk has no
download-only cross-architecture mode; actual installation rejects a Node
artifact that cannot execute on the host.

Node merges version indexes from every reachable source while retaining the
higher-ranked source's LTS metadata for duplicates. A responsive but stale
mirror therefore cannot hide a release present on a later source; the archive
download then verifies each `SHASUMS256.txt` candidate in the same order.

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
