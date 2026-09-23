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

## The third tier: script files

Somewhere around twenty lines a script outgrows a TOML string: the editor stops
highlighting it, the linter stops seeing it, and the escaping costs more than
the braces save. Move it into `osdk-tasks/`:

```
osdk-tasks/
  build.ps1          -> osdk run build
  test/units.ps1     -> osdk run test:units
  test/_default.ps1  -> osdk run test
```

The filename without its extension is the task name, directories become `:`
namespaces, and `_default` names the directory itself. A script can also be
pointed at explicitly:

```toml
[tasks.release]
description = "Cut a release"
file = "scripts/release.ps1"
```

`file` resolves against **the config file that declared it**, not the merged
root -- otherwise a relative path in a global config would point into whichever
project happens to be current rather than at its own directory.

### Metadata in the script header

A script declares its own description and dependencies in comments, without
going back to the TOML:

```powershell
#OSDK description="Build the release artifacts"
#OSDK depends=fetch, lint
```

Parsing stops at the first non-comment line, so the header has to be at the top
-- buried in the middle is somewhere nobody would look for it.

### Using a different directory

```toml
[task_config]
includes = ["tools/tasks"]
```

This **replaces** the defaults rather than adding to them: a project that moves
its scripts rarely wants the old directories still searched, and keeping them
silently would resurrect the very tasks the move meant to retire.

### Windows visibility

NTFS has no execute bit, so the rule is: **the extension is one of
`exe/bat/cmd/com/ps1/vbs`, or the file starts with a shebang**. Either will do.
A file with neither works on Linux and macOS and cannot be launched on Windows.

Such a task still appears in `osdk task list` with the reason attached rather
than vanishing -- a task that simply disappears sends you back to check the
spelling, and the actual fix (add an extension or a shebang) is not guessable
from an absence.

The cross-platform idiom is a same-name pair: `build` (with a shebang) beside
`build.ps1`. Windows takes the latter, everything else the former, and the task
is called `build` either way.

### Which PowerShell runs a `.ps1`

`cmd` cannot launch a `.ps1` directly, so such a script always goes through
PowerShell. The choice is made at run time: **`pwsh` (7+) when it is on PATH,
falling back to the bundled `powershell` (5.1) when it is not.**

This is not optional politeness. PowerShell 7 is a separate download on Windows,
and a fresh install has only 5.1 — hardcoding `pwsh` makes every `.ps1` task
fail there with a bare "program not found".

Both accept `-File`, so the rest of the command line is identical. If a script
needs syntax only 7 has, check `$PSVersionTable.PSVersion` inside it.

### Interaction with the shell hook

A task's environment is injected by osdk directly, so the activation snippet for
all four shells **stands down** when it sees `OSDK_TASK`. Otherwise the profile
hook would re-derive the environment from the config inside the task, discarding
the task's own `env` and any tool it had just installed.

## The fourth tier: embedded Lua

The first three tiers cover the vast majority of tasks. When you genuinely need
**conditionals, loops that generate work, or cross-platform path arithmetic**,
use `lua`:

```toml
[tasks.sync]
lua = """
for _, target in ipairs(osdk.argv) do
  local dest = osdk.path.join(osdk.project_root, "dist", target)
  local code = osdk.run("cp", "-r", target, dest)
  if code ~= 0 then return code end
end
return 0
"""
```

The field is `lua` rather than `run` so the tier is visible at a glance, and a
task setting both is **rejected** instead of leaving the runner to guess which
one you meant.

Returning nil or true succeeds, a number becomes the exit code, and
`return false` counts as failure. Raising an error fails the task with that
message.

### Available API

| | |
| --- | --- |
| `osdk.sh(cmd)` | Run through the platform shell, **returning an exit code** rather than throwing |
| `osdk.run(prog, ...)` | Exec directly; each argument stays separate, no shell |
| `osdk.path.join(...)` | Join with the host separator |
| `osdk.path.exists(p)` | Whether a path exists |
| `osdk.env(name)` | Read an environment variable, nil when unset |
| `osdk.platform.os` / `.windows` / `.arch` | Platform facts |
| `osdk.project_root` / `osdk.dir` / `osdk.task` | Location and identity |
| `osdk.args.<name>` / `osdk.argv` | Declared arguments and the leftovers |

Prefer `osdk.run` when building a command from data: each argument becomes one
argv entry, so a value with spaces or metacharacters cannot split or be
reinterpreted. `osdk.sh` suits a fixed one-liner.

### Standard library and environment variables

The full Lua 5.4 standard library is loaded — `string`, `table`, `math`, `os`,
`io`, `coroutine` and `utf8`, all except `debug`. The interpreter is statically
linked, so **`require` can load plain Lua files but not C extension modules**
(no `lfs`, no `socket`).

`os.getenv` is redirected to the same source as `osdk.env`, so the two never
disagree: the task's declared `env` first, then the process environment. This
matters because a task's `env` applies to the children osdk spawns, not to osdk
itself — stock `os.getenv` would return nil for precisely the variables the task
declared, and a nil is indistinguishable from "unset", making the `env` table
look broken.

(Injecting into the real process environment would also make them agree, but it
would leak one task's `env` into every later task, the freshness state and the
trust checks — and `set_var` is a data race under threads.)

### This is not a sandbox

The config file has already passed the trust gate, and `run` in the same file
can execute arbitrary shell, so confining Lua would protect nothing. What the
host API offers is **convenience with correct semantics**, not isolation.

### A C compiler is needed at build time

Lua is compiled from source and statically linked by `mlua`, so there is no
runtime dependency — but building osdk needs a C compiler. It sits behind the
`scripts` feature, which is on by default; in a build with it disabled, a task
using `lua` reports an error telling you to use `run` instead.

Size: about +335 KB on osdk, and **not a single byte** on `osdk-shim` — the shim
only dispatches already-installed tools and never evaluates a task, so the
engine is not in its dependency graph at all.

## Timeouts

```toml
[tasks.e2e]
run = "pytest tests/e2e"
timeout = "5m"
```

Write `30s`, `5m`, `1h`, or a plain number of seconds. A malformed value is
rejected **before anything runs** rather than silently meaning "no timeout" —
asking for a limit and not getting one is the worst outcome available.

### The whole process tree is killed

When the limit expires, osdk terminates **every process the task started**, not
just the one it launched directly. That distinction is the common case rather
than an edge case: with `cmd /c npm test`, killing `cmd.exe` ends the task
immediately while the `node` it started keeps running, holding the port and the
files.

The mechanisms differ by platform:

- **Windows** uses a job object. The child is placed in the job at spawn time,
  processes it creates inherit the job, and terminating the job terminates all
  of them. `KILL_ON_JOB_CLOSE` is also set, so an osdk that dies unexpectedly
  has the tree reclaimed by Windows instead of leaking it.
- **Unix** uses `setsid` to make the child a process-group leader, then signals
  the group with `kill(-pgid)`: `SIGTERM` first, with five seconds to clean up,
  then `SIGKILL` for whatever ignored it.

Both set the grouping up **before** the process starts — once a process has
forked there is no reliable way to find all of its descendants.

Timeouts compose with teardown: a task killed by its timeout did start, so its
`run_post` still runs.

## Teardown that runs even on failure

```toml
[tasks.e2e]
depends = ["start-db"]
run = "pytest tests/e2e"
run_post = "docker compose down"
```

`run_post` runs after `run`, **including when `run` failed** — which is the
entire reason it exists. A final line inside `run` cannot do this: a failing
task never reaches it, and the test database stays up.

It belongs to the `run` family rather than the `depends` family, so it takes
**commands**. The common one-line cleanup needs no separate task that nobody
would ever invoke directly; when the teardown really is shared,
`{ tasks = [...] }` still works:

```toml
run_post = [{ tasks = ["stop-db", "notify"] }]
```

> mise calls this `depends_post`. That name reads as a kind of dependency and it
> is not one: prerequisites run before and decide whether the task runs at all,
> while teardown runs after and decides nothing.

### When it does not run

| Situation | Teardown | Why |
| --- | --- | --- |
| `run` succeeded | runs | — |
| `run` failed | **runs** | it started, so it has something to clean up |
| a dependency failed, `run` never started | skipped | nothing was set up |
| skipped as up to date by freshness | skipped | same |

### Exit codes

The **first** failure wins: if `run` exits 3 and teardown succeeds, the task is
still 3 — cleaning up is not passing. If `run` succeeds and teardown fails, the
task fails, because the machine is not in the state the task promised.

When teardown has several steps, **the rest still run after one fails** —
stopping halfway would strand exactly the resources this is meant to release.

> `run_post_windows` is not supported yet. For a platform-specific teardown, use
> `run_post = [{ tasks = ["cleanup"] }]` pointing at a task that has its own
> `run_windows`.

## Passing arguments

The simple case needs no declaration at all: a task with **exactly one command**
takes leftover arguments on the end.

```toml
[tasks]
test = "cargo test"
```

```
$ osdk run test -- --nocapture
# runs: cargo test --nocapture
```

The rule keys on "one command step", not on "`run` was written as a string", so
`run = "cargo test"` and `run = ["cargo test"]` behave identically. Rewriting one
into the other never silently stops arguments from being accepted.

With more than one step the task **refuses**, and says how to fix it:

```
$ osdk run ci -- --nocapture
error: task `ci` does not accept arguments: it has several steps, so there is no
single place to append them. Add `{{args}}` to an argv step, or declare them
with [[tasks.ci.args]]
```

There is no guess at "append to the last one", because with several steps that
answer does not hold up: the last command? every command? what about tasks inside
a `{ tasks = [...] }` step? npm can append because a script is always exactly one
command.

### Declaring arguments

```toml
[tasks.deploy]
run = [{ argv = ["kubectl", "apply", "-f", "{{manifest}}", "--context", "{{env}}"] }]

[[tasks.deploy.args]]
name = "env"
help = "Target environment"
choices = ["staging", "prod"]

[[tasks.deploy.args]]
name = "manifest"
default = "k8s/app.yaml"     # having a default makes it optional

[tasks.deploy.options.replicas]
default = "3"

[tasks.deploy.flags.wait]
help = "Wait for rollout to finish"
```

```
osdk run deploy -- prod --replicas 5 --wait
```

Positionals use `[[...]]` arrays because **order is their meaning**; options and
flags are unordered, so they are tables. <code v-pre>{{args}}</code> stands for everything not
otherwise consumed and expands to **separate arguments**, never one joined
string.

Validation runs before any command does: a value outside `choices`, or a missing
required argument, is reported up front rather than halfway through.

### Why substitution only happens inside `argv`

<code v-pre>{{...}}</code> is substituted in `{ argv = [...] }` steps and **not** in `run`
strings. The reason is escaping:

| shell | can any value be escaped safely? |
| --- | --- |
| `sh -c` | yes (single quotes plus `'\''`) |
| `pwsh` | yes (single quotes plus `''`) |
| **`cmd /c`** | **no** — `%VAR%` expands before quoting is considered |

A feature that is safe on two platforms and injectable on the third is worse
than no feature, because it gets trusted. In an `argv` step each element becomes
one argument with no parser in between, so this is not "we escape carefully" but
"there is no parsing step left to inject into":

```
$ osdk run deploy -- prod 'x & echo pwned > bad.txt'
# manifest receives the whole string; & is never interpreted, bad.txt is never created
```

Pipes, redirection and globbing still belong in `run = "..."`, which simply does
not substitute. To pass arguments, use `argv` or a script file.

Every value is also exported as an environment variable (`osdk_arg_env`,
`osdk_args`). That path has no escaping problem at all — values enter the child's
environment block without passing through a parser — so anyone who knows their
own shell can use them inside a `run` string at their own risk.

## Incremental: skip when inputs have not changed

```toml
[tasks.build]
run = "cargo build --release"
sources = ["Cargo.toml", "crates/**/*.rs"]
outputs = ["target/release/osdk.exe"]
```

When every `source` is older than every `output`, the task is skipped:

```
$ osdk run build
build: up to date, skipped
```

The default comparison is modification time. Three choices:

| `freshness` | Behaviour | When |
| --- | --- | --- |
| `"mtime"` (default) | Compare modification times | Cheap, but `git checkout` and CI cache restores rewrite timestamps |
| `"hash"` | Compare content hashes | Immune to touched-but-unchanged files, at the cost of reading every input |
| `"always"` | Never skip | Opt out of incrementality |

**The task definition counts as an input too**: editing `run` reruns the task
even when no source file moved.

With `outputs` omitted, osdk compares against the last successful run, stored in
the cache directory rather than in your project, so nobody needs a `.gitignore`
entry for it.

### Two things worth knowing

**A pattern that matches nothing is an error, not "no inputs".**

```
$ osdk run build
error: task `build`: sources: ["src/**/*.typo"] matched no files; a pattern that
matches nothing would make this task's freshness check silently meaningless
```

A `sources` entry matching zero files makes the freshness check vacuous — the
task is then either skipped forever or rerun forever, while the config looks
perfectly fine. Failing outright is the only version of this that is debuggable.

**Globs are scanned from their literal prefix.** `crates/**/*.rs` descends into
`crates/` only, never the whole project. This is not a micro-optimization: a
measured scan rooted at the current directory next to a `target/` tree took
2,827.91 ms against 61.26 ms from the correct starting point — 46x. Prefer
patterns with a directory prefix; a bare `**/*.rs` walks everything.

Prefix a pattern with `!` to exclude:

```toml
sources = ["src/**/*.rs", "!src/generated/**"]
```

### wait_for: ordering without scheduling

```toml
[tasks.serve]
run = "npm start"
wait_for = ["migrate"]
```

`wait_for` differs from `depends` in exactly one way: **what happens when the
named task is not scheduled**. `depends` pulls it in and runs it; `wait_for`
does nothing.

It says "if we are both running, I go second" without making the other task a
prerequisite — useful when two tasks touch the same resource but neither needs
the other's output.

Naming a task that does not exist is therefore **not an error**; it is the case
the field exists for. The cost is that a typo is silent, so `osdk task info`
prints the field for when the ordering does not come out as intended.

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
| `osdk task add <name> --run <cmd>` | Write it into the project config; repeat `--run` for a sequence |
| `osdk task rm <name>` | Remove it from the project config |
| `osdk task edit <name>` | Open the config in `$EDITOR` |

`add` and `rm` preserve the file's existing comments, indentation and ordering —
a config is something a person wrote, and adding one task should not reformat
the rest of it. A single command is written as the one-line shorthand; the table
form appears only when there are several commands or extra metadata.

`add` validates before writing: a config that will not load is worse than a
rejected command, because the next osdk run then fails on something the user
never typed.

Note there is only `osdk run <name>`, never a bare `osdk <name>`: the bare form
gets shadowed by any subcommand added later, a trap mise hit and now advises
scripts against.

When a task fails, `osdk run` exits with that task's exit code, so it composes
directly into CI scripts.

## Trust

**Declaring a task does not require trust.** osdk never runs a task on its own:
there is no postinstall, no lifecycle hook, no automatic invocation — `[tasks]`
is read by `osdk run` and `osdk task` and nowhere else. Typing `osdk run build`
*is* the authorization, so demanding a trust record first asks the same question
twice, and a gate that fires on something you just asked for only teaches people
to approve without reading.

The contrast with `syspkg` makes the rule clear: it acts during `osdk install`,
which you did not request per package, so review has to happen beforehand. A
task only ever runs because someone named it.

**`task_config` does still require trust**, because it is not a command you name
but an ambient setting:

```
$ osdk task list          # only prints what is declared; needs no trust
build
test

$ osdk run test           # actually uses that interpreter, so it is refused
error: project config is not trusted: /path/to/osdk.toml
these keys need review because they affect what runs on this machine:
  task_config -- decides which interpreter runs your tasks, so a task may not run what it says
```

Note which command is refused: `osdk run`, not `osdk task list`. Listing only
reports what the file says and executes nothing, so it keeps working on an
untrusted project -- otherwise the route to *reading a config before deciding
whether to trust it* would itself be blocked.

Its `shell` field decides which interpreter **every** task in scope runs under.
A config that quietly sets `shell = "evil --run"` turns every later `osdk run`
into something other than what the task text says, with nothing at the call site
to reveal it. Review it, then `osdk trust`.

## Monorepos: tasks in sub-projects

`[task_config].roots` declares which directories are sub-projects. Their tasks join
the same table under `//<path>:<name>`:

```toml
# osdk.toml at the repository root
[task_config]
roots = ["apps/*", "packages/*"]

[tasks.hello]
run = "echo root"
```

```toml
# apps/api/osdk.toml
[tasks.build]
run = "cargo build"
```

```
$ osdk task list
//apps/api:build
//packages/ui:build
hello

$ osdk run //apps/api:build
```

The prefix is not decoration: without it, `packages/ui`'s `build` would replace the
identically named task in `apps/api`. This is the same addressing `[deps].roots` uses
for `//apps/api:uv` -- one syntax, not a second one.

**Only declared directories are read.** A directory with a perfectly good `osdk.toml`
sitting beside the others is not found unless a pattern covers it. For tasks this
matters more than it does for dependencies: a `run` line is an arbitrary command, so
"discovering a project by accident" means "discovering code to execute by accident".
Patterns match exactly as `[deps].roots` does -- segment by segment, single-level `*`
only, `..` rejected -- because both share one implementation rather than each having
its own.

### Dependencies across sub-projects

A `//` prefix in `depends` crosses sub-projects; without one, the name stays inside
the sub-project that declared it:

```toml
# apps/web/osdk.toml
[tasks.prep]
run = "npm run codegen"

[tasks.build]
run = "npm run build"
depends = ["//packages/ui:build", "prep"]
```

`//packages/ui:build` names the other sub-project; `prep` means the one **next to
it**, even though `packages/ui` has a task by that name too. A sub-project's config
therefore reads as what it literally says, and a `depends = ["prep"]` written before
the move into a monorepo needs no change.

Cycles are caught, across sub-projects as well:

```
$ osdk run //apps/web:build
error: config error: task dependency cycle: //apps/web:build -> //packages/ui:build -> //apps/web:build
```

Both ends are named, so you can see which edge to remove.

### A sub-project cannot declare `[task_config]`

A sub-project contributes task **definitions** only. `[task_config]` holds
scope-wide settings, and `shell` among them decides which interpreter every task runs
under. That power stays with the config that declares `roots` -- the one you actually
reviewed and ran `osdk trust` on.

```
$ osdk task list
error: config error: apps/api/osdk.toml: a sub-project cannot declare `[task_config]`;
runner defaults such as `shell` belong to the config that declares `[task_config].roots`
```

An **error**, not a silent omission: a setting that is written down, has no effect and
draws no complaint is worse than an error, because its author believes it worked.

### Declaring roots means `osdk run` needs trust

`roots` lives in `[task_config]`, and that table is already behind the trust gate
(previous section). So declaring sub-projects makes `osdk run` ask for `osdk trust`
first -- not a new gate added for monorepos, but the existing one inherited
automatically. `osdk task list` keeps working, because it reports without executing.

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
