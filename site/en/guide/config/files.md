# Configuration Files

osdk combines user defaults, project declarations, environment variables, and CLI overrides into the effective configuration. This page covers where configuration files live, the `osdk config` commands, the complete editable schema, and the exact override granularity between layers. See [Environment Variables](./environment) for variables and [Project Configuration Trust](./trust) for approving project files.

## Configuration locations

- User configuration: `$OSDK_CONFIG_DIR/config.toml`; on Linux the default is `~/.config/osdk/config.toml`.
- Project configuration: `osdk.toml` or `.osdk.toml`. osdk walks upward and uses the nearest file; `osdk.toml` wins when both exist in one directory.
- `.tool-versions`: discovered independently by walking upward; it fills tool keys still absent after project/user `[tools]` are merged.

```text
osdk config path
osdk config list
osdk config get KEY [-g]
osdk config set KEY VALUE [-g]
osdk config unset KEY [-g]
osdk config migrate [--dry-run] [-g]
```

`config path` prints the config directory, user file, and discovered project
file. `config list` prints selected resolved settings, not every raw TOML field;
for example it omits `yes`, `lang`, and full source details.

`get`, `set` and `unset` act on the **project** config by default, with `-g` for
the user config, the way `git config` and `npm config` behave. `get` reports the
merged effective value, including defaults and environment overrides; `-g` reads
the user layer alone.

```bash
osdk config set jobs 8                        # writes ./osdk.toml
osdk config set -g jobs 8                     # writes the user config
osdk config get jobs                          # effective value
osdk config unset jobs                        # back to the default
```

The writable settings are the scalar and list ones: `jobs`, `offline`, `yes`,
`verify_signatures`, `require_checksums`, `attestations`, `prerelease`,
`link_mode`, `lang`, `sources.probe_timeout_ms`, `shims.include`,
`shims.exclude`, `shims.expose`, and the per-tool
`shims.<tool>.{include,exclude,expose}`. Lists take a comma-separated value. Tool
pins, source pins and aliases are not included; `osdk use`, `osdk source pin`
and `osdk alias` own those.

`sources.probe_timeout_ms` is written into the top-level `[sources]` table, in
milliseconds, and must be >= 1 (default 1500). It is the budget for **one probe
of one source**: a model-source probe spends it on both a metadata request and a
bounded range fetch, so on a slow network 1500 ms can classify a perfectly usable
local mirror as unreachable. Raise it rather than hand-editing the file:

```bash
osdk config set -g sources.probe_timeout_ms 4000   # relaxes probing only, not downloads
osdk config get sources.probe_timeout_ms
```

The Python and npm probe timeouts are the separate `probe_timeout_ms` under
`[registries.python]` / `[registries.npm]` and are not currently in the
`config set` list (the two URL lists are).

The enum vocabularies come from the settings' own types, and accepted aliases
are normalized on write (`attestations=auto` is stored as `if-available`):

| Setting | Values |
| --- | --- |
| `attestations` | `off`, `if-available`, `required` |
| `prerelease` | `never`, `if-explicit`, `allow` |
| `link_mode` | `auto`, `hardlink`, `reflink`, `copy`, `symlink` |
| `lang` | `en`, `zh` |

Values are parsed before the file is touched, so a rejected value leaves nothing
half-written, and `unset` prunes the table it empties rather than leaving a bare
`[settings]` header behind.

::: warning Writing a governed setting makes it trust-required
If `config set` writes a [governed key](./trust#which-keys-require-trust) such as
`verify_signatures`, that file starts requiring trust; the command offers to trust
it on the spot, and `--yes` accepts. In a non-interactive session the write still
succeeds but trust is withheld, and the output says what is still needed. Writing a
safe key such as `jobs` or `lang` prompts for nothing.
:::

### Section names and legacy migration

Plural top-level sections are collections of named entries: `[tools]`,
`[tasks]`, `[models]`, `[skills]`, `[sources]`, and `[registries]`. Singular
sections are one subsystem's settings or namespace: `[task]` holds shared runner
settings, `[container]` holds container settings, `[alias.tools]` holds tool
version aliases inside the alias domain, and `[sys.pkg]` is the package-manager
subsystem inside the system domain. Thus `osdk task` operates on one entry in
`[tasks]`; the collection does not become singular merely to mirror the command.

Four legacy layouts remain readable temporarily and can be migrated together:

```bash
osdk config migrate --dry-run   # preview the current project
osdk config migrate             # rewrite the current project
osdk config migrate --global    # rewrite the user config.toml
```

The moves are `[aliases]` → `[alias.tools]`, `[containers]` → `[container]`,
`[task_config]` → `[task]`, `[syspkg]` → `[sys.pkg]`, and flattening temporary
`[sys.pkg.packages]` entries into `[sys.pkg]`. The command preserves
comments and unrelated sections. If both forms of one section exist, it refuses
to guess a merge order and leaves the file unchanged. osdk's writing commands
emit only the new layout.

## Minimal project configuration

```toml
[tools]
node = "20"
python = "3.12"
go = "1.22"
rust = "1.91.1"
pnpm = "10"
"npm:prettier" = "3"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
"go:golang.org/x/tools/gopls" = { version = "0.20", tags = ["netgo"] }
"cargo:cargo-release" = { version = "0.25", lazy = true }

[alias.tools.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` updates the nearest project file or creates `osdk.toml` in
the current directory, then records the installed result and injected runtime
dependencies in the sibling `osdk.lock`. Both files form one metadata
transaction: a failed lock update restores the configuration. When the current
platform has no lock section, no-argument `install`/`lock` inherit a version only
when all compatible existing platform locks agree, then resolve the current
platform's artifact and checksum again. `osdk use
--global node@20` updates user configuration without touching the current
project lock.
For `npm:<package>`, local `use` first looks for the nearest `package.json`; the
next section describes its project-aware behavior.
Every `cargo:<crate-or-https-url>` entry requires one exact explicit or configured
`rust` entry. Floating and linked Rust toolchains are rejected; see
[Cargo Developer Tools](../tools/cargo).
Every `go:<module-or-command-path>` entry likewise requires a managed `go`
selection. osdk resolves that selection first and binds its exact version; see
[Go Developer Tools](../tools/go).

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

[settings.shims]
include = []                # global allowlist; when non-empty, nothing else is shimmed
exclude = []                # skipped names; applied last
expose = []                 # additive; only ever adds

# Per-tool overrides, keyed by backend id. Fields left out inherit the global lists.
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "tr"]

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

[container]
runtime = "auto"              # auto|docker|containerd
builder = "auto"              # auto or a validated Buildx builder name
platform = "runtime"          # runtime or OS/ARCH[/VARIANT]
probe_timeout_ms = 1500

[tools]
node = "20"
python = "3.12"
pnpm = "10"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
lazy = true                    # skipped by bare install unless --include-lazy
installer = "npm"             # auto|npm|pnpm; auto is the implicit default
allow_builds = ["@scope/native-tool", "esbuild"]

[tools."npm:only-on-windows-arm"]
[tools."npm:only-on-windows-arm"]
version = "1.0.0"
when = { os = "windows", arch = "arm64" }   # single token or a list; both must match

[tools.node]
version = "20"
arch = "arm64"                # backend option: which artifact to download
when = { os = "windows" }     # filter: where this entry applies; both coexist

[tools."http:https://downloads.example.com/acme-{version}.tar.gz"]
version = "1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
kind = "tar.gz"
strip-components = "1"
bin = "bin/acme"
rename = "acme"

[alias.tools.node]
default = "20"
```

Registry URLs are deduplicated and normalized with a trailing `/`. Only HTTP(S)
URLs with a host are accepted; credentials, query strings, and fragments are
rejected. See [Sources and Supply-chain Security](../tools/github-releases) and
[JavaScript Package Managers](../tools/package-managers) for runtime selection.
Structured tool objects require `version`; other options may be strings,
booleans, or string arrays. Arrays become comma-separated values when passed to
the backend. `lazy` is osdk metadata rather than a backend option: a bare
`install` skips the entry unless `--include-lazy` is present, while an explicit
operand still installs it. `installer` selects the npm tool installer. `allow_builds` controls
isolated and global installs; project-aware `use` always disables lifecycle
scripts. See [npm Developer Tools](../tools/npm#build-script-policy) for the
complete security boundary.
The `http:` entry requires an exact semantic version and a SHA-256 for its
strict HTTPS `{version}` template; see [Direct HTTPS Artifacts](../tools/http-artifacts)
for file/archive layout and offline replay.

`when` is a **platform filter**, not a backend option: it is stripped before
anything reaches a backend, and a non-matching entry is **absent** from the merged
configuration, so resolution, lock, shims and activation never see it. Values
within one dimension are OR, and the dimensions are AND.

::: warning Why it nests under `when` instead of plain `os` / `arch`
`os`, `arch` and `libc` are **already** backend options on `github:` and `node`,
where they select which artifact to download -- cross-architecture locking relies
on exactly that. An earlier version read a flat `arch` as the filter, so
`[tools.node] arch = "arm64"` made node vanish entirely on an x64 host, with
nothing pointing at the cause. Nesting keeps the two vocabularies apart and leaves
room for `libc` later.
:::

Only implemented dimensions are accepted inside `when`; an unsupported one such as
`libc` is an error rather than being ignored, which would quietly widen the filter
to "every libc". An unrecognized value is likewise an error, not a filter that
never matches. Naming a filtered tool explicitly fails with the restriction quoted,
and `osdk current` lists it with the reason -- a tool that is in the config yet
never appears would otherwise look like a mistake in the config.

The shorthand string form (`fd = "npm:fd@10"`) carries no filter; use the table
form above when you need one.

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
| `[container]` | Replace the whole section; omitted runtime, builder, platform, timeout, and registry-policy fields use built-in defaults |
| `[tools]` | Merge by backend key; higher same-name key wins |
| `[alias.tools.<tool>]` | Merge by tool and alias key; higher same-name alias wins |
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

`[settings.download_cache]` has `max_size_bytes` (default 2147483648; 0 retains none; `"unlimited"` disables size limit) and `max_age_secs` (default 604800; 0 disables expiry). `OSDK_DOWNLOAD_CACHE_MAX_SIZE` overrides size. See [Caches and Cleanup](../maintenance/cache).
