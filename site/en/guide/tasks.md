# Project tasks

Declare project commands under `[tasks]` in `osdk.toml` and run them with
`osdk run <name>`. The goal is to cover what most Makefiles and npm scripts are
used for: tasks are phony by default so there is no `.PHONY`, prerequisites run
in topological order, and osdk injects the tool versions the project declared.

## The simplest form

```toml
[tasks]
build = "cargo build --release"
test = "cargo test"
```

```
osdk run build
```

This tier covers most cases, and most tasks in most projects should stay here.

## Several commands

```toml
[tasks.ci]
run = [
  "cargo fmt --check",
  "cargo clippy -- -D warnings",
  "cargo test",
]
```

**Entries run in order and the task stops at the first failure**; later commands
do not run. That is what chaining with `&&` would give you, without depending on
any shell syntax, so it behaves identically on Windows and Unix.

### Why not `&`

`&` means three different things depending on the shell, so one config would do
two different things on two platforms — and neither would report anything:

| shell | what `a & b` means |
| --- | --- |
| Unix `sh` | background a, start b immediately (concurrent) |
| Windows `cmd` | run a, then b, **unconditionally** — a's failure is swallowed |
| PowerShell 7 | a trailing `&` backgrounds a job |
| PowerShell 5.1 | parse error |

So "keep going" and "run in parallel" each get their own spelling.

### Keep going after a failure

```toml
[tasks.ci]
run = [
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },
  "cargo test",
]
```

A tolerated failure does not fail the task, but it **does print a warning** —
tolerated is not the same as unnoticed.

### Run in parallel

```toml
[tasks.check]
run = [
  "cargo fmt --check",
  { tasks = ["clippy", "test", "doc"] },
  "echo all-green",
]
```

The tasks inside `{ tasks = [...] }` run together, and the next step waits for
**all** of them. osdk waits and collects their exit codes, stopping at the first
failure — precisely what `&` cannot do, since a backgrounded process is never
reaped and its failure is silently dropped.

## Dependencies

```toml
[tasks.build]
run = "cargo build"
depends = ["fetch-deps"]
```

Prerequisites run first, and a shared one runs only once. **Order among
prerequisites is not guaranteed**: when you really need "A first, then B and C
together", express it with `run` steps as above rather than stacking `depends`.

A dependency cycle is reported before any command runs, with the path:

```
error: task dependency cycle: a -> b -> a
```

A misspelled name in `depends` is caught the same way, rather than surfacing
halfway through a pipeline that already had effects.

## Windows variants

```toml
[tasks.build]
run = "make"
run_windows = "nmake"
```

`run_windows` **replaces** `run` on Windows rather than adding to it.

The default interpreter is `cmd /c` on Windows and `sh -c` elsewhere. `sh` is
deliberately not the Windows default: requiring it would mean requiring Git for
Windows or Cygwin, which contradicts osdk's "install it and it works" premise.
Note that a developer machine may well *happen* to have `sh` on PATH — it can
even be one of osdk's own shims — so a config that relies on it works locally
and fails on a clean machine.

To choose a different interpreter:

```toml
[tasks.deploy]
run = "./deploy.ps1"
shell = "pwsh -Command"
```

Or set a default for the whole config scope:

```toml
[task_config]
shell = "pwsh -Command"
```

## The environment a task sees

Before starting a task, osdk **injects the project's environment itself** — the
same one `osdk hook-env` gives a shell: tool bin directories prepended to
`PATH`, variables like `JAVA_HOME` and `GOROOT` exported per project config,
plus package-cache and model-provider variables.

So a task sees the tool versions the project declared **whatever directory or
shell it was invoked from**, and without the user having run `osdk activate`.

osdk also sets two variables:

| Variable | Meaning |
| --- | --- |
| `OSDK_TASK` | always `1`; marks the process as an osdk task |
| `OSDK_TASK_NAME` | the current task's name |

The first exists so an osdk activation hook in a shell profile can tell that the
environment is already prepared and skip layering a second activation on top.

A task's own `env` wins over all of the above:

```toml
[tasks.test]
run = "cargo test"
env = { RUST_BACKTRACE = "1" }
```

## Other fields

```toml
[tasks.release]
description = "Package release artifacts"   # shown by osdk task list
alias = ["rel"]                             # osdk run rel
dir = "packaging"                           # relative to the config file
hide = true                                 # hidden from the listing
quiet = true                                # silence osdk's own output
when = { os = "linux" }                     # same filter syntax as [tools]
```

A task excluded by `when` is not treated as nonexistent:

```
$ osdk run linuxonly
error: task `linuxonly` is not available on this platform (os=linux)
```

`osdk task list` lists it separately too, rather than letting it vanish.

## Commands

| Command | Purpose |
| --- | --- |
| `osdk run <name>` | Run a task |
| `osdk run <name> --dry-run` | Print what would run |
| `osdk task list` | List tasks (`--hidden` includes hidden ones) |
| `osdk task info <name>` | Show the merged definition |
| `osdk task deps <name>` | Print the execution order |

Note there is only `osdk run <name>`, never a bare `osdk <name>`: the bare form
gets shadowed by any subcommand added later, a trap mise hit and now advises
scripts against.

When a task fails, `osdk run` exits with that task's exit code, so it composes
directly into CI scripts.

## Trust

`[tasks]` runs arbitrary commands on the machine, so a project config that
contains it requires explicit trust:

```
$ osdk task list
error: project config is not trusted: /path/to/osdk.toml
these keys need review because they affect what runs on this machine:
  tasks -- can run arbitrary code on this machine during install
```

Review it, then `osdk trust`. `task_config` requires trust for the same reason —
its `shell` field decides which interpreter every task runs under.

## What this is not

osdk tasks are a **task runner**, not a build system. Make's pattern rules
(`%.o: %.c`) and automatic variables (`$@` / `$<`) are deliberately not
provided: they are a language for generating one rule per file, and adopting
them means also owning dependency-chain inference, VPATH, and intermediate-file
lifetimes — that is make's full complexity. Neither mise, just, Task,
cargo-make, npm scripts, deno task, nor Turborepo offers pattern rules; the
judgement is unanimous.

Compiling a source tree is cargo's, tsc's, or go build's job. The task runner's
job is to **invoke them with the right tool versions and environment**.
