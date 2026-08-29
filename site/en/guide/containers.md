# Container Runtimes and Native Caches

osdk can inspect Docker Engine, containerd, and Docker Buildx without changing
their configuration or storage. Use these commands to answer two separate
questions: which native runtime is usable, and how much space its supported
native cache interface reports.

## Diagnose runtimes and builders

```bash
osdk container doctor
osdk container doctor --runtime docker
osdk container doctor --runtime containerd
osdk container doctor --runtime docker --builder team-builder
osdk container doctor --json
```

The default runtime comes from effective `[containers].runtime` configuration.
With `auto`, osdk probes Docker first and containerd second, then chooses by
diagnosed state: healthy, degraded, client-only, permission-denied,
unreachable, unsupported-version, then not-installed. Docker wins equal-status
ties. Selection is based on the probes, not on whether a binary merely exists.

Buildx is reported separately because a builder is not the selected runtime.
Auto and explicit Docker diagnostics inspect the configured/current Buildx
builder. Explicit containerd diagnostics skip Buildx unless `--builder NAME` is
provided. Builder inspection never uses `--bootstrap`, so it does not start a
stopped builder.

Human output starts with the selected runtime and its status. `--json` emits a
deterministic schema-version-1 object containing the selected report, every
runtime report attempted during auto selection, and the separate optional
builder report. Field names and enum values remain English regardless of
`--lang`, so automation receives the same contract in either UI language.

## Inspect native cache usage

```bash
osdk container cache status
osdk container cache status --runtime docker
osdk container cache status --runtime buildkit
osdk container cache status --runtime buildkit --builder team-builder
osdk container cache status --runtime containerd --json
```

`docker` runs Docker's aggregate `system df` interface. `buildkit` uses the
selected Buildx builder and its documented JSON `du` interface; this requires
Buildx 0.28 or newer. `containerd` returns a structured `unsupported` result:
containerd has no single stable aggregate cache-status contract comparable to
those interfaces. osdk does not approximate one by scanning containerd's
private content, snapshot, or metadata stores.
`--builder NAME` overrides the configured builder for cache status just as it
does for doctor; it affects the BuildKit query and is otherwise ignored.

For `auto`, osdk applies the same runtime-status selection as `doctor`. If
Docker is selected it queries Docker Engine; if containerd is selected it
returns containerd's explicit unsupported result. Select `--runtime buildkit`
when you specifically want builder cache usage.

Cache JSON is the native-cache schema version 1. It contains only typed
categories, counts, byte totals, reclaimable bytes, status, owner, and redacted
command evidence. Native object IDs, descriptions, builder names, command
output, credentials, and private paths are excluded.

## Configuration and precedence

```toml
[containers]
runtime = "auto"
builder = "auto"
platform = "runtime"
probe_timeout_ms = 1500
```

`--runtime` and `--builder` override effective configuration for that command.
The non-secret selectors can also be set with `OSDK_CONTAINER_RUNTIME`,
`OSDK_CONTAINER_BUILDER`, and `OSDK_CONTAINER_PLATFORM`. Each captured probe
uses `probe_timeout_ms` plus fixed 64 KiB stdout and stderr ceilings.

## Status guidance

| Status | Meaning | Typical next step |
| --- | --- | --- |
| `healthy` | Client and selected daemon/builder responded | No action |
| `degraded` | Only part of the expected typed data was available | Inspect the native service and retry |
| `client-only` | The CLI exists but no daemon/builder was confirmed | Start or select the intended service |
| `permission-denied` | The endpoint exists but the current user cannot inspect it | Fix native socket/context permissions |
| `unreachable` | The selected endpoint did not respond before the bound | Check the daemon, context, socket, or network |
| `unsupported-version` | The native CLI is older than the machine-readable contract osdk requires | Upgrade the native tool |
| `not-installed` | The required executable could not be started | Install it or choose another runtime |

Cache status uses a more specific status set:

| Cache status | Meaning |
| --- | --- |
| `available` | Aggregate records and byte totals were parsed successfully |
| `not-installed` | The required Docker or Buildx executable is missing |
| `permission-denied` | The native cache interface rejected the current user |
| `unreachable` | The native service reported a connectivity failure |
| `timed-out` | The bounded cache query exceeded `probe_timeout_ms` |
| `unsupported` | No safe aggregate interface exists; currently returned for containerd |
| `unsupported-version` | The native CLI is too old for the required machine format |
| `output-truncated` | A fixed capture ceiling was reached |
| `invalid-output` | Successful output did not satisfy the typed aggregate schema |
| `command-failed` | The native command failed without a more specific classification |

These commands are read-only. They do not pull images, prune caches, rewrite
daemon configuration, start builders, or inspect implementation-private store
directories. See [Container diagnostics implementation](./implementation/containers)
for the probe and redaction boundaries.
