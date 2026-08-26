# Projects and Configuration

osdk combines user defaults, project declarations, environment variables, and
CLI overrides into the effective configuration. This page documents discovery,
the editable schema, exact merge granularity, and the trust boundary.

## Configuration locations

- User configuration: `$OSDK_CONFIG_DIR/config.toml`; on Linux the default is `~/.config/osdk/config.toml`.
- Project configuration: `osdk.toml` or `.osdk.toml`. osdk walks upward and uses the nearest file; `osdk.toml` wins when both exist in one directory.
- `.tool-versions`: discovered independently by walking upward; it fills tool keys still absent after project/user `[tools]` are merged.

```text
osdk config path
osdk config list
```

`config path` prints the config directory, user file, and discovered project
file. `config list` prints selected resolved settings, not every raw TOML field;
for example it omits `yes`, `lang`, and full source details.

## Project version discovery

Active-version sources use this type-first priority. Within each type, osdk
walks from the current directory toward ancestors:

1. `[tools]` in `osdk.toml` / `.osdk.toml`;
2. `.tool-versions`;
3. native ecosystem version files;
4. Node metadata in `package.json`;
5. user-global `[tools]`.

Therefore, an ancestor `osdk.toml` still outranks a nearer `.nvmrc`.

| Backend | Native files |
| --- | --- |
| Node.js | `.nvmrc`, `.node-version`, then `package.json#engines.node` and `devEngines.runtime` |
| Python | `.python-version` |
| Java | `.java-version`, `.sdkmanrc` |
| Go | the `go` directive in `go.mod`, `.go-version` |
| Rust | `rust-toolchain.toml`, `rust-toolchain` |
| Maven / Gradle / Kotlin | `.mvn-version`, `.gradle-version`, `.kotlin-version` |
| Bun / Deno | `.bun-version`, `.dvmrc` |

Single-value files use the first non-empty, non-comment line and strip a leading
`v`. Rust TOML reads `[toolchain].channel`. Node npm semver ranges resolve to the
highest matching stable version; an invalid range fails explicitly.

::: warning Current no-argument lifecycle boundary
`current`, shims, and shell hooks read all native files above. No-argument
`lock`, `upgrade`, and `install` when no usable lock exists currently enumerate
merged `[tools]`/`.tool-versions`, then additionally discover `packageManager`
and Node. A lone `.python-version`, `.java-version`, `go.mod`, or
`rust-toolchain.toml` does not automatically add that non-Node tool to those
lifecycle commands. Put it in `[tools]` or pass it explicitly.
:::

## Minimal project configuration

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
pnpm = "10.15.0"
"npm:prettier" = "3"

[aliases.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` updates the nearest project file or creates `osdk.toml` in
the current directory. `osdk use --global node@20` updates user configuration.
For `npm:<package>`, local `use` first looks for the nearest `package.json`; the
next section describes its project-aware behavior.

## npm tools in a real project

From any directory below a Node project, this command adds the package to the
project rooted at the nearest `package.json`:

```bash
osdk use npm:prettier@3
```

The package stays in its existing `dependencies`, `devDependencies`,
`optionalDependencies`, or `peerDependencies` section; a new package defaults
to `devDependencies`. osdk uses Aube when the incumbent lock format is
compatible, or accepts `-o installer=aube|npm|pnpm` for an explicit choice. It
does not retry a failed operation through another installer. With no
`package.json` in the ancestor chain, the command keeps the legacy isolated
osdk-managed install and shim behavior.

Project-aware use updates the native `package.json` and package-manager lock,
then writes an exact Node selection and structured npm entry to `osdk.toml`. It
also writes a compact `osdk.lock` entry with installer, scope, Node, and native-
lock identity. That metadata does not contain the transitive dependency graph;
the native package-manager lock remains its source. Keep all four project files
together. See [npm Developer Tools](./npm-tools) for installer and activation
details.

## Complete configuration reference

The following example covers the current editable schema. Values in `[tools]`
may be version strings or structured objects with backend options; quote keys
containing `:`, `@`, or `/`. Missing fields use the defaults of the file layer
being deserialized; the next section explains why that is not always inheritance
from the lower layer.

```toml
[settings]
link_mode = "auto"          # auto|hardlink|reflink|copy|symlink
jobs = 8                    # default min(available parallelism, 8), fallback 4
yes = false
verify_signatures = true
require_checksums = false
attestations = "off"        # off|if-available|required
offline = false
lang = "en"                 # optional; en|zh
prerelease = "if-explicit"  # never|if-explicit|allow

[settings.node]
corepack = false

[settings.python]
catalog_url = "/approved/python-catalog.json" # HTTP(S) or local path; optional
catalog_sha256 = "0123456789abcdef..."         # required with catalog_url

[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages" # optional

[sources]
selection = "auto"          # auto|pinned|ordered
probe_timeout_ms = 1500
cache_ttl = "6h"            # s/sec/secs, m/min/mins, h/hr/hrs, d/day/days

[sources.node]
pin = "official"            # optional
disable = ["tuna"]          # optional
env = false                 # meaningful for global model-provider adapters
env_force = false

[[sources.node.custom]]
id = "corp"
kind = "custom"             # official|mirror|custom
download_url = "https://mirror.example/sdk/"
index_url = "https://mirror.example/sdk/index.json" # optional
headers = [["Header-Name", "value"]]              # optional
forward_credentials = false
priority = 0
enabled = true

[registries.npm]
urls = [
  "https://registry.npmmirror.com/",
  "https://registry.npmjs.org/",
]
probe_timeout_ms = 1500

[tools]
node = "20"
python = "3.12"
pnpm = "10.15.0"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
installer = "aube"            # auto|aube|npm|pnpm; auto is the implicit default
allow_builds = ["@scope/native-tool", "esbuild"]

[aliases.node]
default = "20"
```

Registry URLs are deduplicated and normalized with a trailing `/`. Only HTTP(S)
URLs with a host are accepted; credentials, query strings, and fragments are
rejected. See [Sources and Supply-chain Security](./sources-security) and
[JavaScript Package Managers](./package-managers) for runtime selection.
Structured tool objects require `version`; other options may be strings,
booleans, or string arrays. Arrays become comma-separated values when passed to
the backend. `installer` selects the npm tool installer. `allow_builds` controls
isolated and global installs; project-aware `use` always disables lifecycle
scripts. See [npm Developer Tools](./npm-tools#build-script-policy) for the
complete security boundary.

## Exact override and merge semantics

The overall precedence is:

```text
CLI > OSDK_* environment > nearest project config > user config > built-in defaults
```

File layers do not use one universal field-by-field merge:

| Area | Effect of the higher-precedence file |
| --- | --- |
| `[settings]` | **Replace the whole section**; omitted fields become built-in `Settings` defaults instead of inheriting the user file |
| `[sources]` top level | Replace `selection`, `probe_timeout_ms`, and `cache_ttl` as a group; omissions become defaults |
| `[sources.<tool>]` | Merge by tool key; the higher layer replaces the entire same-tool value (`pin`, `disable`, `custom`, and so on) |
| Model `env` / `env_force` | Project files cannot change them; user-global values are retained to prevent a repository from silently changing credential environment |
| `[registries]` | Replace the whole section; project npm URLs are not combined with user URLs |
| `[tools]` | Merge by backend key; higher same-name key wins |
| `[aliases.<tool>]` | Merge by tool and alias key; higher same-name alias wins |
| `.tool-versions` | Fill only tool keys still missing after `[tools]` is merged |

For example, if user configuration sets `verify_signatures = false` and a
project contains only:

```toml
[settings]
jobs = 2
```

the project creates a new default `Settings` value and changes `jobs`, so
`verify_signatures` returns to its built-in default `true`. Declare the full
non-default combination in the higher layer when you need to preserve it.

## Environment overrides

| Variable | Configuration field |
| --- | --- |
| `OSDK_LINK_MODE` | `settings.link_mode` |
| `OSDK_JOBS` | `settings.jobs` |
| `OSDK_YES` | `settings.yes` |
| `OSDK_VERIFY_SIGNATURES` | `settings.verify_signatures` |
| `OSDK_REQUIRE_CHECKSUMS` | `settings.require_checksums` |
| `OSDK_ATTESTATIONS` | `settings.attestations` |
| `OSDK_OFFLINE` | `settings.offline` |
| `OSDK_PRERELEASE` | `settings.prerelease` |
| `OSDK_PYTHON_CATALOG_URL` | `settings.python.catalog_url` |
| `OSDK_PYTHON_CATALOG_SHA256` | `settings.python.catalog_sha256` |
| `OSDK_JAVA_CATALOG_URL` | `settings.java.catalog_url` |
| `OSDK_SELECTION` | `sources.selection`; an unknown value currently falls back to `auto` |
| `OSDK_LANG` | Output language, ahead of configuration and locale |

Directory variables are listed under [Directory layout and overrides](./storage-shell#directory-layout-and-overrides).

## Project configuration trust

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` may name a config file or directory; a directory triggers upward project
discovery. Project files containing exclusively top-level `[tools]` and
`[aliases]` are normally trust-free, but an npm tool entry requires trust
because shell activation may expose project `node_modules/.bin`. Any other
top-level section—including `settings`, `sources`, `registries`, or an unknown
section—also requires trust. A successful project-aware `osdk use npm:...`
trusts the exact `osdk.toml` it generated; later edits invalidate that record.

```bash
osdk --yes trust                 # nearest project configuration
osdk --yes trust ./osdk.toml     # explicit file
osdk trust list                  # active and stale records
osdk untrust                     # revoke the nearest project configuration
```

Persistent identity binds the canonical path and BLAKE3 of normalized TOML
content. Editing or moving the file makes the record stale; whitespace-only
formatting normally does not. Symlinks resolve to their real target. Records
live in `$OSDK_CONFIG_DIR/trusted-configs.toml`; `trust list` reports stale
entries but does not remove them.

CI may set `OSDK_TRUSTED_CONFIG_PATHS` to an OS path-list of reviewed files or
directories. Matching project files are trusted for that process without being
written to the local store. `trust` and `untrust` themselves load only user
configuration, so an untrusted project cannot influence its own approval.
