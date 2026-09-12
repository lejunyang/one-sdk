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

## Missing from a platform is not missing from a host

`ManagerStatus` separates `NotApplicable` from `NotInstalled` because they call
for opposite responses: winget absent on macOS is nothing to fix, and doctor
must not advise installing it. Every known manager appears in the report
regardless of the host, so consumers can rely on a fixed output shape.
