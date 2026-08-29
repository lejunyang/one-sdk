# Container Diagnostics and Native Cache Implementation

This page describes the read-only native adapter path behind `osdk container
doctor` and `osdk container cache status`. The adapters treat Docker Engine,
containerd, and BuildKit as separate owners; osdk does not introduce a shared
container store.

## CLI orchestration and selection

The command layer reads the effective `ContainersConfig`, applies explicit CLI
runtime/builder selectors, and converts `probe_timeout_ms` into a `CaptureLimits`
value with fixed 64 KiB ceilings for each captured stream. All subprocesses go
through the injected `CommandRunner`; production uses `SystemCommandRunner`,
while tests supply deterministic outcomes and inspect command order and limits.

Explicit Docker or containerd selection invokes only that runtime adapter. Auto
selection probes Docker and containerd in a stable order and ranks their typed
`DiagnosticStatus`: healthy, degraded, client-only, permission-denied,
unreachable, unsupported-version, then not-installed. Docker is the fixed
tie-break. This avoids treating an installed but unusable client as healthier
than a responsive daemon. The schema-v1 doctor wrapper retains both attempted
reports, even though human output leads with the selected conclusion.

Buildx remains a separate optional report. Auto and Docker selection inspect it;
explicit containerd skips it unless the caller supplied `--builder`. A named
selector is validated before invocation and never appears in serialized
evidence.

## Read-only probes

The Docker adapter uses supported CLI formats in this order:

```text
docker version --format '<json-template>'
docker context inspect
docker info --format '<json-template>'
```

The containerd adapter runs `containerd --version`, `ctr --address ...
--namespace ... version`, and, only for a local endpoint, `containerd config
dump`. Explicit address and namespace selectors are passed as arguments instead
of inferred from ambient native environment variables.

The BuildKit adapter uses `docker buildx version`, machine-readable `buildx ls`,
and `buildx inspect` for the exact selected list result. It deliberately omits
`--bootstrap`; inspection cannot start a builder. Minimum supported versions are
Docker 19.3, containerd 1.6, and Buildx 0.10 for diagnostics.

## Native cache ownership

Docker cache status parses `docker system df --format '<json-template>'` into closed
categories: images, containers, local volumes, and build cache. BuildKit cache
status first proves Buildx 0.28 or newer, then parses `docker buildx du
--format=json`, optionally bound to a validated builder name. Totals are checked
for overflow and invalid active/reclaimable relationships.

containerd returns a typed `unsupported` status without executing a cache
command. containerd exposes several namespace-dependent content, image,
snapshot, and CRI views, but no single supported aggregate equivalent. Walking
`/var/lib/containerd`, Docker roots, BuildKit state, or any private native store
would couple osdk to implementation details and can cross privilege boundaries,
so this path never does that.

## Serialization and redaction

`DiagnosticReport` and `NativeCacheStatus` are closed schema-version-1
contracts. Ordered maps/sets and sorted cache records make repeated JSON output
deterministic. JSON field names and enum values are never localized. Human
labels come from the English/Chinese catalog after selection is complete.

Raw `CommandSpec`, stdout, and stderr are not serializable. Reports contain only
typed status/capability facts and redacted evidence. Endpoint construction
removes user information, secret path/query details, and fragments; command
evidence records only the native program and operation purpose. Builder names,
containerd namespaces, cache IDs/descriptions, and parser errors cannot enter
the stable JSON contract.

## Failure boundaries

Missing executables, permission failures, timeouts, unreachable endpoints, old
versions, truncation, invalid structured output, and command failures remain
distinct typed states where the underlying contract supports them. Native
stderr is used only for classification and is discarded afterward. A status
query therefore produces useful machine output without echoing daemon errors or
credentials.

No path in these commands calls foreground execution, writes configuration,
pulls content, prunes native data, bootstraps a builder, or scans osdk's private
store.
