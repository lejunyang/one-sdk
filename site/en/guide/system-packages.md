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
