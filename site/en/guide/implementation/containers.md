# Native Container Diagnostics and Operations

This page describes the read-only adapter path behind `osdk container doctor`,
`osdk container cache status`, `osdk container registry test`, and
`osdk container mirrors plan`, plus the controlled mutation paths behind
`osdk container pull` and `osdk container prune`. The adapters treat Docker
Engine, containerd, and BuildKit as separate owners. osdk never introduces a
shared OCI store or silently changes which native control plane owns an
operation.

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
than a responsive daemon. The schema-v2 doctor wrapper retains both attempted
reports and a closed tagged details object for each runtime, even though human
output leads with the selected conclusion.

Buildx remains a separate optional report. Auto and Docker selection inspect it;
explicit containerd skips it unless the caller supplied `--builder`. A named
selector is validated before invocation and never appears in serialized
evidence.

The details projection reuses data from those same probes and starts no extra
processes. Docker exposes typed version, context-kind, daemon-platform,
rootless/Desktop, and origin-only mirror facts. containerd exposes version and
configuration-state facts but not namespace or config paths. BuildKit replaces
builder/node names with stable ordinals, validates platform tokens, and reduces
endpoints to scheme/host/port origins. Missing facts remain absent rather than
being inferred.

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

## Direct image-pull launch

`container pull` reads the effective container runtime and platform, with
explicit `--runtime` and `--platform` values taking precedence. `--address` and
`--namespace` form a paired explicit containerd target. Explicit Docker or
containerd selects only that adapter. `auto` performs one bounded read-only
Docker-plus-containerd resolution using the deterministic diagnostic ordering,
then freezes the selected owner before any mutating command is constructed.
Explicit containerd requires the pair immediately; auto requires it only when
containerd wins, so a Docker winner can launch without containerd selectors.

The selected adapter builds exactly one direct foreground command:

```text
docker image pull [--platform PLATFORM] IMAGE
ctr --address ADDRESS --namespace NAMESPACE images pull [--platform PLATFORM] IMAGE
```

The child inherits stdio instead of using the bounded capture path, and osdk
waits for it. The process result is the child's direct exit code, or normalized
to `128 + signal` if it terminates by signal on Unix. A launched command is the
single attempt: osdk does not catch a Docker failure and retry with containerd,
or vice versa. Resolution is bounded and read-only, but the foreground command
itself follows the native client's normal pull lifetime.

Offline mode fails before resolution and foreground command construction.

The pull path performs no registry transfer through osdk, creates no image
manifest or layer objects in osdk's CAS, and has no osdk OCI store. Platform is
forwarded to the selected native client; ownership, authentication, content
verification, unpacking, and local image visibility remain that client's
responsibility.

## Anonymous OCI registry diagnostics

The CLI constructs the HTTPS upstream from the positional registry; logical
`docker.io` uses `registry-1.docker.io` for transport. A registry policy is
optional for `registry test`: without one the run is upstream-only, while a
matching policy contributes mirrors in configured order. API-only runs probe
`/v2/` on upstream and each mirror. Image runs resolve and verify upstream first,
then query every mirror by the resolved digest rather than re-resolving a moving
tag. The image registry must match the positional registry.

[`RegistryEndpoint`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/container/registry.rs)
accepts HTTPS without userinfo, query, fragment, percent encoding, or parent
components. A mirror path prefix is retained when `/v2/...` is appended, while
the serialized diagnostic exposes only its origin. The production transport
disables automatic redirects, proxies, gzip, cookies, client certificates, and
ambient registry credentials. Redirects are followed manually only on the same
HTTPS origin and within fixed budgets.

A `401 Bearer` challenge can request an anonymous pull token only from a
same-origin HTTPS realm. An optional scope must exactly equal
`repository:<repository>:pull`; the token request carries no authorization, and
the result is bound to the issuing origin. Upstream and mirrors authenticate
independently, so no token crosses between them.

The CLI derives request bounds from `probe_timeout_ms`: per-request timeout is
`min(probe_timeout_ms, 60s)`, and total timeout is
`min(per-request × 12, 5 minutes)`. The protocol also caps a run at 12 requests
and each request chain at three redirects, API bodies at 8 KiB, anonymous-token bodies at 64 KiB,
manifests at 2 MiB, general bodies at 4 MiB, and the layer Range sample at
16 KiB. Offline mode fails before transport construction.

For an image, manifest bytes are SHA-256 hashed and checked against an explicit
digest, `Docker-Content-Digest`, and selected child descriptor digest/size where
applicable. An index requires exactly one descriptor for the requested platform.
The smallest layer is sampled with an exact bounded `Range`; a complete small
layer is also digest-checked. No complete image is pulled or stored.

`RegistryDiagnosticReport` schema 1 serializes typed API, manifest, Range, and
ordered mirror results plus safe origins, requested image/platform, digests,
byte counts, and request count. Tokens, raw headers, response bodies, cookies,
and native credentials cannot enter the report.

## Native mirror planning

The CLI requires one configured positional registry and one explicit
`docker|containerd|buildkit` runtime. Missing policy fails before native
discovery; there is no auto mode and no sibling registry is folded into the
plan. `--builder` is BuildKit-only. Native discovery uses `probe_timeout_ms` and
fixed 64 KiB stdout/stderr capture ceilings.

The three planners preserve their native boundaries:

- Docker supports only `docker.io`. Moby `registry-mirrors` accepts origins, so path-prefixed mirrors are rejected rather than truncated. A local/rootless target with `resolve=mirror` and explicit daemon JSON can be `ready`; `resolve=upstream` is `manual-only` because Moby cannot separate resolution from transfer. Activation is `restart-daemon`.
- containerd targets the exact `<config_path>/<registry>/hosts.toml`. A path-prefixed mirror remains a base URL before containerd appends `/v2/...`; osdk does not infer `override_path`, which would mean the path is already the API root. `resolve=upstream` grants `pull`; `resolve=mirror` grants `pull, resolve`; `push` is never added. Existing unrelated host/TLS/custom entries remain in the in-memory candidate. If no active `config_path` exists, an explicit hosts path and `--containerd-main-config` can describe both changes, but the plan is `manual-only` and requires daemon restart.
- BuildKit's Docker driver is `unsupported` because it uses Engine policy. A local `docker-container` builder with explicit `buildkitd.toml` can be `ready` under mirror resolution and reports `recreate-builder`; remote/Kubernetes/cloud/unknown targets and upstream-resolution separation are `manual-only`. A prefixed mirror is rendered in candidate TOML as `host[:port]/path`.

Without `--native-config`, a planner that could otherwise create a local candidate
records `native-config-path-required`, emits no candidate fingerprint, and
downgrades `ready` to `manual-only`. Snapshots are bounded to 4 MiB, open the
final component without following symlinks/reparse points, detect changes during
reading, and can represent a missing target file.

## Plan identity and disclosure boundary

[`MirrorPlan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/container/plan.rs)
is schema version 1. Inputs and candidates are path-sorted, and `plan_id` is the
SHA-256 of canonical JSON for every semantic field except the ID itself: target
and policy, input/candidate fingerprints, changes, privilege, activation,
validation steps, and warnings. Human output prints the ID first; `--json` emits
the complete semantic plan.

Plan JSON is operational rather than anonymous. It may contain canonical
absolute native-config paths, the selected Buildx builder name, containerd
namespace/config path, and redacted native endpoint identities. Mirror changes
expose only the origin and `has_path_prefix`; an exact prefix is replaced with a
redaction marker. The policy fingerprint always binds the exact prefix, and a
generated candidate's hidden bytes and fingerprint bind it as well. Consumers
should review plan JSON before sharing it.

`NativeConfigSnapshot` contents and generated `NativeConfigCandidate` bytes are
not serializable. JSON retains only path, state/format, size, and SHA-256/metadata
fingerprints. The CLI discards the in-memory candidate bundle and prints only
the plan. It exposes no apply option: planning performs bounded reads and
discovery but never writes files, elevates privileges, restarts a daemon, or
recreates a builder.

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

## Preview-bound native pruning

Pruning has a closed runtime/scope matrix. `docker + images` and
`buildkit + build-cache` are the only valid pairs. The shared CLI grammar still
requires `--scope` for containerd, but neither scope is an accepted pair. With
no selector or execution flag it returns typed unsupported; `--context`,
`--builder`, `--execute`, and `--accept-preview` are rejected for containerd.
Crossed Docker/BuildKit combinations also fail before execution. There is no
generic `all`/`system` scope and no route that prunes containers, volumes,
networks, or osdk/private native stores.

The preview phase performs bounded read-only discovery and resolves one exact
owner target:

- Docker resolves one exact context for display, then privately retains its raw
  endpoint. Execution is exactly `docker --host ENDPOINT image prune --force`,
  so later context-name retargeting cannot redirect it. Only a local Unix socket
  or Windows named pipe without context-held TLS material is executable; remote,
  SSH, TCP/TLS, Docker Desktop, and incomplete targets fail closed. Its native
  scope is dangling images.
- BuildKit resolves one exact Buildx builder for preview. An explicit
  `--builder NAME` wins; otherwise discovery uses the effective configured
  builder and then the current builder. Execution is unsupported because the
  native CLI exposes only the mutable builder name, not an immutable handle.

The default path stops after rendering a schema-version-2 preview. Its deterministic SHA-256 ID
binds the owner, scope, resolved target, warning, and a secret-safe fingerprint
of the complete Docker endpoint or Buildx driver plus sorted node-name/endpoint
topology. Raw endpoints are hashed before diagnostic redaction and never enter
serialized output. The execute path recomputes
the preview from current discovery and requires both `--execute` and an exact
`--accept-preview` match. Consequently, a same-named context or builder retargeted
to another endpoint, a changed driver/node topology, a changed current context/builder, a
different explicit selector, a scope/owner change, a warning change, or an ID
from any differently bound preview all fail closed before the native prune is
launched. Such a changed preview is semantically stale. The ID has no time
component: unchanged bound fields reproduce it.

For an executable Docker preview, matching the ID enables only the
execution-confirmation stage. The normal prompt must still be accepted, while
global `--yes` may answer that prompt. It does not substitute for `--execute` or
`--accept-preview`. The already validated endpoint value—not the mutable context
name—is then passed directly to the one native prune launch.

## Serialization and redaction

`DiagnosticReport` is a closed schema-version-2 contract; `NativeCacheStatus`
remains schema version 1. Ordered maps/sets and sorted cache records make repeated JSON output
deterministic. Pull selection and prune previews also use canonical typed inputs
before native launch; the prune preview identity deliberately includes the exact
target and warning. JSON field names and enum values are never localized. Human
labels come from the English/Chinese catalog after selection is complete.

Raw `CommandSpec`, stdout, and stderr are not serializable. Doctor and cache
reports contain only typed status/capability facts and redacted evidence. Endpoint construction
removes user information, secret path/query details, and fragments; command
evidence records only the native program and operation purpose. Builder names,
containerd namespaces, cache IDs/descriptions, and parser errors cannot enter
those diagnostic/cache contracts. Mirror plans intentionally use the different
operational disclosure boundary documented above.

## Failure boundaries

Missing executables, permission failures, timeouts, unreachable endpoints, old
versions, truncation, invalid structured output, and command failures remain
distinct typed states where the underlying contract supports them. Native
stderr is used only for classification and is discarded afterward. A status
query therefore produces useful machine output without echoing daemon errors or
credentials.

Doctor, cache status, registry testing, mirror planning, and prune preview never
call foreground mutation. Pull and approved prune are intentionally narrow
exceptions: each launches one selected native command after resolution and does
not fall back. No path here writes native configuration, bootstraps/recreates a
builder, restarts a daemon, scans osdk's private store, or implements a private
OCI store. Registry testing performs only the bounded metadata and Range reads
described above.
