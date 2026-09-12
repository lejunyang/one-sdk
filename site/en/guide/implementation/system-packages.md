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

## Missing from a platform is not missing from a host

`ManagerStatus` separates `NotApplicable` from `NotInstalled` because they call
for opposite responses: winget absent on macOS is nothing to fix, and doctor
must not advise installing it. Every known manager appears in the report
regardless of the host, so consumers can rely on a fixed output shape.
