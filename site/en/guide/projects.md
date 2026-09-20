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
osdk config get KEY [-g]
osdk config set KEY VALUE [-g]
osdk config unset KEY [-g]
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
`link_mode`, `lang`, `shims.include`, `shims.exclude`, `shims.expose`, and the
per-tool `shims.<tool>.{include,exclude,expose}`. Lists take a comma-separated
value. Tool pins, source pins and aliases are not included; `osdk use`,
`osdk source pin` and `osdk alias` own those.

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
If `config set` writes a [governed key](#which-keys-require-trust) such as
`verify_signatures`, that file starts requiring trust; the command offers to trust
it on the spot, and `--yes` accepts. In a non-interactive session the write still
succeeds but trust is withheld, and the output says what is still needed. Writing a
safe key such as `jobs` or `lang` prompts for nothing.
:::

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
rust = "1.91.1"
pnpm = "10.15.0"
"npm:prettier" = "3"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
"go:golang.org/x/tools/gopls" = { version = "0.20", tags = ["netgo"] }

[aliases.node]
maintenance = "20"
default = "maintenance"
```

`osdk use node@20` updates the nearest project file or creates `osdk.toml` in
the current directory. `osdk use --global node@20` updates user configuration.
For `npm:<package>`, local `use` first looks for the nearest `package.json`; the
next section describes its project-aware behavior.
Every `cargo:<crate-or-https-url>` entry requires one exact explicit or configured
`rust` entry. Floating and linked Rust toolchains are rejected; see
[Cargo Developer Tools](./cargo-tools).
Every `go:<module-or-command-path>` entry likewise requires a managed `go`
selection. osdk resolves that selection first and binds its exact version; see
[Go Developer Tools](./go-tools).

## npm tools in a real project

From any directory below a Node project, this command adds the package to the
project rooted at the nearest `package.json`:

```bash
osdk use npm:prettier@3
```

The package stays in its existing `dependencies`, `devDependencies`,
`optionalDependencies`, or `peerDependencies` section; a new package defaults
to `devDependencies`. osdk follows the project's declared `packageManager`, then
the installer that owns the incumbent lock, and otherwise the configured
default; `-o installer=npm|pnpm` forces an explicit choice. It does not retry a
failed operation through another installer. With no
`package.json` in the ancestor chain, the command keeps the legacy isolated
osdk-managed install and shim behavior.

Project-aware use updates the native `package.json` and package-manager lock,
then writes an exact Node selection and structured npm entry to `osdk.toml`. It
also writes a compact `osdk.lock` entry with installer, scope, Node, and native-
lock identity. That metadata does not contain the transitive dependency graph;
the native package-manager lock remains its source. Keep all four project files
together. See [npm Developer Tools](./npm-tools) for installer and activation
details.

For shell activation, `use` also generates local derived state under
`.osdk/npm-bin/`. Only curated launchers for the configured npm tools are
activated; the whole `node_modules/.bin` directory is never added to PATH. Add
`/.osdk/npm-bin/` to an ignore file at the package root, or
`**/.osdk/npm-bin/` at a repository root that contains nested packages, while
continuing to commit the four source-of-truth files above.

## Choosing which commands reach PATH

An install often brings more executables than you asked for: a conda prefix holds
an entire dependency closure, and an Android NDK ships 172 of them.
`[settings.shims]` decides which ones get a shim and reach PATH.

Withholding a shim is not the same as not installing: the file stays in the
install directory and remains available through `osdk exec` and an activated
shell.

### The three lists

| Setting | Meaning | Scope |
| --- | --- | --- |
| `include` | **Allowlist**: when non-empty, anything not listed is withheld | every tool |
| `expose` | **Additive**: also shim these, without affecting any other tool | every tool |
| `exclude` | Skipped, applied last, trims the result of the other two | every tool |

Order: `include` picks the candidate set → `expose` adds → `exclude` removes.

`expose` wins over a narrow `include` elsewhere. When both match a name, one says
"shim this" and the other says "not on the list"; letting `include` win would mean
silently dropping an explicit request.

### `include` is a global allowlist, not "add one back"

This is the easiest mistake to make:

```bash
# Dangerous: meant to recover make, loses the shims for cargo / go / node
osdk config set shims.include "conda:m2-base:make"
```

Once `include` is non-empty it becomes an allowlist over **every** tool, so naming
one command declares "and nothing else". In practice that single command took 646
shims down to zero.

To recover a withheld command, use `expose`:

```bash
# Safe: only ever adds
osdk config set shims.expose "conda:m2-base:make"
```

Nothing is wrong with `include` itself — it means "only these", which is the right
thing when you genuinely want to narrow down. Prefer the per-tool form below.

### Per-tool overrides

All three lists can be scoped to a single tool, keyed `shims.<tool>.<field>`:

```bash
osdk config set shims.conda:m2-base.expose  "make,sh,bash,tr,awk"
osdk config set shims.android-ndk.include   "clang,clang++,llvm-strip"
osdk config set shims.conda:m2-base.exclude "ls,test"

osdk config get   shims.conda:m2-base.expose
osdk config unset shims.conda:m2-base.expose
```

In TOML:

```toml
[settings.shims.tools."conda:m2-base"]
expose = ["make", "sh", "bash", "tr", "awk"]
```

Scoping is what makes `include` safe: an `include` under `android-ndk` can only
affect the NDK's own commands and cannot take `cargo` away. **When narrowing one
tool, prefer this form over the global `include`.**

Overrides are **per field**: a field you set replaces the global one, a field you
leave out inherits it. Adjusting only `expose` therefore does not clear the global
`exclude`. `config get` prints `inherit` for an unset field, so it is
distinguishable from one explicitly set to an empty list.

Tool keys match a backend id **exactly** — no globs. Use the global lists for
patterns that span tools.

### Pattern syntax

Every element of the three lists is a pattern, with the same rules:

| Form | Matched against | Example |
| --- | --- | --- |
| no `:` | the command name | `make`, `clang*` |
| contains `:` | `<backend>:<command>` | `conda:m2-base:make`, `android-ndk:*` |

- `*` matches any run of characters, `?` matches exactly one;
- matching is case-insensitive (Windows executables are too);
- patterns match the whole name, not a substring.

Both forms work inside a per-tool list, but since the scope is already fixed, a
bare command name reads better there.

### Metapackages: no shims at all by default

A metapackage such as `conda:m2-base` installs no commands of its own — everything
in the prefix belongs to the packages it pulls in — so nothing is shimmed by
default. That is deliberate: otherwise the msys `ls`, `test` and `sort` would
shadow their Windows namesakes.

Name the ones you need with `expose`:

```bash
osdk config set shims.conda:m2-base.expose "make,sh,bash,tr,awk,grep,printf"
osdk reshim
```

Exposing only what you actually use keeps the rest from interfering with system
commands.

### Reshim after changing these

`config set` edits configuration only; it does not touch the shims on disk. Follow
it with:

```bash
osdk reshim
```

`osdk where --bins <tool>` verifies the outcome, listing `published` (shimmed) and
`withheld` separately.

::: tip These settings do not need trust
`settings.shims` only decides which commands get a shim. It neither executes code
nor changes where downloads come from, so writing it into a project `osdk.toml`
never asks for trust. See
[Which keys require trust](#which-keys-require-trust) for the ones that do.
:::

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

[containers]
runtime = "auto"              # auto|docker|containerd
builder = "auto"              # auto or a validated Buildx builder name
platform = "runtime"          # runtime or OS/ARCH[/VARIANT]
probe_timeout_ms = 1500

[tools]
node = "20"
python = "3.12"
pnpm = "10.15.0"
"npm:prettier" = "3"

[tools."npm:@scope/native-tool"]
version = "1.2.3"
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
The `http:` entry requires an exact semantic version and a SHA-256 for its
strict HTTPS `{version}` template; see [Direct HTTPS Artifacts](./http-artifacts)
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
| `[containers]` | Replace the whole section; omitted runtime, builder, platform, timeout, and registry-policy fields use built-in defaults |
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
| `OSDK_CONTAINER_RUNTIME` | `containers.runtime`; `auto|docker|containerd` |
| `OSDK_CONTAINER_BUILDER` | `containers.builder`; `auto` or a validated Buildx builder name |
| `OSDK_CONTAINER_PLATFORM` | `containers.platform`; `runtime` or `OS/ARCH[/VARIANT]` |
| `OSDK_LANG` | Output language, ahead of configuration and locale |

Directory variables are listed under [Directory layout and overrides](./storage-shell#directory-layout-and-overrides).

## Project configuration trust

```text
osdk [--yes] trust [PATH]
osdk trust list
osdk untrust [PATH]
```

`PATH` may name a config file or directory; a directory triggers upward project
discovery.

### Which keys require trust

The gate is evaluated **per key**, and only two things qualify: running
**arbitrary code** on this machine, or **weakening verification** of what gets
installed and where it is fetched from.

| Requires trust | Why |
| --- | --- |
| `[syspkg]` | Installs machine-wide, may prompt for elevation, and is not covered by `osdk.lock` |
| `[sources]`, `[registries]` | Change where subprocesses download from |
| `settings.verify_signatures`, `settings.require_checksums`, `settings.attestations` | Disable or downgrade artifact verification |
| `settings.python`, `settings.java` | Both carry `catalog_url`, which decides which runtime bytes get installed |
| `allow_builds`, when explicitly enabled in `[tools]` | The only switch that lets npm lifecycle scripts run |

**Declaring which tools or packages to install never requires trust on its own.**
That covers `[tools]` and `[aliases]`, including their `npm:`, `github:`, `http:`,
`go:`, `cargo:`, `pypi:` and `conda:` entries: npm installs pass
`--ignore-scripts` by default, an `http:` artifact without a `sha256` is refused
outright, and `go:` builds run with `CGO_ENABLED=0`. This is the same act as
adding a line to `package.json` -- adding a package or changing a version never
asks for re-approval.

`settings.node` (a single `corepack` bool) and `settings.npm` (a single
`default_installer` choice between npm and pnpm, and only as the lowest-priority
fallback) do not require trust either. `corepack enable` runs the corepack shipped
inside that Node install and only writes shims into the install directory -- the
bytes arrived with Node itself. Corepack does download a package manager later, but
that happens at run time, triggered by `packageManager` in `package.json`, which
trust has never governed; gating the bool would not prevent it.

Two things fail closed: an **unknown top-level section** and an **unregistered
`settings` key** both require trust. A key this build cannot interpret is not
cleared just because it is unrecognized.

A refusal lists each offending key and its reason, rather than only reporting
that the file is untrusted:

```text
error: project config is not trusted: /path/to/osdk.toml
these keys need review because they affect what runs on this machine:
  settings.verify_signatures -- weakens verification of installed artifacts, or redirects where they are downloaded from
  syspkg -- can run arbitrary code on this machine during install
```

### What the gate covers: commands that act

Trust exists to stop an unreviewed config from *doing* something, so a command
that does nothing has nothing to gate. These keep working while a project is
untrusted:

- **Read-only inspection**: `list`, `current`, `where`, `doctor`, `completions`,
  and `task list` / `task info` / `task deps`. They report state and reach no
  install, download or subprocess. They are also exactly what you run *while
  deciding* whether to trust a project -- refusing them hides both the evidence
  and the way out.
- **Trust management itself**: `trust`, `untrust`.
- **`config set` / `config unset`**: the way an untrusted config is edited back
  into shape. Gating them would block the only exit with the very config being
  undone. Each addresses one named key in one named file and never acts on what
  the untrusted config asks for. `config get` and `config list` stay gated
  because they *do* report that config's merged values.

**Tools dispatched through the shim are a separate line.** `cargo`, `node` and
the rest are started by the shim, which gates only the keys it can act on itself
-- `sources`, `registries` and the like, which decide where a subprocess it
starts will fetch from. A table the shim never reads, such as `[syspkg]` or
`[task_config]`, does not stop you from using tools in that directory. The cost
of doing otherwise is the whole directory becoming unusable, and since trust is
bound to the file's hash, every later edit of `osdk.toml` would lock it again.

Everything else stays gated. That is the fail-closed direction: a new command is
gated until someone deliberately exempts it, rather than slipping through
because it was forgotten.

### Trust identity and record states

```bash
osdk --yes trust                 # nearest project configuration
osdk --yes trust ./osdk.toml     # explicit file
osdk trust list                  # records and their state
osdk untrust                     # revoke the nearest project configuration
osdk trust prune --dry-run       # preview which dead records would go
osdk trust prune                 # drop records whose config file is gone
```

`trust list` reports four states, and the right response differs for each:

| State | Meaning | What to do |
| --- | --- | --- |
| `active` | File present, governed keys match the approval | nothing |
| `changed` | File present, governed keys differ | review, then `osdk trust` again |
| `missing` | File gone, its directory still readable | `osdk trust prune` can drop it |
| `unreachable` | Its directory is unreadable too | check whether the volume is mounted; **never** pruned |

`prune` removes only `missing`, deliberately. `changed` means the project is still
there and merely needs another look, so dropping its record would resurface later
as an unexplained "untrusted". And on Windows `unreachable` is exactly what a
detached USB disk, network share or WSL mount looks like, so treating it as garbage
would revoke valid approvals whenever a volume happened to be unplugged.
`--dry-run` uses the same candidate list as the real run, so the preview is what
will actually happen.

Persistent identity binds the canonical path and the BLAKE3 of the normalized
TOML content **of the governed keys listed above**. Because the hash covers only
those keys, both gates read the same judgement: a change that needed no trust
also cannot invalidate an existing record. Bumping a tool version, adding a
dependency, changing `jobs`, adding a comment and reordering keys all leave the
record intact; editing a governed key makes it `changed`, and moving the
repository makes it `missing` or `unreachable`. Symlinks resolve to their real
target. Records live in `$OSDK_CONFIG_DIR/trusted-configs.toml`.

CI may set `OSDK_TRUSTED_CONFIG_PATHS` to an OS path-list of reviewed files or
directories. Matching project files are trusted for that process without being
written to the local store. `trust` and `untrust` themselves load only user
configuration, so an untrusted project cannot influence its own approval.

`osdk config set` and `osdk config unset` also run before the trust check.
Otherwise the exit would be blocked by the very config being undone -- `unset`
could not remove the key causing the refusal. Both address one named key in one
named file and never act on what the untrusted config asks for. `config get` and
`config list` stay behind the check, because they do report its merged values.
