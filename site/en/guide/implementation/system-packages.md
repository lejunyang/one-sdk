# How system package managers are inspected

`osdk pkg` deals with the host's own package managers — winget, Homebrew. These
are unlike SDKs: osdk does not own them, it can only ask. This page explains why
the subsystem is shaped the way it is.

## Why this is not a backend

The obvious approach is a `Backend` implementation for winget. It would cost far
more than it returns.

`Registry::new()` instantiates every backend as an `Arc<dyn Backend>`, so each
trait method lands in the vtable where the linker cannot prove it unreachable
and therefore cannot strip it. This once cost `osdk-shim` 5.15 MB for nothing:
the same 13 backends measured 7.02 MB through `dyn` dispatch and 1.83 MB
statically dispatched.

The deeper mismatch is conceptual. A backend is an SDK osdk installs and
switches versions of; a system package manager is host infrastructure osdk can
only query. The `container/` subsystem reached the same conclusion for Docker,
containerd, and BuildKit, and deliberately stays out of the trait. `syspkg/`
mirrors it.

Measurement confirms the choice: adding the subsystem moved `osdk` from 11.92 MB
to 11.95 MB (+0.25%), and left `osdk-shim` **unchanged at 3.5 MB**.

## Why it sits behind the `install` feature

The shim launches tools; it never needs to know what winget is doing. The whole
`syspkg` module is therefore gated behind `#[cfg(feature = "install")]`,
alongside `self_update` and `verification`, which keeps the subsystem and its
process probes out of the shim's build entirely.

Verifying this requires **two separate cargo invocations**:

```powershell
$env:CARGO_TARGET_DIR="target\size-check"
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

A single `--workspace` build unifies features, re-enabling `install`, and the
shim size it reports is wrong.

## Why the managers' tables are never parsed

This is the subsystem's most important constraint, and the easiest to
underestimate.

On a Simplified Chinese Windows host, `winget list` prints its columns as
名称 / ID / 版本 / 可用 / 源, and `winget source list` as 名称 / 参数 / 显式.
**A parser written against English headers does not fail there — it fails
silently**, reading a column it cannot find as "package not installed". That is
considerably worse than crashing.

The output layer is therefore ordered in three tiers and contains **no column
header literals at all**:

1. **Exit codes.** The one locale-independent channel: a query that matches
   nothing is `0x8A150014` in every display language. Classification reads
   `exit_code` and never the message.
2. **The JSON path.** `winget source export` emits JSON Lines whose keys stay
   English regardless of display language, including a structured `TrustLevel`;
   `winget export -o` writes a schema 2.0 package list.
3. **There is no third option.** Table parsing is ruled out.

The tests in `report.rs` assert negatively against the Chinese literals, so that
text can never leak into machine-readable output.

## Why `--no-progress` is not passed

The design initially assumed `--no-progress` was needed to keep stdout clean.
Measurement on a real host (Windows 11, winget v1.29.290) overturned that:

- Once redirected to a file, output is **byte-identical** with and without the
  flag, and carries no escape sequences either way.
- The flag has been **removed** from the help text of all seven subcommands osdk
  calls, though it is still silently accepted.

Depending on something undocumented that buys nothing is pure liability. The
call convention uses `--disable-interactivity` and `--nowarn`, both currently
documented. A test in `winget.rs` asserts osdk never passes `--no-progress`.

## The limits of discovery

- **Read-only.** Discovery runs two query commands; it installs nothing,
  configures nothing, and never elevates.
- **Bounded.** A user is waiting, so an unresponsive manager has to lose on a
  deadline rather than hang.
- **Injectable.** Probes run through `CommandRunner`, so tests cover host states
  with a scripted runner instead of requiring winget to be installed.
- **Purpose recorded, not command line.** A `ProbeRecord` keeps the purpose and
  the exit code, not the arguments, so it neither goes stale when flags change
  nor leaks an argument value.

## Why mirror probing reuses `source::select`

`osdk pkg mirrors test` has no measurement logic of its own. It goes through
`source::select`'s `effective_sources_for` and `refresh_with_timeout`, with the
pseudo-tool id `pkg:winget-source` standing in for the backend a real download
would name.

Those entry points exist precisely for downloaders that are not backends —
osdk's own self-update was the first. They already handle pins, the probe cache
and its fingerprint, `--refresh-sources`, offline mode, and the one-shot
`--source` override. A second measurement path would immediately diverge on
those semantics, and `select.rs` says so outright: the path exists so that "a
non-backend downloader cannot drift into its own mirror policy".

The `pkg:` prefix keeps these pseudo-ids out of the namespace real tools occupy,
so `osdk source` configuration for a tool can never collide with a package
manager's mirror configuration.

### Why the probe deadline is 12 seconds, not the default 1.5

The default `probe_timeout_ms` is tuned for small version indexes. A winget
source probe pulls `source.msix`, a real index package. Inside a 1.5s window
every candidate times out, all of them are marked unreachable, and ranking
silently degrades to fixed priority order — **the exact opposite of measuring
which route is faster**.

`self_update` hit the same problem earlier (a CN proxy measured at 6.1s just to
first byte), and this reuses its conclusion. Probes run concurrently, so 12s
bounds the whole command rather than each source. A configuration that raises
`probe_timeout_ms` is honoured; one that lowers it cannot go below this floor.

## Why index acceleration and artifact acceleration are distinguished

The `Acceleration` enum exists for one reason: winget and Homebrew differ
structurally.

- A winget source is a manifest index, and each manifest's `InstallerUrl`
  points at the vendor's own servers. A mirror **can only** speed up finding a
  package; it does nothing for downloading the installer.
- Homebrew bottles are hosted centrally and can be redirected wholesale, so a
  mirror accelerates both halves.

Hiding this has a concrete cost: a user switches sources to fix slow downloads,
sees no change, and concludes the feature is broken. So `MirrorCandidate`
records which half each candidate accelerates, and the human-readable output
appends an explanation when every candidate is `IndexOnly`. A test in `pkg.rs`
asserts that text is present.

For the same reason `acceleration_of` returns `None` for Homebrew rather than
guessing. Homebrew is not wired up yet, and inventing a value would be a lie.

## Why the official source is among the candidates

The official endpoint is measured alongside the mirrors. This is not redundant:
**"no mirror beats the official source" is a real and useful conclusion**,
especially outside China. Ranking mirrors alone nudges users toward switching
even when switching is slower.

The candidate set is itself evidence-based. The widely cited TUNA and Tencent
Cloud winget sources both return 404 and are therefore absent, with a test in
`mirror.rs` pinning that fact. The circulating Tencent Cloud walkthrough also
recommends a `winget source pin` command that does not exist, which is reason
enough to distrust the whole source.

Endpoints their operator does not document (nju, huaweicloud) rank below
documented ones when no measurement separates them. They work today, but nobody
has promised they will keep working, and saying so is more honest than quietly
ranking them first.

## Source selection: two namespaces and one exclusivity rule

osdk picks the fastest source for its own winget calls
(`preferred_winget_source`). Two traps live here, and both fail in ways that
look fine.

### osdk's mirror ids and the host's registered names are different namespaces

Internally the mirror ids are `ustc` / `nju` / `huaweicloud`, while
`winget --source` accepts only a `Name` the host has **registered**. They may
look alike, but they must never be assumed equal: measured on a real host,
passing an unregistered name fails outright with `0x8A150012`, turning an
installable package into an error.

Matching therefore compares **endpoints**, not ids: take the measured
candidate's URL, look for the same endpoint in `winget source export`'s
registration list, and use that entry's `Name` only on a hit. Both sides are
compared with any trailing slash trimmed, because `winget source add` preserves
whatever form the user typed.

The opposite direction is covered too: the same path on a different host
(`evil.invalid/winget-source`) is **not** a match, since treating it as one
would repoint osdk at an unrelated server.

### A mirror cannot coexist, so the test is the source *name*, not the endpoint

This overturned the first implementation of this module, and how it was
overturned is worth recording.

The first version decided whether to omit `--source` from
`SourceKind::Official`, on the implicit assumption that a mirror registers under
its own name alongside the official source, making "official" and "mirror" two
distinguishable entries.

Tested as administrator, that assumption is false. Adding a
`Microsoft.PreIndexed.Package` source pointing at USTC fails with
`0x80073D06`: the package could not be installed because a higher version is
already installed. Cross-checking `winget source export` shows the official
source's `Data` and `Identifier` are both
`Microsoft.Winget.Source_8wekyb3d8bbwe` — **the same MSIX identity named in the
failure**. Sources of this type install under one fixed identity, a mirror
distributes a copy of that same package, so a second one cannot be installed;
and because mirrors lag upstream, the mirror's version was older than the
installed one, which is what Windows rejected.

That is why mirror operators document `source remove winget` followed by
`source add winget <mirror>`: **replacing is the only available shape**.

So the test had to change. The real question is not "is this endpoint the
official one" but **"is this source already what winget uses by default"** — and
that is decided by the *name*: a source named `winget` always participates in a
call, whether it points at Microsoft's CDN or at a mirror. Judging by endpoint
breaks precisely after replacement: the fastest source is then a mirror, so the
code would name `--source winget` explicitly and hide msstore.

`NoPreferredSource::AlreadyTheDefaultSource` therefore covers both hosts: the
unmirrored one, where the default source is Microsoft's CDN, and the mirrored
one, where the default source is a mirror. Mutation testing confirms the test is
pinned: switching the guard back to an endpoint comparison turns it red.

**Consequence for the L2 layer**: `--source` is not a mirror-acceleration
mechanism on winget at all. Before replacement no mirror exists under any name
to select; after replacement the mirror is the default and needs no argument.
Acceleration comes entirely from the replacement. The layer is kept because it
still honours a user pin and refuses to pass osdk's internal ids as source
names, and because it is the shape Homebrew needs, where a mirror is chosen per
invocation through environment variables rather than a shared registration.

### `--source` is exclusive, so the default source is omitted, not named

Naming one source hides all the others. Measured on winget 1.29.290:
`winget search --query WhatsApp` returns msstore's WhatsApp, and adding
`--source winget` makes it disappear — **with exit code 0**.

So "the official source is fastest" cannot return `Ok("winget")`. That would
pass an argument with no benefit — it is already the default — while turning
msstore-only packages into "not found", a functional break that reports no
error. This case is modelled as `NoPreferredSource::AlreadyTheDefaultSource`, meaning
"no argument needed", kept distinct in the type system from a selection failure.

Every `NoPreferredSource` variant maps to its own sentence, because the remedy
differs: an unregistered mirror needs registering, while an honoured user pin
needs nothing at all. A generic "unavailable" would leave the two
indistinguishable.

### Mutation-tested

This logic was mutation-tested: four injected defects were all caught, including
the most dangerous one — returning the internal id instead of the registered
name, the `0x8A150012` bug above — which three tests caught at once. Per this
repository's own standard, a check that has never been seen red does not make
its green count as evidence.

## Applying a mirror: why three stages rather than one step

`mirrors apply` is the only entry point in this subsystem that changes machine
state, so its structure follows from the risk.

### Replacing necessarily passes through a window with no source

winget cannot atomically repoint a source, so the sequence is `remove` then
`add`. Between them the host has **no package source at all** -- and the likeliest
failure is the second command: when a mirror lags upstream, Windows refuses the
older package with `0x80073D06`.

That makes two things jointly necessary:

1. **Refuse what can be predicted** -- `assess_feasibility` compares publish
   times before anything runs.
2. **Roll back what cannot** -- any failing command triggers
   `winget source reset`.

The rollback uses `reset` rather than re-adding the official URL, because winget
knows its own built-in definition while an endpoint hardcoded here would go
stale. Whether the rollback worked is reported, never assumed: a silently failed
rollback is the one outcome a user must not be told is fine.

### The feasibility test is publish time, not a version number

Mirrors publish no version metadata. USTC answers `/version` with an honest 404;
Huawei's mirror returns `200` with the same 11,963-byte HTML page for *any* path,
so a successful request is not evidence a file exists, and trusting the status
code would feed an HTML document into a destructive decision.

`Last-Modified` on `source.msix` is used instead -- one `HEAD`, no 20 MB
download. It cannot be converted into a version (`2026.915.1714.48` was published
at 17:45 GMT, so `1714` is a build time), but it moves monotonically with the
version, which is all a staleness test needs. A `text/html` content type is
treated as unknown, which is what defends against the "200 plus HTML" case
above.

Dates are parsed into ordered fields rather than compared as text: "Dec" sorts
before "Sep" alphabetically while December is later, so a string comparison
would call a newer mirror stale and refuse a valid apply. Mutation testing found
the gap here first -- the initial dates happened to sort correctly either way, so
swapping in a string comparison left the tests green.

### A fingerprint confirms a host state, not merely a plan

Before running, the registered sources are re-read and compared against the
plan's fingerprint; a mismatch refuses. This is the same `StaleInput` guard
`container/apply.rs` applies to a config file. A wrong fingerprint and a changed
host are distinct refusals -- one is a stale copy-paste, the other is the machine
moving after you confirmed -- and they call for different remedies, so they read
differently. Neither runs a single command.

### Exit codes: not done is not success

`--dry-run` exits 0 once the plan is printed, because the report is the
requested output. But **asking to apply and ending with nothing applied** -- no
usable mirror, no confirmation, a wrong fingerprint, a changed host, a failed
command -- always exits non-zero. Otherwise a script reads "nothing happened" as
success, which is the hardest class of defect to notice.

## Why Linux detection is read-only, and how that is verified

### The stance follows from three facts, not from caution

apt, apk, pacman and dnf are structurally unlike winget and brew, so they are
kept apart in the type system: they live in the report's own `distro_managers`
field rather than among `managers`. Merged, a consumer could treat an apt entry
as something osdk installs through, which it deliberately is not.

Three facts each independently rule out driving them:

1. **Changes are global and need root.** apt's manual states that `full-upgrade`
   "will remove currently installed packages if this is needed to upgrade the
   system as a whole".
2. **Arch declares partial upgrades unsupported**, and installing just the
   package a project needs is a partial upgrade. So `install_command` prints
   `-Syu --needed` for pacman rather than `-S`: handing a user a command their
   distribution does not support is worse than handing them none. A test pins
   this.
3. **Rollback splits three ways.** `RollbackAbility` is therefore a per-manager
   enum rather than a boolean: transactional for dnf, manual-from-cache for
   pacman, none for apt. One shared answer would be false.

### The query interfaces chosen are the ones whose format osdk dictates

`dpkg-query -W -f=${Version}` and `rpm -q --qf` both pass an explicit format
string, so the parse target is osdk's own rather than a distribution default that
could change; `pacman -Q` and `apk info -e -v` have documented, unlocalized
shapes. A test asserts that **no query command could install anything** -- none
contains `install`, `add` or `-S`, and none starts with `sudo`.

Parsing apk's `name-version` output has a trap: a package name may itself contain
hyphens (`py3-foo-1.2-r0`). Splitting on the first hyphen would report
`foo-1.2-r0` as the version, so the split point is the first hyphen **followed by
a digit**, with a test for exactly that case.

### The platform decision is a parameter, or the interesting branch never runs

Had `detect` tested `cfg!(target_os = "linux")` inline, the only branch that
matters would be unreachable on the machine most of this is developed on. The
logic therefore lives in `detect_for(runner, limits, is_linux)`, with the `cfg!`
confined to `detect`. Debian, Arch and Fedora host shapes are all exercised from
Windows as a result.

### The one thing unit tests cannot establish, containers do

The unit tests cover the branching with a scripted runner, but they cannot prove
that these query interfaces exist and print what osdk expects. A changed
`dpkg-query` flag, or an `apk info` shape differing from the documented one,
would leave every test green and every report wrong.

So `scripts/linux-distro-detection.sh` runs `osdk pkg doctor --json` inside real
Debian, Alpine, Arch and Fedora containers and asserts, per distribution, that
the manager it ships is reported present, that the ones it does not ship are not,
and that `sudo` never appears in the diagnostic. It greps the JSON rather than
the human output, which is localized -- and **a unit test pins that exact JSON
literal**, since otherwise renaming a field would leave the script matching
nothing and passing vacuously.

With neither docker nor podman present the script prints why it is skipping and
exits 0, rather than failing a developer machine that has no container runtime.

## Missing from a platform is not missing from a host

`ManagerStatus` separates `NotApplicable` from `NotInstalled` because they call
for opposite responses: winget absent on macOS is nothing to fix, and doctor
must not advise installing it. Every known manager appears in the report
regardless of the host, so consumers can rely on a fixed output shape.
