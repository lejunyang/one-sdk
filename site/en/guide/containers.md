# Container Runtimes, Registries, and Native Caches

osdk can inspect Docker Engine, containerd, and Docker Buildx without changing
their configuration or storage. It can also test an OCI registry and its
configured mirrors anonymously, then produce a read-only native mirror plan.
Testing and planning are separate: a test performs bounded network reads, while
a plan inspects one native control plane and never writes its configuration.

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

[containers.registries."docker.io"]
mirrors = [
  "https://mirror-one.example/",
  "https://mirror-two.example/",
]
anonymous_only = true
resolve = "mirror"         # upstream (default)|mirror
```

`--runtime` and `--builder` override effective configuration for that command.
The non-secret selectors can also be set with `OSDK_CONTAINER_RUNTIME`,
`OSDK_CONTAINER_BUILDER`, and `OSDK_CONTAINER_PLATFORM`. Each captured probe
uses `probe_timeout_ms` plus fixed 64 KiB stdout and stderr ceilings.

Registry keys are canonical host names with an optional port, not URLs. Mirror
values must be HTTPS URLs without credentials, query strings, or fragments.
They are normalized with a trailing slash and deduplicated while preserving the
first configured order. Project-level `[containers]` configuration requires
explicit trust and replaces the lower-precedence section as a unit.

`anonymous_only=true` is the policy default. Registry tests are always anonymous
regardless of that setting. A native Docker/containerd/BuildKit configuration
cannot guarantee that the runtime will never attach its own credentials, so a
plan with `anonymous_only=true` surfaces an `anonymous-only-not-enforced`
warning. `resolve=upstream` asks the origin to own tag resolution where the
runtime can express that separation; `resolve=mirror` allows the mirror to
resolve tags.

## Test an OCI registry and its mirrors

```text
osdk container registry test REGISTRY
  [--image IMAGE]
  [--platform OS/ARCH[/VARIANT]]
  [--json]
```

```bash
# API-only upstream test; no registry policy is required.
osdk container registry test docker.io

# Verify a tag or digest, select a platform from an image index, and test Range.
osdk container registry test docker.io \
  --image ubuntu:24.04 --platform linux/amd64

osdk container registry test ghcr.io \
  --image ghcr.io/example/tool@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --platform linux/arm64 --json
```

`REGISTRY` is a host with an optional port. It is always tested at its canonical
HTTPS upstream; `docker.io` uses `registry-1.docker.io` as the transport host. If
there is no matching `[containers.registries.<registry>]` block, the command is
an upstream-only test. When a policy exists, its mirrors are checked
sequentially in configured order; osdk does not latency-sort them. An API-only
test calls `/v2/` on the upstream and every configured mirror. With `--image`,
the image registry must match `REGISTRY`; osdk resolves and verifies the upstream
manifest, then asks each mirror for that immutable digest rather than resolving
the moving tag again.

`--image` accepts tags and digests. A digest selector must match the returned
manifest bytes. For an image index, `--platform` selects exactly one child and
verifies its descriptor digest and size. The CLI value overrides an explicit
`[containers].platform`; when neither supplies a platform, an index reports
`platform-not-found`. After validating an image manifest, osdk sends a bounded
Range request for up to 16 KiB from its smallest layer and validates the exact
`Content-Range`; when the complete layer fits in the sample, it also verifies
the layer digest. The command never pulls or stores the complete image.

The diagnostic is anonymous: it does not read native credential stores, cookies,
client certificates, or ambient registry credentials, and it disables proxies.
A `401 Bearer` challenge may obtain an anonymous pull token only when its realm
is HTTPS and same-origin; any supplied scope must be exactly
`repository:<repository>:pull`. Tokens remain bound to the issuing origin, so an
upstream token is never sent to a mirror. Redirects are followed manually only
over HTTPS on the same origin and within the request/redirect budgets.

Each request timeout is `min(probe_timeout_ms, 60s)`. The total deadline is
`min(request timeout × 12, 5 minutes)`; with the default 1500 ms setting this is
1.5 seconds per request and 18 seconds overall. The run permits at most 12
requests and three redirects per request chain, bounds API/token/manifest bodies,
and reads at most the 16 KiB layer sample. `--offline` rejects this network
diagnostic before constructing its transport. `resolve` and `anonymous_only` do
not relax this command: upstream remains authoritative and the test remains
anonymous.

Human output is localized and conclusion-first. `--json` emits registry report
schema version 1 with typed API, manifest, blob-range, and ordered mirror checks.
It includes image names, platforms, digests, byte counts, registry origins, and
request count, but never tokens, raw response headers, or response bodies. For a
path-prefixed configured mirror, requests retain the prefix before `/v2/...`;
the diagnostic report deliberately shows only its origin.

## Plan native mirror configuration

```text
osdk container mirrors plan REGISTRY
  --runtime docker|containerd|buildkit
  [--builder NAME]
  [--native-config PATH]
  [--containerd-main-config PATH]
  [--json]
```

The positional registry must have a matching `[containers.registries.<registry>]`
policy. `--runtime` is required—there is no `auto` mode—and one invocation plans
exactly that registry for exactly that native control plane. Other configured
registries are not folded into the plan. `--builder` is valid only with
`--runtime buildkit`; when omitted there, it uses effective
`[containers].builder`.

`--native-config` identifies the exact bounded input/target: Docker's daemon
JSON, containerd's `<config_path>/<registry>/hosts.toml`, or the selected
BuildKit builder's `buildkitd.toml`. If it is omitted, a plan that could otherwise
be locally actionable is downgraded to `manual-only` and has no generated
candidate. For containerd, `--containerd-main-config` is accepted only when the
discovered configuration has no active registry `config_path`; it identifies the
main containerd TOML that would need that path. Supplying it when `config_path`
already exists is an error.

```bash
osdk container mirrors plan ghcr.io --runtime containerd \
  --native-config /etc/containerd/certs.d/ghcr.io/hosts.toml --json

osdk container mirrors plan docker.io --runtime buildkit \
  --builder team-builder --native-config ./buildkitd.toml --json
```

Runtime behavior is intentionally not flattened:

| Runtime | Plan boundary | Path-prefixed mirror |
| --- | --- | --- |
| Docker | Only `docker.io` is supported. A local/rootless target with `resolve=mirror` and an explicit daemon JSON can be `ready`; `resolve=upstream` is `manual-only` because Moby cannot separate origin resolution from mirror transfer. Applying the described change would require a daemon restart. | Rejected because Moby `registry-mirrors` accepts origin URLs only. |
| containerd | Plans the exact registry namespace in `hosts.toml`. `resolve=upstream` grants mirrors `pull`; `resolve=mirror` grants `pull, resolve`; `push` is never added. Existing unrelated host/TLS entries are preserved in the in-memory candidate. | Preserved as the full host-table URL. It remains a base prefix before containerd appends `/v2/...`; osdk therefore does not infer `override_path`. |
| BuildKit | The Docker driver is `unsupported` because it uses Engine policy. A local `docker-container` builder can be `ready` with `resolve=mirror` and an explicit TOML; it reports `recreate-builder`. Kubernetes, remote, cloud, and upstream-resolution cases are `manual-only`. | Rendered in the non-serialized candidate TOML without the `https://` scheme as `host[:port]/path`, preserving the path. |

The human report starts with the deterministic `sha256:` `plan_id`, then
applicability, change/candidate counts, activation requirement, and warnings.
`--json` emits mirror-plan schema version 1. The ID binds the canonical target,
policy, input and candidate fingerprints, semantic changes, privilege, activation,
validation steps, and warnings; changing those semantics changes the ID.

Plan JSON is operational metadata, not an anonymized report. It serializes
canonical absolute native-config paths, the selected Buildx builder name, and
containerd namespace/config path. For mirrors it exposes only a redacted origin
and `has_path_prefix`; the exact prefix is replaced by a redaction marker. Review
the output before sharing. The exact prefix itself is never serialized: the
policy fingerprint always binds it, and when a candidate exists its hidden bytes
and candidate fingerprint bind it as well. JSON stores input state, sizes, and
SHA-256 fingerprints and candidate size/format/fingerprint, but not existing
native config contents or generated candidate bytes.

`mirrors plan` performs only bounded, no-follow reads and native discovery. It
does not write a file, elevate privileges, restart a daemon, recreate a builder,
or apply the plan. There is no apply option in this command.

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

These commands are read-only. They do not pull complete images, prune caches,
rewrite daemon configuration, start or recreate builders, restart daemons, or
inspect implementation-private store directories. Registry tests perform only
the bounded metadata and Range reads described above. See
[Container diagnostics implementation](./implementation/containers) for the
probe, planning, and disclosure boundaries.
