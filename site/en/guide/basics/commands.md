# Install, Switch, and Inspect

These are the everyday commands: install and lock, set the current version, inspect local and remote versions, run temporarily, and define version and tool-name aliases. See the [runtime pages](../runtimes/overview) for backend options and [Reproducible Lockfiles](../project/lockfiles) for exact lock read/write rules.

## Install, lock, check, and upgrade

```text
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...] [--include-lazy]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
```

| Command | Behavior |
| --- | --- |
| `install` | Install tools and generate shims; a bare project install consumes the current-platform lock when present, otherwise resolves configuration and records an exact lock; lazy entries are skipped unless `--include-lazy` is present |
| `lock` | Resolve requests and write a platform-partitioned `osdk.lock`; do not install; floating Rust channels remain channel names |
| `outdated` | Re-resolve configuration or explicit requests and report targets not installed; never read the lock |
| `upgrade` | Re-resolve, install, and refresh the lock; never use the old lock as resolution input |

`-o/--opt` is repeatable and must be `KEY=VALUE`. A later duplicate wins. The
same option set is applied to every tool in a multi-tool command, so do not mix
backend-specific options with unrelated tools.

```bash
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk install --include-lazy
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk lock node@20 -o arch=arm64
osdk outdated node@20 python@3.12
osdk upgrade
```

Explicit `install TOOL...` is a one-shot shared installation and changes neither
project `[tools]` nor the lock. Use `osdk use TOOL@VERSION` to add a project
selection; it installs the tool and atomically updates project configuration and
the current-platform lock.

A structured `[tools]` entry with `lazy = true` stays out of a no-argument
install by default. `--include-lazy` includes all such entries. The flag is not
needed for explicit operands: naming a tool is already an explicit request to
install it. Runtime dependencies of an included tool are installed even when
their own declaration is lazy.

See [Reproducible Lockfiles](../project/lockfiles) for the exact read/write matrix.
Floating Rust channels such as `stable`, `beta`, and `nightly` are not frozen to
a concrete release. Use an explicit or dated toolchain for immutable rebuilding.

## Set the current version and uninstall

```text
osdk use|u TOOL[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk uninstall|rm TOOL@VERSION
```

`use` installs and generates shims, then writes a pin. By default it updates
the nearest project configuration, creating `osdk.toml` in the current directory
if none exists. `--global` updates user `config.toml`. An explicit prefix or
channel is preserved; a bare tool stores the exact resolved version.

`uninstall` normally expects an exact version. A prefix selects the last
string-sorted installed match; other non-exact requests are rejected. Bare
`rust` is the exception and removes `stable`. It asks for confirmation, so pass
`--yes` in automation. When a regular dynamic tool is removed, osdk also removes
any user-global pin and user lock entry that still select that version; project
configuration and project locks are unchanged. Newly unreferenced CAS objects
are collected afterward. Shell activation fails explicitly when a configured
runtime is not installed instead of silently using a same-named command from the
system PATH.

## Inspect local and remote versions

```text
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
osdk current TOOL --sources [--project DIR] [--json] [--probe]
osdk where TOOL[@VERSION]
osdk reshim
```

| Command | Exact semantics |
| --- | --- |
| `list [TOOL]` | List local versions with completion markers; without a tool, include registered backends and inventory-backed dynamic tools found on disk, including GitHub, Cargo, and Go command tools |
| `list-remote TOOL [FILTER]` | List stable remote versions; optional `FILTER` is a string prefix |
| `current [TOOL]` | Show the raw request and discovery source for the current directory; the request need not be installed or remotely resolved |
| `where TOOL[@VERSION]` | With an explicit selector (including prefixes such as `21` or build-number-less `21.0.12`), select only from installed versions using the same matching rules as install and error when nothing matches, without reading the project selection; a bare tool resolves the active version (project ecosystem files, config, dynamic shim request), falling back to the last installed entry |
| `reshim` | Regenerate shims for installed built-in and inventory-backed dynamic tools, and coordinate npm/npx routing |

`current node` and `where node` answer different questions: the former shows the
project selection (which need not be installed), while the latter prints an
installed directory — a bare tool follows the active version, and an explicit
`where node@<selector>` locates strictly among installed versions. Give a
selector when a script needs a deterministic path.

## Temporary execution

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

At least one `-t/--tool` is required, and the option is repeatable. osdk installs
the requested versions, then exposes their bins and environment to one child
process. It neither changes project pins nor reads the lock.

**Without `@`, the operand inherits the pin in effect.** `-t java` means "the Java
this project selected": osdk walks up for `osdk.toml`, `.tool-versions` and the
idiomatic version files following the
[working-directory precedence](../../internals/resolution#working-directory-precedence).
Only `-t java@<selector>` overrides it -- including an explicit `@latest`. The rule
is the same for `install`, `exec`, `lock`, `outdated` and `upgrade`, and `latest`
remains the fallback when nothing is configured.

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool node@20 --tool pnpm@10 -- pnpm test
osdk exec -t bun@latest -- bunx vite
osdk exec -t java -- ./gradlew build   # uses the project's pinned JDK
```

`pnpx` routes to managed `pnpm dlx` and `bunx` to managed `bun x`; omitting the
corresponding backend is an error. A package-manager invocation may run registry
preflight before startup. Child failure makes osdk return an error, but exact
numeric exit-code pass-through is not currently guaranteed.

## Version aliases

```text
osdk alias set TOOL NAME TARGET
osdk alias list [TOOL]
osdk alias unset TOOL NAME
```

```bash
osdk alias set node maintenance 20
osdk alias set node default maintenance
osdk alias list node
osdk use node@default
osdk alias unset node maintenance
```

The CLI always edits user-global aliases. A project may define
`[alias.tools.<tool>]` manually and override a global name. Alias chains are allowed;
cycles are rejected. Names cannot be empty, contain whitespace or `@`, or use
`latest`, `current`, `stable`, `system`, `lts`, `lts/*`, `lts-latest`, or any
`lts/` or `lts-` prefix. Tool aliases are canonicalized before storage.

This kind of alias replaces a **version request** only. It is expanded by
install/use/uninstall, activation, shim selection, and global npm-tool version
resolution; it neither renames an executable nor duplicates an installation.
`[alias]` is a category namespace reserved for future capabilities such as
`alias.shell`; only `alias.tools` exists today, and unknown categories fail
explicitly.

## Tool name aliases

| Input | Canonical backend |
| --- | --- |
| `nodejs` | `node` |
| `py`, `cpython` | `python` |
| `jdk`, `openjdk` | `java` |
| `golang` | `go` |
| `rustup` | `rust` |
| `mvn` | `maven` |
| `kotlinc` | `kotlin` |

Next, read [Projects and Configuration](../config/files) to turn individual commands
into a shared project environment.

Use [source diagnostics](../config/tool-selection#inspect-sources) to list supported, present, missing and inapplicable kinds with effective order; default inspection does not execute system tools.
