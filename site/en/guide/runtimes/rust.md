# Rust

Configuration reference: [applicable tool_order sources](../config/tool-selection#rust), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#rust).

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

The source probe ranks candidates using a generic stable manifest; it does not
prove that a mirror has synchronized the requested exact toolchain, component,
or target. Every downloading rustup operation runs the complete command against
each ranked source in turn. A 404, transfer failure, or rustup rejection falls
through until the command itself succeeds.

## Exposed commands and `cargo install`

On install and on `osdk reshim`, osdk generates shims for **every** executable in
the active toolchain `bin` and the isolated `CARGO_HOME/bin`, not just the five core
launchers `rustc`, `cargo`, `rustup`, `rustfmt`, and `clippy-driver`. rustup proxies
such as `rustdoc`, `rust-analyzer`, and `cargo-miri`, together with any CLI installed
later through the managed `cargo`, are exposed as well. These shims still inject the
isolated `RUSTUP_HOME`/`CARGO_HOME` at run time, so they can never reach a system
rustup.

Third-party tools installed with the managed `cargo install` land in `<data>/cargo/bin`
(not the system `~/.cargo`); run `osdk reshim` once afterwards to make a new command
directly callable from your shell. The `cargo <subcommand>` form (for example
`cargo tauri`) needs no reshim, because cargo locates that launcher in the isolated
`CARGO_HOME/bin` itself.

## Management boundary versus a system rustup

osdk drives only the isolated rustup under its data directory and never takes
over a rustup already installed on `PATH`:

- Before `osdk install rust` has run, the isolated rustup does not exist;
  `osdk rust *` fails with an explicit message pointing at the install command
  instead of falling through to the system rustup.
- `osdk source pin rust <source>` and the one-shot `--source <source>` change
  only the `RUSTUP_DIST_SERVER` injected when osdk drives the managed rustup.
  Neither edits an external rustup, its environment, or its configuration; when
  no managed Rust exists the command additionally prints this scope note. To
  mirror a system rustup, set `RUSTUP_DIST_SERVER`, `RUSTUP_UPDATE_ROOT`, and
  related variables yourself.
- Every managed operation that downloads (`osdk install rust`,
  `osdk rust component add`, `osdk rust target add`) shares one source
  selection, so a pinned source also applies when you add a component or target
  later. A pin only puts that source first; an unavailable target still falls
  through to the remaining candidates.
- The managed rustup always uses the source osdk selected: a `RUSTUP_DIST_SERVER`
  or `RUSTUP_UPDATE_ROOT` already exported in your shell does not affect managed
  operations, so an external mirror cannot override osdk's choice. To use a
  different source for one run, pass `--source <source>` instead of exporting
  those variables.

## Components and targets

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

## Status, overrides, and local toolchains

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
