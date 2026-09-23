# Application Dependencies

`osdk install` installs **tools**: it puts Node, pnpm, or Python into osdk's own
isolated directories. `osdk deps` installs **a project's own dependency
manifest**: it reads `package.json`, drives the project's own package manager,
and lands the whole dependency closure inside the project.

The two do not overlap:

| What you want | Use |
| --- | --- |
| Provide pnpm itself | `osdk install pnpm`, or `[tools]` |
| Turn a whole `package.json` into `node_modules` | `osdk deps` |
| Add or remove one dependency | `osdk install npm:<package>` |

## Enabling it

`deps` does not act just because a `package.json` is nearby. That would be an
implicit, large side effect -- the same judgement that keeps `osdk install` from
fetching models on its own. Declare a provider explicitly:

```toml
# osdk.toml
[deps.pnpm]
```

One line is enough. From there, two ways to use it.

**Explicitly:**

```bash
osdk deps --list            # detected providers and their freshness
osdk deps --dry-run         # print what would run, without running it
osdk deps                   # materialize the manifest
osdk deps --explain         # also explain each freshness decision
```

**Automatically (on by default):** a bare `osdk install`, `osdk run <task>` or
`osdk exec` checks whether declared dependencies are still fresh and materializes
them if not. So after cloning a project, `osdk run dev` just works.

That check is cheap: it compares manifest hashes and **does not scan installed
files**. Roughly 0.16ms for a 20KiB lock, 2ms for a 2MiB monorepo lock -- and on a
hit it does nothing else, with no package manager started. Deep verification is a
separate thing, done only when you ask for `osdk deps --verify`.

Three cases never trigger it:

```bash
osdk install node@22        # a named tool installs that tool, nothing else
osdk run build --no-deps    # skip once; install and exec take it too
osdk run build --dry-run    # --dry-run is supposed to have no effects
```

To turn it off for a provider permanently:

```toml
[deps.pnpm]
auto = false                # only affects automatic runs; `osdk deps` still does it
```

With no `[deps]` section, `osdk deps` only tells you what it found:

```
no `[deps]` section; found manifests osdk could manage:
  /path/to/project/package.json
    candidates: bun, npm, pnpm, yarn

enable one in osdk.toml, for example:
  [deps.bun]
```

## Supported providers

| provider | ecosystem | manifest | native lock |
| --- | --- | --- | --- |
| `npm` / `pnpm` / `yarn` / `bun` | Node | `package.json` | its own lockfile |
| `uv` | Python | `pyproject.toml` | `uv.lock` |
| `pip-requirements` | Python | `requirements.txt` | none (it *is* the lock when fully pinned) |
| `go` | Go | `go.mod` | `go.sum` |
| `cargo` | Rust | `Cargo.toml` | `Cargo.lock` |
| `deno` | Deno | `deno.json` / `deno.jsonc` | `deno.lock` |

### Two ways go / cargo / deno differ

**Fetching dependencies does not execute their code.** Measured: `cargo fetch`
creates no `target/` (so `build.rs` never ran), `go mod download` leaves no
artifact in the project, and `deno install` creates no `node_modules`. These
providers therefore have no `--ignore-scripts` equivalent to pass -- build scripts
only become a concern at `cargo build`.

**All three have a real frozen mode, and cargo's is the strictest.**
`cargo fetch --locked` fails both with no lock and when the lock is merely *stale*,
whereas `uv sync --frozen` only promises not to update the lock and will quietly
install the old set. So "`--locked` means the same thing everywhere" is not a safe
assumption.

::: tip go's toolchain is pinned
`GOTOOLCHAIN` defaults to `auto`: when `go.mod` asks for a newer Go, go **downloads
another toolchain itself** (measured: it prints `go: downloading go1.99.0`). The
fetch would then run under a Go that osdk neither selected nor verified, so osdk
passes `GOTOOLCHAIN=local`. To use a newer Go, run `osdk install go@<version>`.
:::

### Not supported: bundler / composer

osdk has no ruby or php tool backend, so it cannot install bundler or composer
themselves. Listing these providers would produce "declared, detected, then failed
while installing the tool" -- worse than saying plainly that they are unsupported.
Supporting them requires adding the corresponding language backend first.

### Two things that differ on Python

**`uv` gets `--locked`, not just `--frozen`.** uv's `--frozen` only promises not
to update the lock; it does **not** check the lock against `pyproject.toml` --
measured, a stale lock exits 0 and installs the old set, with a newly added
dependency simply missing. Asserting the lock is current is `--locked`, so osdk
passes both. `npm ci` fails in that situation, so the ecosystems are not
symmetric.

**A `requirements.txt` is only reproducible when every line is pinned.** uv
installs an unpinned file happily (exit 0), so "it installed" says nothing about
reproducibility. osdk checks and says so:

```
warning: requirements.txt is not fully pinned, so this install is not
reproducible; pin every requirement (or use uv with a uv.lock) to make it so
```

Dependencies land in the project's own `.venv`, matching the same split as
everywhere else: application dependencies in the project, tools in isolated
directories. osdk also passes `UV_PYTHON_DOWNLOADS=never`, so the interpreter is
the one osdk resolved rather than one uv fetched on its own.

## How the installer is chosen

Highest priority first; nothing lower can take a project away from something
higher:

1. `packageManager` in `package.json` -- the project's own declaration wins.
2. A native lockfile already on disk -- whoever wrote it keeps managing it.
3. `[deps.<provider>].installer`.
4. The provider itself.

Two contradictions are refused rather than guessed: a declaration of `pnpm`
next to nothing but a `yarn.lock`, and two lockfiles of the same ecosystem in one
directory. Both name the specific files so you can decide which to keep.

A manifest that will not parse is also an error rather than a skip. Walking
silently past a broken `package.json` would install the wrong project's
dependencies, or none, and report success.

## Frozen installs

osdk looks for the native lockfile itself instead of passing a flag and assuming
it took effect:

- Lockfile present → a frozen install (`npm ci`, `pnpm --frozen-lockfile`,
  yarn berry `--immutable`, yarn classic and bun `--frozen-lockfile`).
- No lockfile → a plain install, plus a `warning:` line saying a lockfile will be
  created.

**Why this cannot be delegated**: `yarn@1.22.19` and `bun` exit 0 and install
normally with no lockfile at all, and `yarn@1` silently accepts berry's
`--immutable` while neither freezing nor blocking scripts. Passing the flag
without checking would be silently wrong on two of the four.

To require a lockfile in CI:

```bash
osdk deps --frozen
```

That turns the fallback above into an error instead of a warning it is easy to
miss.

## Monorepos: declare the sub-projects

osdk does **not** scan downward for projects. To manage several packages in a
monorepo, declare where they live under `[deps]`:

```toml
[deps]
roots = ["apps/*", "packages/*"]

[deps.npm]
```

Only matching directories are examined. **A package no pattern covers is not
found** -- even with a perfectly good manifest sitting right beside the others.
What can be acted on automatically has to be what was declared; otherwise
`osdk deps` installs dependencies for a package you never mentioned.

Each sub-project gets its own address, `//<path>:<provider>`. Listing shows **this
config root only** by default; `--all` expands the sub-projects:

```
$ osdk deps --list
npm                 stale  /repo

$ osdk deps --list --all --explain
npm                 stale  /repo
//apps/api:npm      stale  /repo/apps/api
    from root: apps/*
//apps/web:npm      stale  /repo/apps/web
    from root: apps/*
//packages/ui:npm   stale  /repo/packages/ui
    from root: packages/*
```

Only **listing** is tiered. A repository with dozens of packages scrolls the useful
part off screen, and that is a readability problem rather than a correctness one.
**Materializing is never tiered**: a bare `osdk deps` always covers every declared
root, and doing less by default would silently skip work.

Addressing one sub-project does not need `--all` either -- asking for something by
name and being told it does not exist would misreport the configuration:

```bash
osdk deps //apps/api:npm          # just this sub-project
osdk deps npm                     # every npm sub-project
osdk deps --skip //apps/web:npm   # all but one
osdk deps --list //apps/api:npm   # just this one, no --all needed
```

### Selecting by location: `--filter`

A provider name selects by **kind**; `--filter` selects by **location**:

```bash
osdk deps --filter 'apps/*'          # every sub-project under apps/, any manager
osdk deps --filter apps/api          # just that one
osdk deps --filter 'apps/*' --filter 'packages/ui'   # repeatable, union
osdk deps --list --filter 'apps/*'   # filtering implies the expansion; no --all
```

Patterns are the **same** dialect as `roots` (next section), not a second one: one
`*` never crosses a `/`, and `**` is unsupported. Two wildcard dialects in one
configuration is pure cognitive cost -- mise has `*` for declaring and `...` for
addressing, and pnpm still carries a `legacyDirFiltering` switch from changing its
mind about exactly this.

**Matching no sub-project is an error, not a quiet success:**

```
$ osdk deps --filter 'services/*'
error: `--filter` matched no sub-project: services/*
```

The opposite of pnpm, whose `failIfNoMatch` defaults to off. Same reasoning as
`--verify` refusing to report an environment it could not examine as clean:
**"nothing was done" must not read like "done, no problems"**. A CI step narrowed to a
directory that has since been renamed should fail, not pass having built nothing.

### How patterns match

- `*` (any characters) and `?` (one character), matched **segment by segment**.
- **No `**`**: that would turn a declared root back into an arbitrary subtree
  walk, which is the thing this feature exists to avoid. Write the depth out
  instead, e.g. `apps/*/*`.
- A `*` never crosses `/`, so `apps/*` does not reach `apps/group/nested`.
- `..` is refused: a committed config should not be able to reach outside the
  project.
- Matching nothing is not an error -- `apps/*` in a repo with no `apps` yet is a
  forward-looking declaration.

Sub-projects found through a root follow the same rules as a top-level one: a
broken manifest is an **error**, not a skip. Installing some of the packages and
reporting success is the failure mode hardest to notice.

## Custom providers

Beyond the built-in package managers, any other name under `[deps]` is a custom
step: a command of your own, with its inputs and its outputs. The shape mirrors
`[tasks]`.

```toml
[deps.codegen]
sources = ["schema/schema.graphql"]   # re-runs when these change
outputs = ["src/generated"]           # missing means stale
run = "pnpm run codegen"
depends = ["pnpm"]                    # wait for pnpm to install first
dir = "apps/api"                      # optional: run in a subdirectory
env = { NODE_ENV = "development" }
```

A custom provider needs no manifest -- **the declaration is the detection**. Its
root is the directory of the `osdk.toml` that declared it, so `sources` and
`outputs` are relative to the same place a built-in provider's would be.

`run` does **not** go through a shell: the command is split on whitespace and
executed directly. Otherwise the same `run` line would mean different things on
different machines, and this string gets committed. Anything needing pipes or
redirection belongs in a script that `run` invokes.

`depends` decides the order and osdk sorts accordingly (declaration order is
irrelevant). A cycle is an error: picking some order anyway would run a step before
its input existed, and the failure would point at the wrong provider.

::: warning Custom providers always need approval
`run` is an arbitrary command, so it **always** requires approving the config --
unlike a built-in provider, where declaring what to install does not. It also
differs from `[tasks]`: a task is triggered by you explicitly running
`osdk run <name>`, and that invocation is the authorization, whereas deps can be
triggered ahead of time by `auto`.
:::

## When the package manager is not installed

`deps` installs it, through the same tool install path `osdk install` uses -- so
source selection, verification and the CAS are identical, and there is no second
installer to keep honest.

The tool lands in osdk's isolated directories, **not** in your project. `deps`
puts dependencies in the project; the package manager that installs them is a
tool.

```bash
$ osdk deps
installing node for deps provider `npm`
installing node@26.10.0 ...installed node@26.10.0
npm.cmd install --ignore-scripts
added 2 packages in 827ms
```

In CI you usually want tools to come from an explicit `osdk install`, so that a
run cannot quietly acquire a different version. `--no-install-tools` turns the
acquisition off -- it forbids *acquiring*, not using one you installed yourself:

```bash
osdk deps --no-install-tools
```

It fails when a tool is missing, naming the command that would fix it.

## Build scripts are off by default

A dependency's `preinstall` / `install` / `postinstall` hooks do not run.
Declaring which packages to install cannot execute anything the publisher did not
ship as plain files, so the declaration itself needs no approval.

Turning them on does need approval, because that is the point at which arbitrary
code runs on your machine:

```toml
[deps.pnpm]
allow_build_from_source = true
```

## Changing the registry

```toml
[deps.pnpm]
index = "https://registry.example.com/"
```

Redirecting where bytes come from requires approving the config -- because the
source changed, not because anything executes. Achieving the same thing through
`env` (for instance `NPM_CONFIG_REGISTRY`) requires approval too: a gate one
spelling walks around is not a gate.

## Full configuration

```toml
[deps]
disable = ["npm"]           # off here even if a broader layer enabled it

[deps.pnpm]
auto = true                 # the default: materialize ahead of install/run/exec
sources = ["package.json"]  # files that decide freshness (replaces the default)
outputs = ["node_modules"]  # missing means stale (replaces the default)
dir = "apps/api"            # run in a subdirectory
depends = ["npm"]           # materialize another provider first
timeout = "10m"
installer = "pnpm"
index = "https://registry.npmjs.org/"
allow_build_from_source = false
env = { CI = "1" }
```

## Checking an installed environment: `osdk deps --verify`

Freshness answers "did the inputs change" and **cannot see** that something else
edited `node_modules` or `site-packages` -- the hash is over the inputs.
`--verify` reads the receipts the package managers write **themselves** to answer
a different question: is what is installed still what was installed?

```bash
osdk deps --verify
```

Two layers, cheapest first:

- **L1**: is the native lockfile's sha256 still the one recorded in `osdk.lock`?
  This catches a drift freshness structurally cannot -- the lock is untouched, so
  the hash matches, but the environment was rebuilt by something else.
- **L2**: does every entry in the receipt still exist, at the recorded size and
  digest? Which receipt depends on the package manager:

  | package manager | receipt read | granularity |
  | --- | --- | --- |
  | pip / uv | `dist-info/RECORD` | per-file size + sha256 |
  | npm | `node_modules/.package-lock.json` | per-package version + integrity |
  | pnpm | the store's `*-index.json` | per-file size + sha512 |
  | yarn | not supported, reported as such | — |

  pnpm's receipt is finer than npm's: it is **per-file**, on par with Python's.
  pnpm writes no `.package-lock.json`, and `.modules.yaml` holds layout metadata
  only; the per-file digests live in the content-addressable store's index, whose
  address happens to be derivable from the integrity recorded in
  `pnpm-lock.yaml`. Verifying a pnpm project therefore needs both halves: the
  lockfile in the project and the store on this machine. For a checkout from
  another machine, or after the store has been pruned, `--verify` says there is no
  receipt to read rather than reporting everything fine.

  yarn is deliberately out of scope: Berry's PnP keeps dependencies in a single
  zip-backed store with no per-package tree to compare against. Saying so is better
  than adding a predicate that would pass whatever the environment looked like.

The exit code is non-zero when anything is wrong, so this works as a CI gate. The
output also reports how much was examined: "0 problems" and "nothing was checked"
must not read the same, so an environment with no receipt to read is **reported as
an error rather than passed**.

Kinds of tampering measured as detectable: deleting a file inside an installed
package, changing a file's contents, deleting a whole installed package, and
swapping a package's `version` in place. That last-but-one is the sneakiest -- the
directory is there, the file count is right, only the version disagrees.

On pnpm one more is caught that npm's receipt cannot see: **an edit that keeps the
file's length**. With per-file digests, flipping a single byte is reported; a
receipt that only goes down to the package notices nothing.

::: warning Do not just re-run the installer after a failure
`uv pip sync` was measured **not** to repair a modified file, and the tampered
content may already be in the tool's global cache (one modified file made every
newly created venv copy the bad version from cache). Clear the cache and
reinstall.
:::

## Freshness

`deps` records a hash of the inputs of the last successful run and compares it
next time. The hash covers the contents of `sources` **and the command that
actually ran**, environment included -- yarn berry disables build scripts through
`YARN_ENABLE_SCRIPTS`, so leaving env out would hash two materially different
runs the same.

State lives in osdk's cache directory, never in your project. `--force` runs
regardless of the decision.

Undeclared `sources`, or declared sources that match no files at all, never count
as fresh: a predicate that matches nothing is vacuously true, which would dress
up "nothing was checked" as "nothing changed".
