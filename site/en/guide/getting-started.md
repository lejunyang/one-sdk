# Getting Started

This page introduces osdk's shared command surface. See [Runtimes and Ecosystem
Tools](./runtimes) for backend options, [Projects and Configuration](./projects)
for discovery, and [Reproducible Lockfiles](./lockfiles) for precise read/write
rules and the floating-Rust-channel exception.

## Your first workflow

```bash
# Install explicit requests; prefixes select the highest matching stable version
osdk install node@20 python@3.12

# Install and write the request to this project
osdk use node@20

# Generate and commit the selected platform's resolution
osdk lock

# Recreate it on another machine of the same platform
osdk install

# Run temporarily without changing a project pin
osdk exec --tool node@20 -- node --version
```

Tool requests normally use `TOOL@VERSION`. Omitting `@VERSION` means `latest`.
Common requests include exact versions (`20.11.1`), prefixes (`20`, `20.11`),
`latest`, `current`, `stable`, `lts`, `lts/iron`, and user aliases. Supported
channels and ranges vary by backend.

## Global options

```text
osdk [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

Global options may appear before or after the subcommand.

| Option | Environment | Effect |
| --- | --- | --- |
| `-v`, `--verbose` | `OSDK_LOG` separately sets a tracing filter | Repeat for info, debug, then trace output |
| `-q`, `--quiet` | — | Hide download/install progress; it does not hide normal results or approve deletion |
| `-j N`, `--jobs N` | `OSDK_JOBS` | Maximum concurrent downloads/installations; CLI `0` does not override config, and execution uses at least 1 |
| `-y`, `--yes` | `OSDK_YES` | Approve uninstall, archive-cache cleanup, and real GC |
| `--source ID` | — | Put source `ID` first while retaining fallbacks; tool requests must use the canonical backend ID (for example, `node`, not `nodejs`) |
| `--refresh-sources` | — | Force re-probing for `install`, `use`, `upgrade`, and `exec`; `model pull` refreshes only with no explicit endpoint/pin, `auto` selection, and online mode; no effect on `lock`, `outdated`, or `list-remote` |
| `--source-mode MODE` | `OSDK_SOURCE_MODE` | `auto` (default) validates a mirror set in the environment and ranks it together with the built-in mirrors; `env` uses only that mirror and fails when it is missing or unusable |
| `--offline` | `OSDK_OFFLINE` | Prohibit network access and use cached metadata/artifacts only |
| `--require-checksums` | `OSDK_REQUIRE_CHECKSUMS` | Reject an artifact without a normal checksum or trusted attestation digest |
| `--attestations POLICY` | `OSDK_ATTESTATIONS` | `off`, `if-available`, or `required` |
| `--prerelease POLICY` | `OSDK_PRERELEASE` | `never`, `if-explicit`, or `allow` |
| `--lang LANG` | `OSDK_LANG` | `en` or `zh`; affects help and argument errors too |
| `-h`, `--help` | — | Show help |
| `-V`, `--version` | — | Show the version |

CLI boolean flags enable a behavior for that invocation; the same flag cannot
turn a configured `true` back to `false`. For example, disable signature
verification with `OSDK_VERIFY_SIGNATURES=false` or configuration.

## Install, lock, check, and upgrade

```text
osdk install|i [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk outdated [TOOL[@VERSION] ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
```

| Command | Behavior |
| --- | --- |
| `install` | Install tools and generate shims; with no tool and no `-o`, consume the current-platform lock section when present |
| `lock` | Resolve requests and write a platform-partitioned `osdk.lock`; do not install; floating Rust channels remain channel names |
| `outdated` | Re-resolve configuration or explicit requests and report targets not installed; never read the lock |
| `upgrade` | Re-resolve, install, and refresh the lock; never use the old lock as resolution input |

`-o/--opt` is repeatable and must be `KEY=VALUE`. A later duplicate wins. The
same option set is applied to every tool in a multi-tool command, so do not mix
backend-specific options with unrelated tools.

```bash
osdk --jobs 4 install node@20 go@1.22 python@3.12
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk lock node@20 -o arch=arm64
osdk outdated node@20 python@3.12
osdk upgrade
```

See [Reproducible Lockfiles](./lockfiles) for the exact read/write matrix.
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
`--yes` in automation. Newly unreferenced CAS objects are collected afterward.

## Inspect local and remote versions

```text
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
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

`--tool` is required and repeatable. osdk installs those requests if needed,
builds their exact `PATH` and backend environment, and starts `COMMAND` once. It
does not read the project lock or change pins.

```bash
osdk exec --tool python@3.12 -- python -c "print('ok')"
osdk exec --tool node@20 --tool pnpm@10 -- pnpm install
```

`pnpx` routes to managed `pnpm dlx`; `bunx` routes to managed `bun x`. The
corresponding backend must be included. Package-manager commands may also run
[registry preflight](./package-managers#registry-preflight). Child failure makes
osdk return an error; exact child exit-code pass-through is not guaranteed.

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
`[aliases.<tool>]` manually and override a global name. Alias chains are allowed;
cycles are rejected. Names cannot be empty, contain whitespace or `@`, or use
`latest`, `current`, `stable`, `system`, `lts`, `lts/*`, `lts-latest`, or any
`lts/` or `lts-` prefix. Tool aliases are canonicalized before storage.

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

Next, read [Projects and Configuration](./projects) to turn individual commands
into a shared project environment.
