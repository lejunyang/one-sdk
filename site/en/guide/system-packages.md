# System package managers

osdk manages SDKs in its own store; it does not manage the host. But an SDK
toolchain sometimes needs things osdk has no business owning — shared
libraries, build prerequisites — and those belong to the host's own package
manager.

`osdk pkg` reports which system package managers the host has, what state they
are in, and which mirror serves them fastest. For now it only inspects and
measures: it installs nothing, changes no configuration, and never elevates.

## Inspecting the host's package managers

```bash
osdk pkg doctor
osdk pkg doctor --json
```

Sample output:

```text
System package managers
  winget: ok
    version: v1.29.290
    sources:
      winget (trusted) https://cdn.winget.microsoft.com/cache
      msstore (trusted) https://storeedgefd.dsx.mp.microsoft.com/v9.0

Nothing to fix.
```

The report separates three kinds of "unavailable", because they call for very
different responses:

- **not installed** — absent from a host where it belongs; guidance is offered.
- **not applicable on this platform** — winget on macOS, for instance. That is
  not a problem, and osdk will not suggest installing it.
- **degraded** — the client works, but some of its state could not be read.

When a non-default source is configured, the report says so, since it means
manifests come from a mirror rather than the official source.

## JSON output

`--json` emits a versioned schema meant for scripts:

```json
{
  "schema_version": 1,
  "managers": [
    {
      "manager": "winget",
      "status": "healthy",
      "details": {
        "kind": "winget",
        "version": "v1.29.290",
        "has_non_default_source": false
      },
      "capabilities": { "client": "supported" },
      "probes": [{ "purpose": "version-query", "outcome": "succeeded", "exit_code": 0 }]
    }
  ]
}
```

::: tip The JSON does not change with your display language
The managers' own tables are localised — on a Chinese Windows host `winget
list` prints its columns as 名称 / ID / 版本. osdk's JSON is not: the keys and
values are the same in every display language, and two runs are byte-identical.
:::

## Declaring which system packages you need

Declare them in the project's `osdk.toml`, keyed by `manager:package-id`:

```toml
[syspkg]
managers = ["winget"]     # only winget may participate; empty means no restriction
no_elevate = false

[syspkg.packages]
"winget:BurntSushi.ripgrep.MSVC" = "latest"
"winget:Microsoft.PowerToys" = "0.101.0"
"winget:Some.MacOnlyTool" = { version = "latest", os = "macos" }
```

The manager prefix is required. Package ids are not portable across managers —
winget's `PackageIdentifier` is case-sensitive and mirrors a repository path,
and brew additionally separates a formula from a cask — so osdk does not map
names between managers and asks you to say which one you mean.

::: warning `[syspkg]` must be trusted first
This table can cause software to be installed on your machine, which makes it
execution-affecting project configuration, so it goes through osdk's existing
trust flow. Until the project is trusted, every `pkg` subcommand refuses and
tells you to run `osdk trust`.
:::

::: tip A version is a wish, not a lock
The version in `[syspkg.packages]` means "ask for this when installing", not
"hold the host at this version". A system package manager updates on its own
schedule, and `osdk.lock` deliberately does **not** cover system packages. A
package present at a different version is reported honestly and **left alone** —
reinstalling it would change something you did not ask to change.
:::

### Linux packages belong in `[syspkg.packages]` too

```toml
[syspkg]
managers = ["apt"]

[syspkg.packages]
"apt:libssl-dev" = "latest"
"apk:build-base" = "latest"
"pacman:base-devel" = "latest"
"dnf:openssl-devel" = "latest"
```

`osdk pkg status` queries each one through its project's documented read-only
interface. No sudo is involved.

::: warning "Could not ask" is not "not installed"
When the host has no apt at all -- an `apt:` entry evaluated on Fedora, say --
osdk reports **manager unavailable**, not **missing**.

The distinction is not pedantry: treating an unanswered question as a negative
answer would make `--missing` fail spuriously in CI, and would send you
installing a package you may already have. Only a manager that actually answered
"no such package" produces `missing`.
:::
## Checking status

```bash
osdk pkg status
osdk pkg status --json
osdk pkg status --missing     # exit non-zero when something is absent, for CI
```

```text
System packages
  7zip.7zip            other version    requested 1.0.0-wrong (installed 22.01)
  Git.Git              ok               requested latest (installed 2.46.0)
  Some.MacTool         not for this os  requested latest (installed -)
  This.Is.Absent       missing          requested latest (installed -)
  invalid entry: `broken-no-prefix` needs a manager prefix, for example `winget:broken-no-prefix`
```

Two of the five states are easy to conflate:

- **other version** — installed, at a version other than requested. This is
  **not** missing, and `--missing` does not fail on it.
- **manager unavailable** — the manager itself could not be queried, so nothing
  can be said about the package. That is not the same as "absent": treating it
  as missing would send you installing something you may already have.

A malformed key is listed as an `invalid entry` rather than ignored: it stands
for a package you believe is managed, so skipping it silently would make
"nothing missing" untrue.

## Installing what is missing

```bash
osdk pkg plan                      # just show what would be installed
osdk pkg plan --detailed-exitcode  # 2 when work is pending, 0 when it is not
osdk pkg apply --dry-run
osdk pkg apply --yes               # the only command that installs anything
```

A plan accounts for every request — what it will install, and **why it leaves
the rest alone** — so nothing has to be inferred from an omission:

```text
Would install:
  This.Is.Absent (latest)
    winget install --id This.Is.Absent --exact --no-upgrade ...

Left alone:
  7zip.7zip     present at another version; the configured version is a wish, not a lock
  Git.Git       already installed
  Some.MacTool  not for this operating system
```

::: warning osdk will not upgrade your packages as a side effect
This one was found by measurement: running `winget install` on a package that is
**already present** makes winget upgrade it. On a host holding Git 2.46.0 it
immediately began downloading 2.55.0.3 — an operation nobody requested.

So osdk always passes `--no-upgrade`. "Make sure this package exists" does only
that, and never moves you off a version you stayed on deliberately.
:::

One package failing does not abandon the others: they are independent requests.
Every outcome is listed, and the command exits non-zero if any failed.

::: tip System packages do not carry osdk-level artifact verification
osdk hashes and verifies signatures for the SDKs it downloads itself, but it
never touches the bytes of a system package — downloading and verification are
winget's (it checks the installer hash declared in the manifest). Worth stating
plainly, so you do not assume `osdk pkg apply` gives the same guarantees as
`osdk install`.
:::

## Linux package managers: detected, not managed

On Linux, `osdk pkg doctor` additionally reports apt, apk, pacman and dnf:

```text
Linux package managers (detected, not managed)
  pacman: present, rollback manual downgrade from cache only
    this distribution supports only full-system upgrades; run `pacman -Syu` yourself
  osdk reports these and prints commands for you to run. It never installs,
  upgrades, or elevates through them.
```

**osdk will not install, upgrade, or elevate through them.** That is a deliberate
boundary, not an unfinished feature, for three reasons:

1. **Every change is global and needs root.** apt's own manual states that
   `full-upgrade` "**will remove currently installed packages** if this is needed
   to upgrade the system as a whole" — one install can cascade into upgrading
   shared libraries and removing other packages.
2. **Arch declares partial upgrades unsupported.** From the Wiki: "**never** run
   `pacman -Sy`; instead, **always** use `pacman -Syu`". Installing just the one
   package a project needs *is* a partial upgrade. Arch also asks you to read
   release announcements first, which cannot be automated.
3. **Failure recovery differs fundamentally.** dnf has atomic `history undo`;
   pacman can only downgrade by hand from a cache that routine maintenance
   clears, which it calls a last resort; apt has logs and no undo at all. **No
   single abstraction can promise consistent recovery semantics** — a deeper
   problem than being hard to implement.

So doctor states each manager's rollback ability plainly. That is the fact which
decides whether you should let any tool drive your package manager, and it is
not the same answer across these four.

::: tip Detection is read-only and never needs sudo
Version queries use each project's documented read-only interface:
`dpkg-query -W -f=`, `apk info -e -v`, `pacman -Q`, `rpm -q --qf`. Each specifies
an explicit output format, so nothing depends on a default that could change, and
no localized table is ever parsed.
:::

::: warning zypper is not listed yet
Not because it does not matter, but because it has **not been verified**. The
available clues point the other way: openSUSE integrates btrfs snapshots through
snapper, which would give filesystem-level rollback, and zypper documents an
exit-code table. Listing it as "same as apt" would assert something unchecked.
:::

### Elevation: four cases, and never a hang

Linux package managers need root. osdk decides what to do from the four cases
below, checked in this order:

| Case | What osdk does |
| --- | --- |
| **Already root** (containers, CI) | Runs directly, without invoking sudo — it may not even be installed there, and is not needed |
| **Elevation forbidden** (`no_elevate = true`) | Does not run; prints the command for you |
| **Passwordless sudo available** | Uses `sudo --non-interactive`, which cannot prompt |
| **Interactive terminal** | Ordinary `sudo`, prompting as usual |
| **No terminal and no passwordless sudo** | **Refuses and prints the full command**, rather than waiting for a password nobody will type |

That last row is why the policy exists: hanging on a password prompt in a CI job
with no TTY burns the entire job timeout before saying anything — **a hang is
worse than a failure**.

Note that `no_elevate` forbids *elevating*, not doing work that requires root.
It has no effect when you are already root, because no elevation happens there.

In every case the full command line is recorded before it runs.
## Three source paths, not one

Three things in osdk are called a "source", and they govern different things:

| Command | Governs |
| --- | --- |
| `osdk source` | where osdk downloads SDKs from |
| `osdk registry` | which registry project dependencies come from |
| `osdk pkg` | the host package manager's sources and mirrors (read-only today) |

## Measuring mirrors

```bash
osdk pkg mirrors test
osdk pkg mirrors test --json
```

osdk fetches each candidate's index package concurrently and ranks them by
measured speed:

```text
Mirrors for winget
  1. huaweicloud        11.5 MiB/s  ttfb 338ms
  2. ustc                3.6 MiB/s  ttfb 457ms
  3. nju                 1.3 MiB/s  ttfb 444ms
  4. official            1.1 MiB/s  ttfb 620ms

These mirrors carry the package index only. Installers are downloaded
from each vendor's own servers, so switching source speeds up finding
a package, not downloading it.
```

The official source is measured alongside the mirrors. That is deliberate: if
no mirror beats it, you should be told so rather than nudged into switching.

::: warning A winget mirror speeds up finding packages, not downloading them
A winget source is a manifest index, and the `InstallerUrl` in each manifest
points at the software vendor's own servers. After switching sources, search
and list get faster and **installer download speed is completely unchanged**.

Homebrew differs here: bottles are hosted centrally, so a mirror accelerates
both halves. osdk reports the distinction rather than glossing over it, so you
do not switch sources, see downloads crawl, and conclude the feature is broken.
:::

Measuring only issues HTTP requests; it changes no winget configuration.
Offline, the command fails outright instead of returning a ranking it never
measured.

Endpoints their operator does not document (today: nju and huaweicloud) rank
below documented ones when no measurement separates them. They work, but
nobody has promised they will keep working.

## How mirror acceleration will apply itself

Measuring is read-only, but it is not the end goal. Acceleration comes in three
layers, with side effects escalating as you go down:

| Layer | Scope | Needs admin? | Status |
| --- | --- | --- | --- |
| Which source osdk downloads SDKs from | osdk's own store only | No | **Already automatic**, see [Sources and security](sources-security.md) |
| Which source osdk passes to winget | that one osdk-issued call only | No | Implemented, but no gain on winget (below) |
| winget's global source configuration | every winget user on the machine | **Yes** | **Implemented**, confirmed once by you |

The middle layer is where "acceleration when installing dependencies" belongs:
osdk picks the fastest **already-registered** source for its own winget calls.
Your own `winget install` behaves exactly as before, and no admin rights are
needed. `mirrors test` ends by stating which source it chose, or why it chose
none:

```text
osdk will pass --source ustc-winget on winget calls it issues itself.
Your own winget commands are unaffected.
```

::: warning winget mirroring works by replacing the default source, not via `--source`
This is counter-intuitive and was verified on a real host.

A winget package source is an MSIX package with a fixed identity
(`Microsoft.Winget.Source_8wekyb3d8bbwe`), so **a mirror cannot be registered as
a separate source alongside the official one** — adding it as administrator
fails with `0x80073D06` ("a higher version of this package is already
installed"). That is exactly why mirror operators instruct you to run
`winget source remove winget` and then
`winget source add winget <mirror-url>`: replacing is the only shape available.

After replacing, the mirror **is** the default source named `winget`, in effect
for every winget call automatically, with **no `--source` argument needed**. So
acceleration on winget comes entirely from the third layer above.

And `--source` is exclusive: naming one source hides all the others. Measured
here, adding `--source winget` makes packages only msstore carries (WhatsApp,
for one) impossible to find — and **the exit code is still 0, with no error at
all**. So whether that source points at Microsoft's CDN or at a mirror, osdk
**omits** `--source`, rather than turning msstore packages into "not found".
:::

osdk also never passes its own built-in mirror names through: naming a source
winget does not know fails the whole command with `0x8A150012`, turning an
installable package into an error. Omitting the argument is the only safe
degradation.

Only the last layer changes this machine's global configuration. It asks you
once, because it affects more than osdk — every winget caller afterwards sees
that source, and a mirror cannot carry the official source's `StoreOrigin`
trust marker. Once confirmed, osdk maintains it without asking again.

## Applying a mirror

```bash
osdk pkg mirrors apply --dry-run              # see the plan, change nothing
osdk pkg mirrors apply --accept-plan <print>  # confirmed; needs administrator
```

This is the only command in `osdk pkg` that changes machine state. It does four
things in order: measure, rule out mirrors that cannot be installed, print the
full plan, and execute only when given the plan's fingerprint. The first three
are read-only, so running it without `--accept-plan` can never alter the host.

### Mirrors that cannot be installed are refused up front

A mirror lagging behind upstream is normal, and winget rejects a package older
than the one installed. osdk checks before touching anything:

```text
No mirror can be applied right now.
  currently registered source published: Wed, 16 Sep 2026 01:08:41 GMT
  huaweicloud: publish time could not be established, so staleness cannot be ruled out
  ustc: published Tue, 15 Sep 2026 18:52:34 GMT, older than what is installed --
        winget would reject it with 0x80073D06

A mirror lagging behind upstream is common and resolves itself once it
syncs. Nothing was changed.
```

The test is each mirror's `Last-Modified` on `source.msix`, taken with one
`HEAD` rather than downloading the 20 MB package. Without a trustworthy publish
time a mirror counts as unusable: proceeding hopefully would cause the very
failure the check exists to prevent.

### A failed apply rolls itself back

Replacing means `remove` before `add`, and between those two commands the host
has **no package source at all**. If the add fails, osdk immediately runs
`winget source reset` to restore winget's own built-in definition, and reports
truthfully whether that worked:

```text
failed: winget source add --name winget ...
  exit code: -2147009274
  ROLLBACK FAILED: winget may have no package source right now.
  Run `winget source reset --name winget --force` as administrator.
```

When the rollback fails too, the recovery command is right there rather than
something to look up.

### The fingerprint confirms one particular host state

A plan's fingerprint covers the sources registered when it was built. If they
change after you confirm — another administrator, another tool, another
terminal — osdk refuses rather than acting on a plan that no longer describes
the machine.

::: tip Exit codes are scriptable
`--dry-run` exits 0 once it has printed the plan. But **asking to apply and
ending up with nothing applied** — no usable mirror, no confirmation, a wrong
fingerprint, a changed host — always exits non-zero, so a script never reads
"nothing happened" as success.
:::

## Current boundaries

- All three layers are implemented. Homebrew is not wired up yet.
- Only winget is covered today. Homebrew is planned.
- osdk does not elevate on your behalf. When a later operation needs
  administrator rights, osdk will print the command for you to run rather than
  attempting to elevate.
