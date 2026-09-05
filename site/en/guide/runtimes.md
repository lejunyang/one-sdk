# Runtimes and Ecosystem Tools

This page covers Node.js, Python, Java/JRE, Go, Rust, Maven, Gradle, and
Kotlin. See [JavaScript Package Managers](./package-managers) for package
managers and [Sources and Supply-chain Security](./sources-security#arbitrary-github-release-tools)
for arbitrary GitHub Release tools.

## Common command forms

```text
osdk install TOOL[@VERSION]... [-o|--opt KEY=VALUE ...]
osdk lock [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk upgrade [TOOL[@VERSION] ...] [-o|--opt KEY=VALUE ...]
osdk use|u TOOL[@VERSION] [-g|--global] [-o|--opt KEY=VALUE ...]
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
osdk list|ls [TOOL]
osdk list-remote|lsr TOOL [FILTER]
osdk current [TOOL]
osdk where TOOL[@VERSION]
osdk uninstall|rm TOOL@VERSION
```

`-o/--opt` is repeatable and must be `KEY=VALUE`. It applies to every tool in
the invocation; do not pass an option specific to one backend in a mixed-backend
command.

## Backend overview

| Backend | Tool aliases | Native version files | Install options |
| --- | --- | --- | --- |
| `node` | `nodejs` | `.nvmrc`, `.node-version`, `package.json` | `arch`, `corepack` |
| `python` | `py`, `cpython` | `.python-version` | `variant`, `tag` |
| `java` | `jdk`, `openjdk` | `.java-version`, `.sdkmanrc` | `distribution`, `package-type` |
| `go` | `golang` | `go.mod`, `.go-version` | none |
| `rust` | `rustup` | `rust-toolchain.toml`, `rust-toolchain` | `profile`, `components`, `targets` |
| `maven` | `mvn` | `.mvn-version` | none |
| `gradle` | — | `.gradle-version` | none |
| `kotlin` | `kotlinc` | `.kotlin-version` | none |

See [Project version discovery](./projects#project-version-discovery) for the
full precedence and the current native-file boundary of no-argument commands.

## Node.js

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

## Python

`python@3.14` is shorthand for default CPython. The complete request form is
`python@IMPLEMENTATION-VERSION+VARIANT`:

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@cpython-3.14+debug
osdk install python@cpython-3.14+freethreaded+debug
osdk install python@pypy-3.11
osdk install python@graalpy-3.12
osdk install python@pyodide-3.14
```

| Implementation | Supported variants | Identity examples |
| --- | --- | --- |
| `cpython` | `default`, `freethreaded`, `debug`, `freethreaded+debug` | `3.14.7`, `cpython-3.14.7+debug` |
| `pypy` | `default` | `pypy-3.11.x` |
| `graalpy` | `default` | `graalpy-3.12.x` |
| `pyodide` | `default` | `pyodide-3.14.x` |

| Option | Value | Effect |
| --- | --- | --- |
| `variant` | A variant above | Equivalent to request suffix `+VARIANT`; the explicit option wins |
| `tag` | A python-build-standalone date such as `20240224` | Select a historical PBS release for classic CPython |

Implementation, exact Python version, variant, and catalog artifact are locked,
and distinct identities can coexist. Standard CPython uses the embedded
python-build-standalone index; multiple implementations, variants, and
pre-releases use an embedded verified catalog. A custom catalog needs its exact
SHA-256:

```toml
[settings.python]
catalog_url = "/approved/python-catalog.json"
catalog_sha256 = "0123456789abcdef..."
```

`catalog_url` accepts HTTP(S) or a local path. Only a valid digest, schema, and
checksum on every artifact can replace last-good. Refresh failure tries
last-good and then the embedded catalog. See [Pre-releases](./sources-security#pre-releases).

Interpreter discovery uses:

```text
osdk python find [REQUEST]
```

```bash
osdk python find
osdk python find pypy-3.11
osdk python find 3.14+freethreaded
```

Managed results are filtered by the optional request. osdk then scans `PATH` and
system candidates, deduplicates them, and labels each as `managed`, `PATH`, or
`system`. It returns an error if nothing is found.

## Java JDK and JRE

```bash
osdk install java@21
osdk install java@zulu-17.0.1
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install java@21 -o package-type=jre
```

| Option | Values | Default |
| --- | --- | --- |
| `distribution` | A Foojay distribution ID such as `temurin` or `zulu` | `temurin` |
| `package-type` | `jdk` or `jre` | `jdk` |

The distribution can also be inline, as in `java@temurin-21`. Foojay lookup
filters distribution, OS, architecture, archive type, JDK/JRE, and Linux libc.
A JRE uses identity `jre-<resolved-version>` and can coexist with the matching
JDK. Execution exports `JAVA_HOME`.

Temurin versions carry a build number (such as `21.0.12+8`), and a PSU adds a
fourth segment (such as `21.0.12.1+1`). You may omit the build number in the
request: `java@21.0.12` matches the same-core `21.0.12+8`, and only falls back
to a four-part PSU when no same-core release exists.

The embedded Temurin LTS catalog covers 8, 11, 17, 21, and 25, allowing
resolution with an empty metadata cache. A locked artifact remains installable
when Foojay is unavailable. Configure a Foojay-compatible `/packages` endpoint
or static mirror with:

```toml
[settings.java]
catalog_url = "https://mirror.example/disco/v3.0/packages"
```

## JVM tools

```bash
osdk install maven@3.9.16
osdk install gradle@9.7.0
osdk install kotlin@2.4.10
```

The current JVM-tool catalog is fixed: Maven `3.9.16` with SHA-512, Gradle
`9.7.0` with SHA-256, and Kotlin `2.4.10` with SHA-256. Other versions fail.
Each has its own installation directory and shims; Kotlin also has a GitHub
proxy download candidate.

## Go

```bash
osdk install go@1.22
osdk use -g golang@1.23
```

Go has no backend-specific `-o`. osdk selects the host OS/architecture archive
from the go.dev JSON index and verifies its SHA-256. Download candidates include
go.dev, Aliyun, and golang.google.cn. Execution exports `GOROOT` and exposes
`go` and `gofmt`.

## Rust

The Rust backend delegates to rustup inside osdk's isolated directories:

```bash
osdk install rust@stable
osdk install rust@nightly -o profile=minimal \
  -o components=clippy,rustfmt \
  -o targets=wasm32-unknown-unknown,x86_64-pc-windows-gnu
```

| Option | Value | Default |
| --- | --- | --- |
| `profile` | Any profile accepted by rustup | `default` |
| `components` | Comma-separated rustup components | empty |
| `targets` | Comma-separated rustup targets | empty |

`latest`, `lts`, and `system` map to `stable`; `stable`, `beta`, `nightly`, and
exact toolchains are otherwise passed through to isolated rustup. The rustup
bootstrap itself uses `minimal` and installs no default toolchain. Runtime
exports `RUSTUP_HOME=<data>/rustup` and `CARGO_HOME=<data>/cargo`.
The lock preserves those floating channel names too, so reinstalling `stable`,
`beta`, or `nightly` later may yield a newer toolchain. Use an explicit or dated
toolchain when the result must be immutable.

### Components and targets

```text
osdk rust component add NAME [--toolchain TOOLCHAIN]
osdk rust component remove NAME [--toolchain TOOLCHAIN]
osdk rust component list [--toolchain TOOLCHAIN]
osdk rust target add NAME [--toolchain TOOLCHAIN]
osdk rust target remove NAME [--toolchain TOOLCHAIN]
osdk rust target list [--toolchain TOOLCHAIN]
```

`--toolchain` defaults to `stable`; all commands act on isolated rustup.

```bash
osdk rust component add rustfmt --toolchain stable
osdk rust target add wasm32-unknown-unknown --toolchain stable
```

### Status, overrides, and local toolchains

```text
osdk rust check [--repair]
osdk rust override import [PATH]
osdk rust override export [PATH]
osdk rust toolchain link NAME PATH
```

- `check` runs isolated `rustup check`; `--repair` creates missing markers for real toolchain directories and removes markers with no toolchain.
- `override import` reads isolated rustup's override for `PATH` (default current directory) and writes it to the nearest project `osdk.toml`.
- `override export` writes the active osdk Rust pin for that directory as an isolated rustup directory override.
- `toolchain link` requires a canonicalizable `PATH` containing `bin/`; `NAME` cannot contain whitespace or slashes or equal `.` or `..`.

A linked toolchain can run through shims and activation, but it is a machine-local
path and `osdk lock` rejects it as a reproducible artifact.
