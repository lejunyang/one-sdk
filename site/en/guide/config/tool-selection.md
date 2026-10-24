# Tool Selection

Start here for shared ordering, then use the [per-tool matrix](#tool-order-reference), [project diagnostics](#inspect-sources), and [per-tool environment reference](./tool-environment). Download mirrors and project package registries are separate configuration.

## Configurable declaration precedence

```toml
[settings.precedence]
tools = "osdk"
packages = "native"
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "error" # default: prefer, select by order
npm = "osdk"           # optional override of packages; inherit clears this override
python = "native"
```

`tool_order` is one shared source-kind order for all tools, highest first; it is not a per-tool map. Only kinds applicable to the requested tool participate. For each filename, the nearest declaration is considered; `idiomatic` also preserves the fixed backend filename order below.
Accepted kinds: `system` (opt-in), `osdk`, `global`, `.tool-versions`, `.nvmrc`, `.node-version`, `idiomatic`,
`package.json#packageManager`, `package.json#devEngines.packageManager`,
`package.json#engines.node`, `package.json#devEngines.runtime`. Omitted declaration kinds are appended in
_default_ order, not disabled. `system` is the exception: it is enabled only when explicitly listed. Unknown or duplicate entries fail. `[]` clears an inherited custom
order and disables inherited `system`. `idiomatic` groups the other native filenames: Python uses `.python-version`; Go checks `go.mod` before `.go-version`, Rust checks `rust-toolchain.toml` before `rust-toolchain`, and Java checks `.java-version` before `.sdkmanrc`. These names are not independent `tool_order` entries. Backend filename priority applies before directory proximity between those filenames.
The default order is project osdk, .tool-versions, .nvmrc, .node-version, other idiomatic files,
packageManager, devEngines.packageManager, engines.node, devEngines.runtime, global.
`tools="native"` moves project osdk after native kinds. Custom order can explicitly promote global.

Project osdk uses only the nearest configuration and explicit `extends`, not implicit parent merging.
User, extends and project precedence settings merge by field; arrays replace; omitted fields inherit.
Overridden ancestors within a kind are shadowed evidence, not simultaneous conflict constraints; inactive version values are not validated. Declarations belonging to another tool are ignored without interpreting their version or integrity values. Native manager fields choose the nearest identity across manager names; automatic manager selection rejects unsupported identities.
`osdk current` reports tool, path, field, category, winner/shadowed/ignored and reason from the same selection result.
Known structured fields, JSON fields and simple-file line numbers are reported; unknown paths remain
absent rather than guessed.

`tool_conflicts="error"` compares effective declarations across kinds: an exact version satisfying a
range is compatible; numeric prefixes, supported semver comparators and `||` use pairwise range
overlap after expanding configured aliases. This does not merge ranges into a new request: the
highest-priority declaration still selects the version. Different latest/LTS/channels or opaque selectors
that cannot be proved compatible offline produce a distinct unverifiable error, not a claimed
incompatibility or a network install. `prefer` selects by order but still validates eligible declarations. packageManager requires exact semver without URLs, hashes or build suffixes; devEngines.packageManager accepts a
single object or single-element array with a range, and rejects multiple alternatives explicitly.
Explicit `tool@version`, explicit deps provider/installer choices, and lock replay keep their authority.

### tool_order values and tool applicability {#tool-order-reference}

`tool_order` is an array of the following **11 exact strings**. It orders declaration sources and the opt-in external execution source, not tool names, executable names, download mirrors, or registries. The same array applies to every tool; a valid kind with no candidate for a tool simply has no effect on that tool. There is no `[settings.precedence.node]` or per-tool `tool_order`.

| Value | Candidate source | Built-in tools |
| --- | --- | --- |
| `system` | Existing executable from the adapter home or PATH; explicitly enabled only | Node, Python, Go, Java, npm, pnpm, Yarn, Bun, Deno, Zig; see [system tools](#system-tools) |
| `osdk` | `[tools]` in the nearest project `osdk.toml` / `.osdk.toml`, including explicit `extends` | All tools with a matching backend key |
| `global` | `[tools]` in osdk user configuration `$OSDK_CONFIG_DIR/config.toml`; locate it with `osdk config path` | All tools with a matching backend key |
| `.tool-versions` | Matching tool line in the nearest applicable `.tool-versions` | All tools with a matching backend key |
| `.nvmrc` | `.nvmrc` version declaration | Node |
| `.node-version` | `.node-version` version declaration | Node |
| `idiomatic` | Backend-declared files, in the fixed order below | Backends in the matrix below; not Node's two separately named kinds |
| `package.json#packageManager` | Exact manager version in `packageManager` | npm, pnpm, Yarn |
| `package.json#devEngines.packageManager` | Manager version/range in `devEngines.packageManager` | npm, pnpm, Yarn |
| `package.json#engines.node` | Node version/range in `engines.node` | Node |
| `package.json#devEngines.runtime` | Runtime version/range for a `name="node"` entry | Node |

`global` means **osdk's user-level tool defaults**, not versions installed on `PATH` or native nvm/npm global configuration. For example, with user `config.toml` declaring `[tools] node="20"` and project `osdk.toml` declaring `[tools] node="22"`, this project policy selects the request `20`:

```toml
[settings.precedence]
tool_order = ["global", "osdk"]
tool_conflicts = "prefer"
```

Changing `tool_conflicts` to `"error"` rejects the incompatible `20` and `22` declarations instead of selecting either. Without the custom order, the default project-first policy selects `22`. Explicit CLI versions and lock replay remain authoritative.

There is no `env` source kind and no universal `OSDK_<TOOL>_VERSION` override: `OSDK_NODE_VERSION` and `OSDK_PYTHON_VERSION` are not implemented. `CLI > OSDK_* > project` applies only to implemented settings. Existing PATH/home tools use the separate opt-in [`system` execution source](#system-tools). Without that opt-in, a shim with no selected version still errors.

Every row below also supports `osdk`, `global`, and `.tool-versions` for its backend key. Only tools explicitly marked `system` in the additional-values column support that source, and it must be enabled explicitly. A file's presence does not automatically request installation of its tool; this table describes version selection when that tool is requested or activated.

| Tool / backend key | Additional applicable `tool_order` values | Files within `idiomatic`, in priority order |
| --- | --- | --- |
| <span id="node"></span>`node` | `system`, `.nvmrc`, `.node-version`, `package.json#engines.node`, `package.json#devEngines.runtime` | None: `.nvmrc` and `.node-version` have their own kinds; moving `idiomatic` does not move them |
| <span id="python"></span>`python` | `system`, `idiomatic` | `.python-version` |
| <span id="go"></span>`go` | `system`, `idiomatic` | `go.mod` (`go` directive), then `.go-version` |
| <span id="rust"></span>`rust` | `idiomatic` | `rust-toolchain.toml`, then `rust-toolchain` |
| <span id="java"></span>`java` | `system`, `idiomatic` | `.java-version`, then `.sdkmanrc` |
| <span id="bun"></span>`bun` | `system`, `idiomatic` | `.bun-version` |
| <span id="deno"></span>`deno` | `system`, `idiomatic` | `.dvmrc` |
| <span id="zig"></span>`zig` | `system`, `idiomatic` | `.zig-version` |
| <span id="maven"></span>`maven` | `idiomatic` | `.mvn-version` |
| <span id="gradle"></span>`gradle` | `idiomatic` | `.gradle-version` |
| <span id="kotlin"></span>`kotlin` | `idiomatic` | `.kotlin-version` |
| <span id="js-managers"></span>`npm`, `pnpm`, `yarn` | `system` (opt-in), `package.json#packageManager`, `package.json#devEngines.packageManager` | None |
| Other built-in and dynamic backends | None | None |
| Declarative plugin backends | `idiomatic` if the definition declares files | The definition's `idiomatic_files` order for other filenames; reserved `.nvmrc` / `.node-version` retain their separate kinds; no new enum values |

Plain version files accept one version line (with optional comments); this is not arbitrary native-manager configuration parsing. In particular, the registered `.sdkmanrc` path currently uses the plain-version reader, not SDKMAN's multi-key `java=...` format. Rust toolchain files and the Go `go` directive have dedicated parsing.

Unknown strings and duplicate entries are errors: `"node"`, `".python-version"`, and `"go.mod"` are not valid `tool_order` values. Omitted declaration kinds are appended in the default relative order determined by `tools`; omission does not disable those sources. `system` is never appended: omission disables it. `[]` clears an inherited custom order and disables inherited `system`. For `idiomatic`, backend filename priority is fixed before directory distance, so an ancestor `go.mod` declaration can outrank a nearer `.go-version`; `tool_order` cannot reverse those two files. See the [three runnable examples](#examples-node-python-and-javascript-managers) below.

### Examples: Node, Python and JavaScript managers

These are separate project `osdk.toml` examples. Edit TOML directly for these policy fields, then use `osdk current node`, `osdk current python`, or `osdk current pnpm` to inspect the winner and shadowed/ignored evidence. An existing lock replay or explicit version request keeps its authority.

**Node — prefer `.node-version` over `.nvmrc` and `[tools]`:**

```toml
[tools]
node = "20"

[settings.precedence]
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "prefer"
```

With `.node-version` containing `22` and `.nvmrc` containing `20`, the selected request is `22`. Changing `tool_conflicts` to `"error"` rejects those incompatible declarations. This order also applies to other tools, but the two Node-only file kinds have no effect on Python or pnpm.

**Python — prefer `.python-version` while retaining native package indexes:**

```toml
[tools]
python = "3.12"

[settings.precedence]
tool_order = ["idiomatic", "osdk"]
packages = "osdk"
python = "native"
```

With `.python-version` containing `3.13`, the selected Python request is `3.13`. `idiomatic` also promotes the applicable Go/Rust/Java native files; there is no Python-only `tool_order` setting. The separate `python="native"` preserves native package-index policy even though other package ecosystems use `packages="osdk"`. Use `python="inherit"` in a child layer to restore the shared package policy; use `tool_order=[]` to clear an inherited custom version order.

**JavaScript manager — prefer a devEngines range over an exact packageManager declaration:**

```toml
[settings.precedence]
tool_order = ["package.json#devEngines.packageManager", "package.json#packageManager", "osdk"]
tool_conflicts = "error"
npm = "native"
```

With the [package.json example](../tools/package-managers#package-manager-version-discovery), pnpm selects `>=10 <11`; the exact `10.15.0` declaration is compatible, but does not pin the winning range. `npm="native"` controls package registries for the npm ecosystem, including pnpm/Yarn, not their executable versions. For an exact manager selection, leave `packageManager` first or pass an explicit version such as `osdk exec -t pnpm@10.15.0 -- pnpm --version`.

### Existing system tools {#system-tools}

To use an existing executable without installing it into osdk:

```toml
[settings.precedence]
tool_order = ["system", "osdk"]
tool_conflicts = "prefer"
```

This is one policy for all supported tools. `osdk current node` shows the absolute executable and its origin; `osdk exec -t node -- node --version` validates and runs it. `osdk reshim` publishes eligible companion shims, and activation uses the same selection. Under `prefer`, discovery/current/activation do not execute version probes; the version is honestly reported as unknown until execution. Under `error`, selection probes the external version and checks compatibility with active declarations, including ranges. Opaque selectors such as `lts` cannot be proven compatible and fail. JS manager probes use the selected Node; a required managed Node must already be installed for a strict selection check.

`system` is **off by default**. It is never appended when omitted; replacing an inherited order with `[]` disables it. Other omitted declaration kinds still append in their default order. For example, `["osdk", "system"]` prefers a project declaration, then system, then omitted native/global sources. To make system the last fallback, list all declaration kinds first:

```toml
[settings.precedence]
tool_order = [
  "osdk", ".tool-versions", ".nvmrc", ".node-version", "idiomatic",
  "package.json#packageManager", "package.json#devEngines.packageManager",
  "package.json#engines.node", "package.json#devEngines.runtime", "global", "system",
]
```

[See per-tool selectors, child variables, caches and version-probe formats](./tool-environment#per-tool-environment).


Only these adapters participate. Rust/rustup, Maven, Gradle, Kotlin, uv, dynamic package tools and plugin backends have no system adapter. `PNPM_HOME`, `NVM_DIR`, cache variables and other homes do not become selectors. Existing unrelated environment/cache policies remain intact; `override_env` is not a selection switch. Before external execution, osdk also restores its own activation-injected forced location variables (for example `PNPM_HOME`, `npm_config_prefix`, `BUN_INSTALL`) to the saved user value or absence. This restoration does not make them selection sources. For Python venv-first or JDK-first selection, set the ordinary `VIRTUAL_ENV` or `JAVA_HOME`, then use `tool_order=["system"]`; do not add new osdk version variables. For mixed managed pnpm and external Node, use `osdk exec -t pnpm@10.15.0 -- pnpm --version`: the explicit manager stays managed while the implicit Node follows the policy.

PATH search accepts absolute entries (including spaces), ignores empty/relative entries, and excludes osdk data/install/store paths, canonical aliases, known shim executable identities/copies and generated wrappers. Home selectors must be absolute; an explicitly set invalid home fails instead of silently choosing PATH. osdk activation's injected locator/PATH values are restored to their saved ambient values before discovery. On Windows only `.exe` targets are supported, case-insensitively; `.cmd`, `.bat`, PowerShell and arbitrary PATHEXT wrappers are not selected. This means a normal `pnpm.cmd` or `npm.cmd` installation needs a managed manager or a real external executable. Paths/homes used to construct this execution environment must be Unicode.

No eligible candidate allows lower declaration sources to win. Once an external candidate is chosen, a failed/unrecognized/overlong version probe stops execution; probes are limited to three seconds and 8 KiB per output stream. It never installs a replacement after a failed probe or retries after execution starts. Unsupported prerelease banners, Java 8 underscore versions and custom launchers are not claimed as verified adapters.

Explicit CLI versions and managed lock replay stay authoritative. `install`/`lock` reject an unversioned request selecting system; give an explicit managed version when creating a reproducible installation. External paths never become managed receipts or portable lock entries, and uninstall/cache cleanup do not own them. Non-frozen deps may use an unpinned external installer/runtime; a pinned installer and frozen plan remain managed. Any deps plan using external tools writes no portable tool/freshness record; with system policy enabled deps revalidates on each run. `deps --dry-run --explain` shows discovery evidence without probing and labels deferred validation. Mixing system execution with dynamic package-tool receipts is rejected; their recorded runtime contracts remain managed.

## Inspect sources in a project {#inspect-sources}

```sh
osdk current node --sources
osdk current python --sources --project ./apps/api
osdk current pnpm --sources --project ./apps/web --json
# Explicitly allow native version probes (and a selected Node runtime for JS managers):
osdk current node --sources --probe
```

`current TOOL --sources` extends the existing inspection command. It requires one tool; `--json`, `--project DIR`, and `--probe` require `--sources`. Without these flags, existing `current [TOOL]` output is unchanged: declaration evidence JSON followed by the selected request/path, not a complete source inventory. `--project` selects a directory **before** loading configuration; it does not reinterpret a config filename as a project directory.

The report separates:

- **Configured and effective order:** the inherited custom array, then the full order after defaults append. Clearing with `[]` removes inherited system selection. The effective array is shared across tools; unsupported kinds remain in that array but cannot produce a candidate for this tool.
- **Source inventory:** all 11 legal kinds with support/enabled flags and one-based rank. `present` means a declaration was recorded (possibly for another manager, hence ignored), or an eligible external path was found. `missing` means no declaration/candidate was found, even if the file exists. `disabled` means supported system selection was not enabled; `inapplicable` means this backend does not support that source. Locations separately show file existence. Missing declaration locations list searched ancestors; project osdk follows only the selected file and explicit `extends`. This is not a dump of every native file's contents.
- **Resolution evidence:** actual winner/shadowed/ignored entries, with source path, field and reason. A strict conflict changes the order winner to `candidate` and removes the selected result. Invalid declarations mark the inventory incomplete and unresolved kinds `unknown`; do not interpret them as missing.

By default the diagnostic does not execute tools, install anything, fetch catalogue metadata, or change project files. A discovered system version is unknown. Under strict policy, when system compatibility still needs checking, `resolution_status="verification-required"` and `candidate` mark the provisional order result; this is not a successful strict validation. `--probe` uses bounded native probes and performs the actual strict checks. It can execute the selected JS manager's Node runtime; a required managed Node must already be installed. Probe mode uses the configuration trust gate. It never installs missing runtimes or retries with another installer. Native executables themselves retain their normal process behavior.

`--json` emits one object with `schema_version=1`, `tool`, `project`, `configured_order`, `effective_order`, `tool_conflicts`, `inventory_complete`, `sources`, `evidence`, `selected`, `resolution_status`, `error`, and `notes`. `selected` identifies managed/system, a safe numeric selector or probed version, and an external executable path where applicable. Opaque selectors are redacted, other tools' raw values and environment values are not dumped. Known resolution/probe errors emit the report and exit nonzero; invalid TOML/policy or an unknown backend can fail before a report is built, with stderr and no JSON object. A read-only provisional report exits zero; automation must check `resolution_status` before treating it as validated.

Edit policy TOML directly: `osdk config get precedence.tool_order` is not implemented. `sources` lists SDK download mirrors, `registry` concerns package sources, and `cache env` describes cache mappings; none substitutes for `current TOOL --sources`. Explicit execution requests, installed dynamic-package receipts and lock replay are separate authority and are not simulated by this project declaration diagnostic.
