# Runtimes Overview

osdk manages every language runtime with the same commands. This page shows the shared command forms and a backend overview; each language has its own page for options. See [Package Managers](../tools/package-managers) for JavaScript package managers and [GitHub Release Tools](../tools/github-releases) for arbitrary release binaries.

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

When an existing structured tool is excluded from the current platform by
`when`, `osdk use TOOL@VERSION` updates only its `version` and preserves `when`,
`lazy`, and backend options. It does not bypass the platform condition to install
the tool or write a lock entry for the current platform. `-o/--opt` cannot be
changed in this case; update and verify backend options on a matching platform.

An inline option block is written `tool[key=value,...]@selector`, with the block
before the `@`. **Quote the whole operand on PowerShell**: it treats an unquoted
comma inside an argument as an array separator and splits one expression into two.

```bash
osdk install 'npm:esbuild[installer=pnpm,allow_builds=true]@0.21'
```

Quote the value as well when it contains commas: `allow_builds="a,b"`. When an
unquoted operand does get split, osdk recognizes it and prints the correctly quoted
command instead of complaining about an unterminated bracket.

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
| `zig` | — | `.zig-version` | none |

See [Project version discovery](../project/discovery#project-version-discovery) for the
full precedence and the current native-file boundary of no-argument commands.
