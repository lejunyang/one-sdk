# Application Dependency Implementation

This page covers how `osdk deps` is put together and why a few of its choices are
what they are. For usage, see [application dependencies](../deps.md).

## Why a separate subsystem

osdk only ever had one semantic: install *a tool*. That holds even where it goes
deepest -- the `npm:<pkg>` backend installs a tool's own full dependency tree,
while the project-level orchestration in `npm_tools.rs` adds *one* package to
`package.json` and installs it. The granularity is **per dependency**, not "read
a whole manifest and materialize an application environment once".

`deps` supplies that second thing. It does not redo what package managers already
do well (resolution, download, writing a lockfile). It takes over only the
decisions osdk is better placed to own: which installer, whether a frozen install
is possible, registry policy, reading the result back, recording the identity in
the lock.

## Module layout

```
crates/osdk-core/src/deps/
├── mod.rs      common layer: provider table, discover, select_installer, plan
├── node.rs     the four Node providers, command construction, yarn line split
└── state.rs    freshness state and decisions
crates/osdk-cli/src/deps_cmd.rs   CLI orchestration and reporting
```

The whole `deps` module sits behind the `install` feature (`lib.rs` has
`#[cfg(feature = "install")] pub mod deps;`). The shim never materializes a
dependency closure, so none of this code is in its build. Measured: the shim grew
by 0.05%, which is noise.

For the same reason `deps` **adds no method to the `Backend` trait**.
`Registry::new()` instantiates every backend as `Arc<dyn Backend>`, so each trait
method enters a vtable the linker cannot prove unreachable -- that once cost the
shim 5.15 MB. Real binary paths come from the existing `Backend::bin_paths`.

## Why not through the shim

The first idea was to invoke package managers through osdk's shims. Measured, it
does not work:

```
$ <data>/shims/pnpm.cmd install
osdk-shim: no version of 'pnpm' selected (set one with 'osdk use pnpm@<version>')
```

A shim requires a version already selected for the current directory, and the
typical `deps` situation is precisely "pnpm was just installed and the project has
never run `use`". So `deps` resolves real binaries via `Backend::bin_paths` and
**prepends** node's bin directory to the child process PATH -- the latter is
required, because dependency scripts call `node` directly.

## osdk owns the frozen decision

This is the subsystem's most consequential design choice, and it comes from
measuring all four installers (versions below):

| installer | "frozen" command with no lockfile |
| --- | --- |
| npm 10.9.8 | exit=1, `EUSAGE` |
| pnpm 12.5.1 | exit=1, `ERR_PNPM_NO_LOCKFILE` |
| yarn berry 4.6.0 | exit=1, `YN0028` |
| **yarn classic 1.22.19** | **exit=0, installs anyway, writes no lock** |
| **bun 1.4.2** | **exit=0, installs anyway, writes no lock** |

The last two rows are the whole reason: delegating the frozen decision would be
**silently wrong** on two of four -- the command succeeds, there is no warning,
it simply did not freeze. And `yarn@1` additionally accepts berry's `--immutable`
while neither freezing nor blocking scripts, the worst failure shape of the set
because it looks like it worked.

So osdk checks for the lockfile itself and picks the arguments from that, and
when it has to fall back it **says so** instead of downgrading silently.
`--frozen` turns that fallback into an error.

## yarn must be dispatched on its major

Classic and berry differ on both flags that matter, and not merely by name: one
silently accepts the other's flag and ignores it.

| | freeze | block scripts |
| --- | --- | --- |
| classic 1.x | `--frozen-lockfile` | `--ignore-scripts` |
| berry 2+ | `--immutable` | `YARN_ENABLE_SCRIPTS=false` |

Berry 4.6.0 **rejects** `--ignore-scripts` with
`Unknown Syntax Error: Unsupported option name` (its `install` accepts only
`--json`, `--immutable`, `--immutable-cache`, `--refresh-lockfile`,
`--check-cache`, `--check-resolutions`, `--inline-builds`, `--mode`), so blocking
scripts there has to go through the environment.

The version comes from the `packageManager` field. When it is absent, berry is
assumed -- berry **fails loudly** without a lockfile while classic continues
silently, so the cost of guessing wrong is asymmetric.

One related measurement: pnpm 12.5.1 and bun already block dependency build
scripts by default (which is why pnpm 12 reports `ERR_PNPM_IGNORED_BUILDS` and
suggests `pnpm approve-builds`). osdk still passes `--ignore-scripts` explicitly
rather than relying on one version's default.

## Deep verification: predicates chosen by measurement, not by plausibility

The design originally proposed L2 as "`pyvenv.cfg` creator/interpreter still the
recorded one" plus "key package metadata present". Each candidate was tested
against real tampering before any of it was implemented, and both of those were
**dropped**:

| Tampering | pyvenv.cfg | dist-info present | **per-file RECORD** |
| --- | --- | --- | --- |
| delete a file inside a package | missed | missed | **found** |
| change a file's contents | missed | missed | **found** |

The reason is plain: they describe **how the environment was created**, while
tampering changes **what is in it**. That is exactly AGENTS.md's "assertion
detached from the mechanism under test" -- keeping them would let `--verify` pass
an environment that is already broken, which is worse than having no layer at all.

What survived is the receipt each package manager writes **itself**: Python's
`dist-info/RECORD` (measured: 14 of idna 3.10's 15 lines carry size and sha256)
and Node's `node_modules/.package-lock.json` (per-package version and integrity).
osdk reads those instead of keeping a second dependency graph -- the same
judgement as "the native lock is the source of truth".

Size is compared rather than sha256 because it catches the same class of tampering
(measured: appending one line took 13239 to 13251) at a fraction of the cost, and
a verification too slow to be run protects nothing. The hash is still in RECORD if
a `--verify --deep` ever needs it.

### What the reverse control exposed

"A clean environment must pass everything" **failed** the first time: a freshly
created venv still reported one size mismatch. The cause was not a false positive
in the predicate -- it was that **`uv` had written the tampered file into its
global cache**, so every new venv copied the bad version from there. A fresh
`UV_CACHE_DIR` brought it to zero.

Two consequences. Methodologically this is AGENTS.md's "reused polluted state"
family: had "even a clean environment reports an error" been read as an unreliable
predicate, L2 would have been abandoned on exactly inverted evidence. And as
product behaviour, it means **`uv pip sync` does not verify the contents of
already-installed files** (measured: the modified `core.py` still carried its edit
after a sync), so "just run it again" is not a fix. That is why a `--verify`
failure recommends clearing the cache and reinstalling.

### "Could not check" is not "checked and fine"

With no receipt to read, L2 reports `ReceiptMissing` rather than returning an
empty clean report. `checked == 0` with no findings would be a vacuous pass,
dressing up "nothing was examined" as "nothing is wrong". The report therefore
states how many entries were checked.

`--verify` short-circuits before anything is installed: it is a check, not a
command that changes the environment.

## Python: two asymmetries with Node

Both measured on 2026-09-22 (uv 0.12.17 / CPython 3.12.14), by the same judgement
as the Node round: an sdist-only package whose `setup.py` writes a marker file, so
"did a source build happen" is answered by the marker, not by the exit code.

**One: denying source builds requires a flag, not an environment variable.**
`UV_NO_BUILD=1` is **silently ignored** by `uv pip install` (exit 0, marker
present, the source built anyway); only `uv sync` honours it. In
`uv pip install --help`, `--no-build` carries no `[env:]` annotation while
`--no-build-isolation` on the same page does -- the same variable name behaves
differently across subcommands.

This is the **exact inverse** of yarn, where berry rejects `--ignore-scripts` and
only `YARN_ENABLE_SCRIPTS=false` works. So the Node lesson that "env is an
equivalent channel" does **not** carry over. Deriving it by symmetry would have
produced a switch that looks enabled while every sdist keeps building locally.

**Two: `--frozen` does not mean "the lock is current".** With a lock that
disagrees with `pyproject.toml`, `uv sync --frozen` still exits 0 and installs the
old set, the newly added dependency missing, because its only promise is not to
update the lock. Checking consistency is `--locked`. `npm ci` fails in that same
situation, so osdk passes both flags to uv.

## The prelude, and why a "before" step exists

`uv sync` creates the project environment itself; `uv pip sync` refuses without
one (`No virtual environment found`). Papering over that asymmetry with an
implicit `uv venv` inside the runner would hide it from `--dry-run` and from the
freshness hash.

So `RunPlan` carries `prelude: Vec<Vec<String>>`: same program, same cwd, same
env, run first and in order. It appears in the printed command string and
therefore in the hash -- otherwise "create the environment, then sync" and "sync
into whatever is already there" would hash identically.

The prelude has to be **idempotent**: plain `uv venv` exits 2 (`Failed to create
virtual environment`) once one exists, so every run after the first would fail
before reaching the sync. `--allow-existing` reuses it, which is also the correct
behaviour -- `uv pip sync` is what makes the contents match the file, so
recreating the environment would only discard a cache.

## `[deps.<p>].dir` was declared but did nothing

`dir` had been in the schema and the docs all along, but the providers used
`project.root` directly and the setting was ignored entirely. **A declared setting
that does nothing is worse than an absent one**: the project looks configured while
the command runs somewhere else.

Both providers now share `effective_cwd`, and `dir` is read **accepting either
separator** -- it comes from a committed `osdk.toml`, the machine that wrote it is
not necessarily the one reading it, and `Path::components()` only understands the
host's own separator. Its test exercises both spellings on every platform, with no
`#[cfg(windows)]`: adding one would declare that the other half is never verified,
which is exactly where this class of bug hides.

## Acquiring tools is delegated

When a package manager is missing, `deps` calls `install_one_without_shims` --
the same path `osdk install` uses. The reason not to build a second one is
concrete: that path already carries source selection, verification, attestation
and the CAS, so an independent implementation would have to duplicate all of it
to be equally trustworthy, and the duplicate is the copy that rots.

Versions come from different places per role. An installer's version can be
pinned by the manifest's `packageManager` field; a runtime's cannot, so it comes
from `[tools]`, falling back to whatever is already installed.

`--no-install-tools` draws its line at **acquiring**, not **using**: a tool the
user installed works as normal, a missing one is an error naming the command that
fixes it. CI needs that distinction -- tools should come from an explicit, reviewed
`osdk install` rather than whatever a given run happened to fetch.

## PATH is prepended, not appended

`run_plan` puts the resolved bin directories at the *front* of PATH. This is not
a style choice: npm and pnpm are themselves node scripts, and dependency
lifecycle hooks invoke `node` directly, so an unrelated node earlier on PATH would
win and the install would run under a runtime osdk did not select -- while looking
entirely normal.

Program lookup searches the installer's own directories first and then all of
them. The second pass is required because npm ships with node and lives in
node's bin directory rather than its own.

## Recording happens only after success

`record` runs only after the command returns successfully, and that ordering is
load-bearing. Moving it ahead of the run would not break the current run -- that
one still fails loudly -- but the *next* one would report "up to date" for a tree
that was never populated, converting one visible failure into a silently broken
working tree.

The lock's `native_lock` section digests the file as it exists **after** the
install, so it describes what was actually consumed rather than what was intended.
On a first install no lockfile exists yet, so the section is absent; the next run
adds it, which is also the run where the command changes from `npm install` to
`npm ci`.

## Discovery is fail-closed

`discover()` walks upward from the current directory, nearest wins, and **does
not recurse downward** -- a blind subtree scan would drag unrelated packages in
from a monorepo.

A manifest that exists but is not a regular file, or will not parse, is an error
rather than a skip. Walking silently past a broken `package.json` produces
"installed the wrong project, or nothing, and reported success".

`select_installer()` keeps the priority order `npm_tools.rs` already established
in `select_automatic_installer`: declaration → existing lock → project config →
default. Both contradictions (a declaration disagreeing with the lockfile's owner,
two lockfiles of one ecosystem) name the specific files and then refuse.

## Trust, in three tiers

This follows the philosophy `trust.rs` states about itself: a dependency
declaration cannot execute anything the publisher did not ship as plain files, so
the declaration must not be gated -- "re-approving for every added package
teaches the user nothing".

| Case | Approval | Why |
| --- | --- | --- |
| `[deps.pnpm]`, `sources`, `outputs`, `auto`, `dir`, `depends`, `installer` | No | Scripts are off by default; this only declares what to install |
| `index` / `extra_index` / `registry` / `insecure` | Yes, `WeakensVerification` | The source of the bytes changed -- **not** code execution |
| `env` variables named `*_REGISTRY`, `*REGISTRY_SERVER`, `*_INDEX_URL`, `*_DEFAULT_INDEX`, or containing `:REGISTRY` | Yes, `WeakensVerification` | Equivalent to the row above; cannot be left out |
| `allow_build_from_source` | Yes, `ExecutesCode` | Arbitrary code really does run locally |
| A custom provider's `run` | Yes, `ExecutesCode` | Arbitrary command, and `auto` can trigger it |

The third row closes a **real hole** found during implementation: the first draft
inspected only a provider's direct keys, so `index` demanded approval while
`env = { NPM_CONFIG_REGISTRY = "…" }` achieved the same redirect for free. A gate
one spelling walks around is not a gate.

The match is by **suffix**, not prefix. AGENTS.md records what a too-wide prefix
costs (`UV_INDEX_` sweeps in `index_strategy`, `NPM_CONFIG_FUND`, and other
unrelated settings), and the damage in that direction is **invisible** -- a
needless approval prompt, or a feature quietly switched off. So both directions
are tested: names that should match, and names that should not.

A `deps` trust requirement **must not affect tool dispatch**. In
`affects_tool_dispatch`, `deps` returns `false` alongside `syspkg`,
`task_config`, and `models`. Otherwise a project that merely declares a provider
could not run `cargo --version` -- exactly the `[syspkg]` accident.

## Freshness

Same shape as `tasks::freshness`: the input hash goes to
`<cache>/deps/<first 16 of blake3>.toml`, never into the project.

The hash covers the contents of `sources` **and the effective command string,
environment included**. Env has to be in it because yarn berry blocks scripts
through `YARN_ENABLE_SCRIPTS` -- leaving it out would hash a scripts-blocked and
a scripts-allowed run, two materially different things, identically.

Undeclared `sources`, or declared sources matching zero files, never count as
fresh. `tasks/freshness.rs` records this vacuous-truth trap: a predicate matching
nothing is trivially satisfied, dressing up "nothing was checked" as "nothing
changed".

## What the lock records

The `[deps.<provider>]` section records what another machine needs to reproduce
*the same* install: the installer and its version, the runtime that drove it, the
manifest path and sha256, the native lockfile's kind/path/sha256, the effective
command, and the registry.

Deliberately **not** recorded: absolute paths (one machine's locations), the
contents of `node_modules` (that is the native lockfile's job -- osdk does not
keep a second dependency graph), and credentials.

Relative paths written into the lock are normalized to `/`, because the value is
committed and read on other platforms. The read side accepts both separators via
`split(['/', '\\'])`.

The section carries `skip_serializing_if`, so it is absent when empty: existing
locks serialize byte-identically and an older binary does not trip over an extra
table. The read-back path is covered by a test, because AGENTS.md records that
`pypi` once wrote an `installer` nothing read back, so re-locking silently
overwrote the promise the lock had recorded.

## Measured versions

Every claim above about external tool behaviour was measured on Windows x64 on
2026-09-22: node v22.23.2, npm 10.9.8, pnpm 9.15.1 and 12.5.1, yarn 1.22.19 and
4.6.0 (via corepack 0.34.6), bun 1.4.2.

Whether a script actually ran was determined by having it append a line to a
marker file and then reading that file -- not by the exit code, which being 0
proves nothing about scripts not running.
