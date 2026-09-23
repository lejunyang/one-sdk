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

## Listing is tiered, materializing is not

`--list` shows the current config root; `--all` expands `[deps].roots`. The asymmetry
is deliberate, and it cannot be reversed.

Listing is a **readability** problem: in a repository with dozens of packages, the
full set scrolls "what is the state of the layer I am in" off the screen. mise makes
the same trade with `mise tasks` versus `mise tasks --all` (the former covers the
current config_root and its parents only), which is worth noting as corroboration.

Materializing is a **correctness** problem: a bare `osdk deps` has covered every
declared root since day one. Doing less by default presents as "the command
succeeded but one sub-project's dependencies were never installed" -- no error, no
warning, just some later run failing inside an incomplete environment. That is the
failure mode this codebase keeps rejecting: worse than an error, because it looks
like success.

So `wants_rooted` defaults to expanding, and only a plain `--list` does not. Two
cases still expand:

- `--list --all`, which is what the flag is for.
- A listing with a rooted operand (`//apps/api:uv`). Asking for a sub-project by
  name and being told it does not exist misreports the configuration; it is not a
  terser output.

The test was **mutated in both directions**: making `wants_rooted` always true turns
the "a plain `--list` must not expand" assertion red, and making it false for every
listing turns the `--all` and rooted-operand assertions red. Testing one direction
only would let an implementation that never expands pass the first half.

One more thing the test recorded: `--dry-run` prints the **directory path**, not the
rooted id, so proving "materializing covered the sub-projects" means matching paths
and counting `would run in` occurrences. The first version matched
`//apps/api:npm` and failed for a reason unrelated to the property under test --
output format, not coverage.

The tiering has one side effect worth stating plainly: **a plain `--list` no longer
expands roots, so a broken manifest in a sub-project goes unnoticed during a bare
listing**. Fail-closed itself did not loosen -- materializing, `--list --all`, and
naming a rooted id all still error out on it. But "just list it and see" no longer
doubles as a whole-repository check; ask for `--all` when that is what you want. An
existing test caught this during wiring:
`a_broken_manifest_inside_a_root_fails_closed` reaches the broken sibling only
through `--list`, so it now has to request expansion explicitly.

## Monorepo roots: matched segment by segment, not scan-then-filter

`discover_in_roots` descends one pattern segment at a time: a literal segment is a
`join`, and only a wildcard segment causes a `read_dir` of **that one level**. The
results may coincide with "walk the subtree, then filter by pattern", but the
properties do not: the latter has to read every directory to decide, and one
missing filter condition turns it silently into a whole-repository crawl.
Descending by segment makes "only look where it was declared" structural, rather
than dependent on a filter branch always being right.

`**` is not supported. It would turn a declared root back into an arbitrary
subtree walk, which is the thing this feature exists to prevent, so it is absent
rather than present-and-restricted. Write the depth out (`apps/*/*`).

`glob_matches` refuses any name containing a separator. Every current caller
passes a single directory name, so this changes nothing today -- but a matcher that
*can* span a separator means the next caller to pass a multi-segment string
silently gets the subtree crawl. That guarantee belongs in the function, not in
every caller remembering.

### An "injection that stayed green", and what it improved

The first injection against the core invariant **did not fail**. Two reasons, both
worth recording:

1. My initial injection routed literal segments through the wildcard branch. That
   is **not a defect**: `glob_matches("apps", name)` still accepts only `apps`, so
   the change merely reads a directory to confirm what `join` already knew.
2. The load-bearing branch is the `glob_matches` call. But replacing *that* with
   `true` stayed green too, because every fixture used `apps/*`, whose only
   wildcard segment is a bare `*` -- which **should** match every directory. So
   "accept any name" and "match the pattern" produced identical results, and the
   filter was never actually under test.

A test using `api-*` was added (`api-v1`/`api-v2` must match, the adjacent
`web-v1` must not), and replacing `glob_matches` with `true` now turns it red.
This is a concrete instance of AGENTS.md's "the probe must fall inside the
mechanism under test": **a fixture that only ever uses a bare `*` cannot test a
wildcard filter.**

## Custom providers: three fields became Cow, not everything became String

A custom provider's name comes from config, so it is a runtime `String`, while the
built-in table is static `&'static str`. Widening everything to `String` would make
every built-in entry allocate, across a 40-site diff.

Only three fields actually carry a provider name: `DetectedProject::provider`,
`InstallerChoice::provider` and `RunPlan::tool`. Those three became
`Cow<'static, str>` -- `Borrowed` and allocation-free on the built-in path, `Owned`
for a custom one. The static schema tables are untouched.

`Resolved::schema` became an `Option` accordingly. Not to dodge the type system:
**not having a built-in schema is precisely what makes a provider custom**, so
`Option` states the fact more directly than a sentinel or an error would. A custom
provider's `sources`/`outputs` therefore come straight from what was declared --
there are no defaults to fall back to. Declaring none means freshness cannot be
established, so it runs every time; not "always fresh", which would be the
vacuous-truth trap.

A custom provider's `outputs` are treated as **required**: writing them down is a
claim that the step produces them, so their absence means the step did not deliver
what it promised. Built-in providers keep their own mix of required and
optional-once-seen.

## `run` does not go through a shell

The command is split on whitespace and executed directly. Handing it to `cmd.exe`
or `sh` would make one `run` line mean different things on different machines --
and this string is committed -- while quoting rules would become a portability
hazard. Anything needing shell features belongs in a script that `run` invokes.

Program resolution takes a different path for a custom provider: it brings no tools
of its own, so there are no install directories to search. The name goes to the OS
for a PATH lookup, and the PATH the child receives is "resolved tools first, then
the inherited one", so whatever `depends` installed is found.

## depends decides order, and a cycle is an error

`order_by_depends` topologically sorts. A cycle is refused rather than resolved
arbitrarily: choosing an order would run a step before its input existed, and the
failure would name the wrong provider. Dependencies pointing at providers that are
not configured are ignored -- that is a no-op rather than a contradiction, since a
disabled provider has nothing to wait for.

## One "injection that stayed green" exposed a code problem, not a test problem

`plan_custom` originally had two guards: `command.is_empty()`, and then
`parts.next()` returning `None`. Injecting a fault into either produced **no
observable change**, because the other still rejected.

That is not a weak test -- it means **a guard whose removal is invisible cannot be
trusted to be there**. It was collapsed into a single check, and the injection was
then confirmed to turn it red.

In the same spirit, `plan`'s ecosystem dispatch lost its catch-all arm: every
ecosystem now has an implementation, so adding one should fail **at compile time**
rather than print "not implemented yet" at runtime.

## go / cargo / deno: why there is no script-suppressing flag

Node and Python both need build scripts explicitly turned off. These three do not
-- not as an omission, but because **the fetch step has no such hook**. Measured
(go 1.27.1 / cargo 1.98.0 / deno 2.9.6):

- After `cargo fetch`, `target/` **does not exist**, so `build.rs` never ran; build
  scripts are a `cargo build` concern.
- `go mod download` leaves no artifact in the project.
- `deno install` creates no `node_modules`.

So adding an `--ignore-scripts`-style argument "for symmetry with npm" would either
be rejected by the tool or silently do nothing -- both worse than not passing it.
The `no_script_suppressing_flag_is_invented` test guards exactly that.

All three have a **real** frozen mode, in contrast with the Node round (yarn classic
and bun exit 0 and install anyway with no lock). And **cargo's `--locked` is the
strictest freeze in this subsystem**: it exits 101 even when the lock is merely
stale, where `uv sync --frozen` quietly installs the old set. That makes
"`--locked` means the same thing across ecosystems" a wrong assumption -- each one
has to be measured separately.

### GOTOOLCHAIN and UV_PYTHON_DOWNLOADS are the same problem

`GOTOOLCHAIN` defaults to `auto`: when `go.mod` asks for a newer Go, go goes and
fetches a different toolchain (measured: `go: downloading go1.99.0`; it failed here
only because that version does not exist, but **the attempt itself happened**). The
fetch would then run under a Go that osdk neither selected nor verified -- exactly
isomorphic to uv downloading an interpreter. Both are pinned explicitly:
`GOTOOLCHAIN=local` and `UV_PYTHON_DOWNLOADS=never`.

`CARGO_HOME` needs no new convention: `backend/rust.rs:39-40` already points it at
`<data>/cargo`, so deps inherits that and does not mix with the user's own
`~/.cargo`.

### One test that was discarded

The first version asserted "a `go.mod` is not parsed as a Node manifest". It could
**never fail** -- `node::declared_manager` returns early when the ecosystem is not
Node, so a misrouted `go.mod` was harmless. That is a probe outside the mechanism it
claimed to cover: injecting the catch-all dispatch left it green.

It was replaced with "these manifests are validated, and a broken one is not
skipped", and that one was confirmed to fail when validation is removed. The
dispatch was still made **exhaustive, with no `_` arm**: having the compiler demand
an arm is cheaper than learning about a missed wiring from a user.

### bundler / composer being unsupported is a hard constraint, not a preference

`backend/registry.rs:21-36` has no ruby and no php, so D2's "install the missing
package manager" chain cannot work for those ecosystems. Listing a provider whose
precondition osdk cannot meet would give the user "declared, detected, failed while
installing the tool" -- worse than saying plainly that it is unsupported. Support
requires adding the language backend first, which is a separate piece of work.

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
`dist-info/RECORD` (measured: 14 of idna 3.10's 15 lines carry size and sha256),
npm's `node_modules/.package-lock.json` (per-package version and integrity), and
pnpm's store index (per-file size and sha512 -- see below). osdk reads those
instead of keeping a second dependency graph -- the same judgement as "the native
lock is the source of truth".

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

### pnpm: the receipt is in the store, and its address comes from the lock

**Nothing** about pnpm's layout resembles npm's, so how to read it was measured
rather than inferred (pnpm 9.15.1):

- There is **no** `node_modules/.package-lock.json`.
- `node_modules/.modules.yaml` exists but carries layout metadata only --
  `storeDir`, `virtualStoreDir`, `nodeLinker` -- and **nothing per package**. It is
  a locator, not a receipt.
- `node_modules/<pkg>` is a junction (Windows) or symlink into
  `node_modules/.pnpm/<name>@<version>/node_modules/<name>`, whose files are in
  turn hardlinks into the content-addressable store.
- The integrity in `pnpm-lock.yaml` is the **tarball** digest, which cannot be
  recomputed from an unpacked tree.

That very nearly produced the conclusion that pnpm has nothing checkable offline.
The real receipt is in the store: `files/<xx>/<...>-index.json`, carrying
**per-file** `integrity` (sha512), `size` and `mode`. Measured on ms@2.1.3's four
files, every sha512 reproduces the bytes on disk exactly.

The key point is that the index does **not** have to be searched for: base64-decode
the tarball integrity, render it as hex, and the first byte is the subdirectory
while the rest is the filename stem. Measured: `sha512-6Flzub...` maps exactly onto
`files/e8/5973b9...-index.json`. That mapping has a test of its own, using the real
path pnpm wrote -- otherwise changing the prefix from one byte to two would go
unnoticed by every test.

This brings a limitation worth stating plainly: **verification needs both halves**
-- the project's lockfile for the addresses, this machine's store for the digests.
For a checkout from another machine, or after a prune, the only honest answer is
`ReceiptMissing`.

In exchange, pnpm's receipt is **stronger** than npm's. npm only goes down to the
package, so an edit that preserves a file's length is invisible to it; pnpm has a
digest per file, so flipping one byte is reported. That has its own test, because a
test that only checks "changed contents are caught" would pass on the size
comparison alone, leaving the digest check deletable without anything noticing.

Dispatch order is load-bearing too: look for `.pnpm` first, fall back to npm. An
npm install followed by a pnpm one leaves the old `.package-lock.json` in place, and
reading it would verify a tree that is **no longer the one installed** -- silently.

yarn stays unsupported: Berry's PnP keeps dependencies in a single zip-backed store
with no per-package tree to compare against. Adding a predicate that would pass
whatever the environment looked like is worse than admitting the gap -- the same
reasoning that retired the `pyvenv.cfg` candidates at the top of this section.

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

## The auto gate: freshness only, never a deep scan

`auto` defaults to `true` and covers a bare `install`, `run` and `exec`. That
default rests on a measured premise: the freshness check is cheap enough to sit in
front of every command.

Measured (via .NET SHA256, a conservative upper bound -- blake3 is faster): 0.16ms
for a 20KiB lock, 0.39ms for 200KiB, 2.06ms for a 2MiB monorepo lock. On a hit,
nothing else happens -- no package manager started, no directory listed, no
installed file read.

That draws a hard line: **the auto gate never runs `--verify`**. Deep verification
reads the package manager's own receipts entry by entry (Python's
`dist-info/RECORD` with per-file size and sha256, Node's
`node_modules/.package-lock.json` with per-package integrity), which takes seconds.
Putting a seconds-long scan in front of every `osdk run` does not produce "safer";
it produces users who switch the mechanism off. So `materialize_auto` passes
`verify: false` and `--verify` stays explicit.

Nothing was holding that constraint at first. Mutating the auto path to
`verify: true` left the whole suite **green** -- the most expensive promise in the
feature had no guard at all. The fix was to lift the option set out of
`materialize_auto` into `auto_options()`: it goes from "a literal buried inside one
call" to "a value a test can read", and its four assertions (no verify, no force,
auto-only, tools allowed) each fail under the matching mutation.

The three valves live in one predicate, `wants_auto_deps`, rather than scattered
across match arms:

| case | why it does not trigger |
| --- | --- |
| `osdk install node@22` | an operand means "install this tool"; also rewriting the dependency tree is a side effect nobody asked for -- the same reasoning behind explicit operands skipping lock replay |
| `--no-deps` | a single-invocation escape hatch, present on all three entry points |
| `run --dry-run` | having no effects is the entire point of the flag |

`auto = false` disables only automatic runs, not an explicit `osdk deps` -- naming
the command is itself the opt-in. Conversely, `auto` is not a trust matter: the
timing changed, not the danger of what gets installed, so the tiers stay as they
were (no trust by default, custom index as `WeakensVerification`, opting into
source builds as `ExecutesCode`), and a custom provider's `run` remains
`ExecutesCode` unconditionally.

`DepsProviderEntry`'s `Default` is written out rather than derived, because `derive`
would give `auto: false` while the field's serde default is `true`. When those
disagree, an entry built in code does not mean what a parsed one means -- and that
divergence stays invisible until some test constructs an entry and draws a false
conclusion about real config from it.

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
