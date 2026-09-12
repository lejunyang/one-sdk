# System package managers

osdk manages SDKs in its own store; it does not manage the host. But an SDK
toolchain sometimes needs things osdk has no business owning — shared
libraries, build prerequisites — and those belong to the host's own package
manager.

`osdk pkg` reports which system package managers the host has and what state
they are in. For now it only inspects: it runs a couple of query commands and
installs nothing, changes no configuration, and never elevates.

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

## Current boundaries

- Read-only. Installing and writing mirror configuration are not available yet.
- Only winget is inspected today. Homebrew is planned.
- osdk does not elevate on your behalf. When a later operation needs
  administrator rights, osdk will print the command for you to run rather than
  attempting to elevate.
