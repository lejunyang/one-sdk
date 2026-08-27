# Docker image acceleration and OCI cache implementation report

Date: 2026-08-28

Project: `github.com/lejunyang/one-sdk`

Status: design recommendation; no implementation is implied by this report

## Executive decision

Docker and OCI acceleration should be delivered in two deliberately separate
layers.

1. **Ship native runtime integration first.** `osdk` should discover Docker
   Engine, containerd, and BuildKit independently; explain their effective
   registry configuration; test candidate paths; render safe configuration
   plans; and delegate pulls, builds, cache reporting, and pruning to the native
   owner. Docker Engine, containerd, and BuildKit must continue to own their
   content stores, credentials, unpacked snapshots, leases, and garbage
   collection.
2. **Add an `osdk`-managed OCI store only as an experimental second product.**
   Its first useful form should be anonymous, public, digest-pinned fetch into
   an OCI image layout, followed by pin/unpin, export/import, resumable transfer,
   and graph-aware GC. A local read-only proxy may be added only after those
   primitives pass the security and concurrency gates in this report.

A general authenticated pull-through proxy is a registry product. It must solve
repository-scoped authorization, bearer-token challenges, credential-helper
integration, mutable-tag freshness, content negotiation, multi-platform
selection, referrers, quota, leases, GC, SSRF prevention, and privacy between
callers. It is therefore explicitly outside the initial implementation.

The most important product constraint is that there are several independent
control planes:

- Docker Engine runtime pulls;
- containerd pulls, including the CRI namespace and configuration used by a
  Kubernetes node;
- each BuildKit or Buildx builder's base-image pulls;
- BuildKit build-cache import/export;
- and a future `osdk` OCI cache.

A successful test of one plane says nothing conclusive about the others. In
particular, Docker Engine's `registry-mirrors` is a Docker Hub mechanism; it is
not a generic `ghcr.io` host rewrite.

## Goals

The proposed capability should:

- make slow and unreliable public image pulls diagnosable;
- configure the correct native control plane without hiding what changes;
- distinguish Docker Hub mirrors from GHCR and other registry proxies;
- preserve native credentials and never leak them to an acceleration endpoint;
- resolve and report immutable digests and target platforms;
- fail closed on size, digest, media-type, signature, and metadata corruption;
- make native pruning explicit and make managed-cache GC lease- and graph-aware;
- retain signatures, attestations, SBOMs, and other OCI referrers;
- work predictably in offline and degraded-network modes; and
- provide a staged path from diagnostics to a local, read-only OCI cache without
  requiring `osdk` to become a container runtime.

## Non-goals

The implementation should not initially:

- replace Docker Engine, containerd, CRI, BuildKit, or their snapshotters;
- unpack layers, construct root filesystems, or run containers;
- push, delete, retag, or mutate content in an upstream registry;
- implement a transparent TLS interception proxy;
- proxy private registries or forward upstream credentials;
- rewrite Dockerfiles, Compose files, Kubernetes manifests, or lockfiles
  silently;
- treat BuildKit build-cache export/import as equivalent to image-pull caching;
- scan or delete files inside a native runtime's private storage directories;
- automatically run `docker system prune`, `docker image prune`,
  `docker buildx prune`, or a containerd equivalent;
- provide a highly available team registry, replication service, or cross-site
  cache cluster; or
- weaken digest or signature checks to make a broken mirror appear usable.

For a durable LAN or organization-wide cache, `osdk` should integrate with and
diagnose a maintained registry product such as Harbor, Distribution, or zot
rather than embedding that operational surface in the CLI. Harbor explicitly
supports GitHub Container Registry as a proxy-cache endpoint type.

## Terminology and trust boundaries

This report uses these terms precisely:

- **Origin registry**: the authoritative registry named by the image reference,
  such as `docker.io` or `ghcr.io`.
- **Mirror**: an alternate endpoint selected by a native client for an origin
  namespace. Whether it may resolve tags is runtime-specific.
- **Proxy cache**: a registry-shaped service that fetches from an origin and
  stores responses for later requests. Its client-visible repository namespace
  may differ from the origin namespace.
- **Build cache**: BuildKit records used to reuse build steps. It is not the same
  thing as cached manifests and layers for a base image.
- **Resolution**: mapping a mutable tag to a manifest or index digest. This is a
  security-sensitive operation.
- **Content fetch**: retrieving bytes already named by a digest. A digest checks
  byte integrity, not publisher identity.
- **Referrer**: a manifest whose `subject` points to another manifest, commonly
  used for signatures, attestations, SBOMs, and provenance.
- **Lease**: a temporary or durable root that protects in-use content from GC.

The source identity, tag resolution, content transport, publisher trust, and
retention decision are separate controls. A mirror can safely supply exact
digest-addressed bytes without being trusted to decide what digest a tag means.

## Runtime and builder capability matrix

| Control plane | Mirror/proxy reach | Configuration and lifecycle | Store and GC owner | Recommended `osdk` role |
| --- | --- | --- | --- | --- |
| Docker Engine | `registry-mirrors` is documented for Docker Hub pulls | Daemon-wide `daemon.json` or daemon flags; normally privileged and may require reload/restart | Docker Engine | Inspect, test, render a merge plan, optionally apply after confirmation |
| containerd | Per-registry namespace, including `ghcr.io`, through `hosts.toml` | `config_path` selects a hosts directory; `hosts.toml` changes normally do not require a daemon restart | containerd content store, image metadata, snapshots, leases, GC | Prefer for arbitrary registry-specific mirror policy; preserve capabilities and namespace behavior |
| BuildKit/Buildx | Per-registry mirrors in `buildkitd.toml`, including non-Hub registries | Builder-specific; different Buildx builders may use different drivers and configs | The selected BuildKit worker | Discover the selected builder and configure/test it separately from runtime pulls |
| Harbor/Distribution/zot | Product-dependent upstream proxy support; Harbor includes GHCR | Independent service with TLS, authentication, storage, monitoring, and upgrades | Proxy registry | Treat as an external endpoint and validate its behavior |
| Future `osdk` OCI store | Initially anonymous public, digest-pinned fetch; later experimental read-only proxy | User-scoped `osdk` storage and optional loopback service | `osdk`, using explicit roots and leases | Experimental only after native integration is complete |

### Docker Engine: Docker Hub is the special case

Docker documents `registry-mirrors` as a way to mirror the Docker Hub library. A
daemon configuration such as this can accelerate `ubuntu:24.04` or
`docker.io/library/ubuntu:24.04`:

```json
{
  "registry-mirrors": ["https://mirror.example"]
}
```

It does **not** transparently map `ghcr.io/org/image` to a GHCR cache. Therefore
the following must be reported as distinct outcomes:

- `docker.io`: a Docker Engine mirror plan is supported;
- `ghcr.io`: Docker Engine has no equivalent per-host mirror map; use an
  explicitly rewritten proxy reference, a registry product's proxy namespace,
  containerd configuration, or builder-specific BuildKit configuration;
- a daemon HTTP(S) proxy: this changes outbound network transport and is not an
  OCI mirror mapping; it is an advanced externally managed option.

Reference rewriting, for example from `ghcr.io/org/image@sha256:...` to
`harbor.example/ghcr/org/image@sha256:...`, may preserve manifest and layer
digests while changing registry and repository identity. Repository-scoped
authorization, admission rules, allowlists, and signature identity policies may
therefore behave differently. `osdk` must show the rewritten reference instead
of presenting it as transparent.

Docker Engine downloads three layers concurrently by default. Lowering
`max-concurrent-downloads` can help lossy or low-bandwidth links, but it is a
daemon-global performance setting. `osdk container doctor` may recommend a
change based on evidence; it must not silently change it. More concurrency is
not always faster because registry throttling, WAN loss, decompression, and disk
write pressure can dominate.

### containerd: separate `pull` from `resolve`

Current containerd registry configuration uses a hosts directory containing
per-namespace `hosts.toml` files. CRI points to it with `config_path`; the exact
plugin table differs between containerd 1.x and 2.x. `osdk` must inspect the
running version and effective config instead of assuming one path. It should
diagnose the deprecated inline CRI `registry.mirrors` and `registry.configs`
form, but not generate it for new installations.

Containerd capabilities are trust declarations as well as protocol features:

- `pull` permits fetching manifests and blobs;
- `resolve` permits mapping tags to digests;
- `push` permits uploads.

An accelerator that is not trusted to decide tag identity should have only
`["pull"]`; the authoritative upstream should resolve the tag, after which
the mirror can supply digest-addressed bytes. A fully trusted proxy cache may
receive `["pull", "resolve"]`. `push` should remain on the authoritative
registry unless the user explicitly configures a different publication model.

The adapter must preserve host order, `server`, capabilities, CA and client
certificate settings, `override_path`, and namespace semantics. A proxy host may
receive the original namespace in the `ns` query parameter; `ns` is routing
context, not authorization. `override_path` is for registries with a nonstandard
API root and must not be enabled for an ordinary `/v2` endpoint.

### BuildKit: the selected builder is its own control plane

BuildKit accepts per-registry mirror configuration in `buildkitd.toml`:

```toml
[registry."docker.io"]
mirrors = ["mirror.example"]

[registry."ghcr.io"]
mirrors = ["ghcr-cache.example"]
```

The configuration belongs to a BuildKit daemon. Buildx can select among
`docker`, `docker-container`, `kubernetes`, `remote`, and cloud-backed builders,
each with a different ownership and configuration boundary. Editing the host
Docker daemon does not prove that a `docker-container` or remote builder uses
the same mirror. The diagnostic path must at least record:

- active Docker context;
- selected Buildx builder and node;
- driver and endpoint;
- reported platforms;
- effective or attached `buildkitd.toml`;
- registry mirror and TLS settings; and
- BuildKit worker disk usage and GC policy.

BuildKit's `reservedSpace`, `maxUsedSpace`, `minFreeSpace`, age, and record
filters are native GC policy. Build-cache backends such as `inline`, `local`,
`registry`, and `gha` import or export solver cache records. They can improve
repeated builds even when every base-image layer still comes from the origin,
and a registry mirror can improve base-image transport without preserving a
single build step. The CLI and documentation must never call these equivalent.

## Fit with the current `osdk` architecture

The repository already contains useful reliability primitives, but container
images require a new domain rather than another SDK backend.

| Existing area | Reusable idea or code | Boundary for container support |
| --- | --- | --- |
| `config/mod.rs` | Layered CLI/environment/project/user configuration, typed settings, and project trust inputs | Add a separate `[containers]` section; do not put OCI registries under SDK `[sources]` or npm `[registries]` |
| `source/mod.rs` and `source/select.rs` | Bounded probes, cached rankings, source fingerprints, pins, offline behavior | OCI mirrors have registry namespace, auth, and `resolve` trust semantics; configured policy order must not be silently latency-reordered |
| `pipeline/download.rs` | `.partial` state, `Range` plus `If-Range`, exact `206` checks, `416` restart, retries, streaming, atomic rename | Partial identity must include registry/repository/digest and final publication must additionally verify descriptor size and OCI digest |
| `store/mod.rs` | Atomic-ish content publication, deduplication, manifest roots, and fail-closed GC on corrupt manifests | The current store hashes extracted files with BLAKE3; OCI requires exact raw descriptor bytes under the descriptor's algorithm and needs graph roots plus leases |
| `cache/mod.rs` | Respect for native manager-owned cache formats rather than pretending every ecosystem shares one CAS | Docker, containerd, and BuildKit stores should likewise remain native-owned |
| `http/mod.rs` | Bounded redirects and removal of explicit headers after an origin change | OCI authentication adds challenge realms, repository/action scopes, token lifetime, and stricter cross-origin policy |
| `verification/mod.rs` | Fulcio/SCT, DSSE, Rekor SET/checkpoint/Merkle proof, signing-time checks, cached bundles, and `off`/`if-available`/`required` policy precedent | Current evidence is GitHub release-artifact-specific; OCI discovery and Cosign/Notation payload binding need a dedicated verifier |
| `cli.rs` and `commands.rs` | Existing `doctor`, `source test`, `cache`, `prune --dry-run`, confirmation, and subprocess patterns | Add one `container` command family so runtime state is not confused with the SDK CAS |
| `docs/package-registry-design.md` | Anonymous bounded probing, trust boundaries, credential non-forwarding, fail-closed preflight, and exactly-once delegated execution | OCI token challenges and registry/repository identity are additional requirements |

### Why the SDK CAS cannot store OCI objects directly

The current `Cas` hashes each **extracted regular file** with BLAKE3 and
materializes installations with hardlink, reflink, or copy. An OCI descriptor
instead names the **exact encoded bytes** of an index, manifest, configuration,
compressed layer, signature, or attestation, normally with `sha256`.
Decompressing a layer, changing JSON bytes, or repacking an archive changes its
registry digest.

The two stores may share low-level filesystem helpers, locking utilities, and
atomic-publication patterns. They must not share an object namespace or claim
that a BLAKE3 extracted-file object is the OCI descriptor it came from. The OCI
store also needs roots, reference traversal, leases, tag-resolution metadata,
and referrer relationships that the current install-manifest GC does not model.

The current CAS correctly refuses GC when a referenced install manifest is
corrupt. That fail-closed rule should carry over. Its current static-root model
and lack of an OCI ingest lease, however, are insufficient for concurrent
fetch/proxy/GC operation.

### Why this is not an SDK `Source`

An SDK source chooses where `osdk` downloads one known tool artifact. An OCI
registry resolves repository-scoped references into a graph, can require a
challenge-token exchange, negotiates media types, and exposes related artifacts.
Likewise, `[registries.npm]` controls delegated project package operations and
must remain separate. The new `[containers]` section is a third network plane.

## Recommended architecture

```text
                         osdk container ...
                                |
             +------------------+------------------+
             |                                     |
       inspect / plan / test                  experimental OCI client
             |                                     |
    +--------+---------+---------+          reference + platform resolver
    |                  |         |                    |
 Docker Engine     containerd  BuildKit         registry protocol
 native store      native CAS  worker cache       /         \
 credentials       snapshots   native GC     OCI blob store  referrers/verify
 leases + GC       leases + GC                  roots + leases + GC
```

The native adapters and the managed OCI client share reference parsing,
platform types, redaction, diagnostics, and policy. They do not share ownership
of runtime storage.

## Native integration design

### Discovery

Discovery is read-only and version-aware. It should never infer one runtime from
the mere presence of a binary. The implementation should collect both client
and server information where available.

For Docker Engine:

- locate `docker` and query the selected context;
- distinguish local, SSH, TCP, and Docker Desktop endpoints;
- record server OS/architecture, Engine version, rootless/Desktop mode, and
  registry mirror information reported by the daemon;
- locate a daemon configuration file only when it is local and unambiguous; and
- report that a remote context must be configured on the remote host.

For containerd:

- locate the daemon and client, record versions, address, and namespace;
- distinguish direct `ctr` behavior from CRI behavior;
- read the effective plugin version and `config_path`;
- identify the namespace-specific `hosts.toml` and `_default` fallback; and
- flag legacy inline registry configuration without rewriting it.

For BuildKit/Buildx:

- query the selected builder unless `--builder` is supplied;
- enumerate nodes, drivers, endpoints, status, and platforms;
- determine whether configuration is local and editable; and
- never attribute host-daemon mirrors to an independent builder without proof.

Detection should return `not-installed`, `client-only`, `unreachable`,
`permission-denied`, and `unsupported-version` as distinct statuses. An
unreachable daemon is not evidence that its config is absent.

### Registry-path testing

`osdk container registry test REGISTRY` should be anonymous by default and
bounded. An optional `--image` enables deeper checks without selecting a large
arbitrary repository. A test report should include:

1. DNS addresses, connection target, TLS peer name, CA result, and timing.
2. `/v2/` response: success or a syntactically valid `401` bearer challenge is
   registry reachability; a login page is not.
3. Challenge realm, service, and requested repository scope after redaction.
4. Manifest `HEAD`/bounded `GET`, negotiated media type, returned
   `Docker-Content-Digest`, byte size, and digest verification.
5. Whether a tag is resolved by the origin or mirror.
6. A selected small blob's `HEAD`, bounded `Range`, `206`/`Content-Range`, and
   full-restart behavior.
7. Requested and selected platform plus index and child-manifest digests.
8. Mirror fallbacks, without forwarding credentials.
9. Referrers API support, pagination, and fallback availability when requested.
10. A clear distinction between authentication required, access denied, rate
    limited, not found, corrupt, and protocol-incompatible.

The command must cap response bodies, redirects, wall time, and downloaded
bytes. It must redact URL userinfo, query values, authorization headers, bearer
tokens, cookies, signed URLs, and credential-helper output. Diagnostic JSON must
follow the same redaction rules as human output.

### Planning native changes

`mirrors plan` should produce a semantic diff, affected scope, privilege
requirement, validation command, restart/recreate requirement, and rollback
path. It should not write. The plan must preserve unknown keys and existing
comments where the format permits it.

Docker Engine planning must:

- reject a `ghcr.io` request as unsupported by `registry-mirrors` rather than
  emitting a misleading daemon patch;
- merge only the requested `registry-mirrors` and optional explicitly requested
  download settings;
- reject conflicting command-line and file options reported by `dockerd`; and
- state whether the target is Docker Desktop, rootless Docker, a local service,
  or a remote context.

Containerd planning must:

- render the correct 1.x or 2.x `config_path` change when it is missing;
- render one namespace directory and `hosts.toml` without touching unrelated
  registries;
- make `pull` versus `resolve` visually explicit;
- preserve TLS and `override_path` settings; and
- state that hosts-directory edits normally do not need restart, while a new
  `config_path` may.

BuildKit planning must:

- name the exact builder and node;
- refuse to pretend a remote or cloud builder's local file is editable;
- render per-registry mirror/TLS changes and leave GC policy unchanged unless
  explicitly requested; and
- state whether the builder must be recreated or restarted with the config.

### Applying changes

Apply is a later phase and must require explicit consent, even when global
`--yes` is present in automation. Before a write, `osdk` should re-read the file
and compare it with the version used for the plan. It should write an adjacent
temporary file, validate the complete result, make a timestamped backup with
restricted permissions, atomically replace when the platform permits, and show
the exact service action still required. It must not elevate privileges by
itself.

Rollback must name the backup and be possible without `osdk`. Secrets, inline
auth, and private keys must never be copied into diagnostic output or a
world-readable backup. When comments cannot be preserved safely, the command
should stop and offer a rendered snippet for manual application.

### Delegating pulls and prune operations

Native `pull` is an exactly-once process operation. After `docker pull`,
`ctr`/CRI pull, or a BuildKit operation starts, `osdk` must preserve stdin,
stdout, stderr, signals, and exit status and must not replay the command against
another mirror. A failed pull may already have populated content or changed
runtime metadata. Mirror failover belongs to the native client before or during
that one operation.

Native cache status should use supported APIs or CLIs. Native prune should be a
separate explicit command with a dry-run/preview where the runtime can provide
one. `osdk` must never implement native cleanup by walking `/var/lib/docker`,
containerd roots, or BuildKit state directories. A preview must explain that
native prune can remove state created outside `osdk`.

## Managed OCI fetch and cache design

### Scope gate

The first managed implementation supports only:

- public registries on an explicit allowlist;
- anonymous requests;
- immutable digest references, or tags resolved online and immediately recorded
  as a digest;
- read-only `GET`/`HEAD` image content;
- selected-platform fetch, with all-platform fetch explicitly requested; and
- OCI image-layout output and import into a native runtime through its supported
  interface.

Private registries, arbitrary challenge realms, credential forwarding, push,
delete, cross-user service mode, and transparent daemon interception remain
disabled. This boundary should be enforced in types and routing, not only in
documentation.

### Reference parsing and canonical identity

Create a typed `ImageReference` that preserves the user's spelling for output
but resolves a canonical registry, repository, and tag or digest. It must safely
handle default Docker Hub expansion, ports, IPv6 literals, tags, and digest
algorithms, while rejecting URL syntax, credentials, query strings, fragments,
empty/path-traversal components, and unsupported digest algorithms.

Examples:

- `ubuntu:24.04` -> `docker.io/library/ubuntu:24.04`;
- `ghcr.io/org/app@sha256:...` remains registry/repository/digest;
- `registry.example:5000/team/app:v1` keeps the port as registry identity.

Cache keys must not be derived from raw, unsanitized reference strings. Blob
identity is the OCI digest. Tag-resolution identity includes canonical registry,
repository, tag, accepted media types, and authentication identity. The MVP has
only the `anonymous` identity. Authenticated caching must not ship until a
privacy-preserving identity partition has been designed.

### OCI graph and media negotiation

An image pull is a graph, not one archive:

```text
tag
 `-- resolves to index or manifest digest
      |-- image index / Docker manifest list
      |    `-- selected platform manifest
      `-- image manifest
           |-- config descriptor
           `-- ordered compressed layer descriptors

subject digest
 `-- referrer index
      `-- signatures / attestations / SBOM manifests and their blobs
```

Every descriptor has a media type, byte size, and digest. The fetcher must send
an explicit `Accept` set and initially support at least:

- `application/vnd.oci.image.index.v1+json`;
- `application/vnd.oci.image.manifest.v1+json`;
- `application/vnd.docker.distribution.manifest.list.v2+json`; and
- `application/vnd.docker.distribution.manifest.v2+json`.

Schema-1 images, unknown manifest semantics, foreign/non-distributable layers,
and artifact types with unsupported traversal rules must fail with a precise
error or be preserved opaquely only where the OCI layout permits safe transport.
No manifest JSON may be normalized before hashing or storage.

For each response, verify advertised bounds before allocation, read no more than
the configured limit, hash the exact bytes, compare the descriptor size and
digest, and only then publish. `Docker-Content-Digest` is useful evidence but
does not replace hashing the body. A mirror returning a stable wrong digest is
corrupt; retries must not suppress or override that result.

### Platform selection

The target platform belongs to the target runtime or builder, not necessarily
the machine running the `osdk` client. A remote Docker context may be Linux/ARM
while the CLI runs on Linux/AMD64. Resolution should use this order:

1. explicit `--platform OS/ARCH[/VARIANT]`;
2. selected builder or target runtime platform;
3. local host only when no remote target exists.

When the top descriptor is an index or manifest list:

1. verify and record the top-level bytes and digest;
2. recursively process nested indexes within a bounded depth;
3. match `os`, `architecture`, optional `variant`, and relevant `os.version` and
   `os.features`;
4. fail when no compatible entry exists;
5. fail when unknown or unsupported platform constraints make selection
   ambiguous, rather than silently choosing an apparently close image;
6. use deterministic index order only after every supported compatibility
   constraint has been evaluated;
7. fetch and verify the selected manifest, config, and ordered layers; and
8. persist both top-level and selected child digests.

`--all-platforms` is a separate opt-in because it multiplies network use, disk
use, referrer traversal, and verification work. Tests must include `amd64`,
`arm64/v8`, variant mismatch, nested indexes, and Windows `os.version` cases.

### Store layout

Use a new versioned store, not `<data>/store`:

```text
<data>/oci/v1/
  blobs/<algorithm>/<encoded>
  descriptors/<algorithm>/<encoded>.json
  roots/pins/<id>.json
  roots/layouts/<id>.json
  resolutions/<registry>/<repository-hash>/<tag>/<scope-hash>.json
  referrers/<registry>/<repository-hash>/<subject-digest>.json
  leases/<lease-id>.json
  locks/blobs/<algorithm>/<encoded>.lock
  locks/gc.lock

<cache>/oci/v1/
  partial/<algorithm>/<encoded>.partial
  partial/<algorithm>/<encoded>.json
  quarantine/<timestamp>-<digest>/...
```

Blob paths contain exact verified bytes. Descriptor records are immutable and
include media type, size, digest, and verified timestamp. Mutable operational
fields such as last access stay outside immutable blob and descriptor files.
Repository names may appear only through a safe encoded or hashed key, with the
canonical value inside validated metadata.

Exact digest blobs may deduplicate across registries and repositories after
successful authorization and verification. Resolution records, manifests whose
visibility is private, referrer discovery, errors, hit/miss reporting, and access
metadata remain registry/repository/auth-identity scoped. Physical deduplication
must not let one caller infer that another private repository contains a blob.
The anonymous-only MVP avoids this class of cross-identity leak.

### Resumable blob ingest

The current downloader supplies the right failure pattern but needs an
OCI-specific API. A partial is keyed by expected digest and scoped request
metadata, not merely by URL.

1. Acquire the per-digest ingest lock and re-check for a verified final blob.
2. Create or renew a temporary lease before writing content.
3. Record canonical origin, repository, expected descriptor, ETag or
   Last-Modified validator, byte count, and creation time in partial metadata.
4. If a compatible partial exists, request `Range: bytes=N-` with `If-Range`.
5. Append only after `206` and an exact, internally consistent
   `Content-Range`; otherwise truncate and restart.
6. On `200`, changed validators, impossible total size, or `416`, perform one
   safe full restart according to bounded retry policy.
7. Stream the response with maximum-size, deadline, redirect, and concurrency
   limits.
8. Hash the complete reconstructed bytes. Re-reading an existing partial before
   resumption is acceptable for the MVP; persistent hash state is not portable
   enough to trust blindly.
9. Require the exact descriptor size and digest. Move inconsistent data to a
   bounded quarantine record for diagnosis, not the live store.
10. Publish through an adjacent temporary file and atomic rename, fsyncing data
    and parent metadata where supported.
11. Commit the descriptor/reference metadata, then release the lease.

OCI Distribution says blob range support is a `SHOULD`, not a universal
guarantee. Full-restart fallback is required. Manifest and small JSON responses
should generally be fetched atomically rather than resumed.

### Tags, freshness, and offline behavior

A digest is immutable cache identity. A tag is a mutable lookup result. Store a
tag record with:

- registry and repository;
- tag and auth-scope identity;
- accepted media-type set;
- resolved digest and media type;
- resolution time, validator, and configured TTL; and
- source endpoint and whether the origin or a trusted mirror performed
  resolution.

Online use revalidates an expired tag. `--offline` may use a recorded tag only
when every selected graph object is present; output must state the recorded
digest, resolution timestamp, and staleness. It must not claim that the tag is
current. A digest reference can work offline whenever its required graph and
verification evidence are present. There is no hidden network fallback in
offline mode.

### Roots, leases, and garbage collection

Managed GC is graph traversal, not a directory-age sweep. Roots include:

- explicit `cache pin` records;
- exported/offline OCI layouts managed by `osdk`;
- retained tag resolutions within policy;
- verification evidence required for an active pin; and
- active fetch, export, import, verification, or proxy-request leases.

Traversal follows indexes, manifests, configs, ordered layers, and retained
referrers. An active lease protects partial and newly published objects before a
durable root exists. Leases have an owner, creation/renewal time, expiration, and
referenced descriptors. A crashed client's lease eventually expires; a live
operation renews before that point.

GC policy should support `max_bytes`, `min_free_space`, high/low watermarks, and
a minimum object age. It should evict unpinned least-recently-used roots until
the low watermark is met, then sweep unreachable blobs. A global GC/mutation
coordination mechanism must prevent collection between content publication and
root creation. Per-digest locks prevent duplicate publication but do not replace
that barrier.

If a root, descriptor, manifest, referrer index, or lease record needed for
reachability is corrupt, GC fails closed before deleting anything. Dry-run and
actual GC must use the same snapshot and traversal engine. Deletion errors are
reported exactly; reclaimed byte totals count only successful deletions. Empty
fan-out directories can be cleaned after the sweep.

Before the proxy phase, the metadata backend must demonstrate crash-consistent
multi-process transactions. Versioned files plus repository locks may be enough
for the fetch-only phase; an embedded transactional index is likely preferable
for a concurrent service, but that dependency and its Windows behavior need a
separate ADR and benchmark.

### OCI image-layout export and import

`fetch --output oci-layout:PATH` should write a standard OCI image layout with
`oci-layout`, `index.json`, and exact digest-addressed blobs. Publication should
use a sibling temporary directory and rename. An existing non-empty destination
is never overwritten without an explicit replacement option.

Export preserves the resolved top descriptor and selected-platform information.
With `--include-referrers`, it also exports the reachable signature, attestation,
and SBOM graph and records any compatibility fallback that cannot be represented
as a direct OCI-layout relation. Import verifies every descriptor before adding
it to the managed store or handing it to a runtime-supported importer.

The runtime adapter should prefer supported native import mechanisms. It should
not write directly into Docker/containerd/BuildKit storage.

## Authentication and security model

### Default policy

- Only anonymous public mirrors are automatic acceleration candidates.
- Private registries and authenticated mirrors use native pass-through.
- An upstream `Authorization` header, cookie, refresh token, Docker config
  entry, credential-helper result, or GHCR PAT is never forwarded to another
  origin.
- A proxy that needs client authentication is a separate registry identity; the
  user logs in to that proxy using the native tool.
- URL userinfo is rejected and query values are redacted.

Docker credentials may reside behind `credsStore` or per-registry
`credHelpers`, not as reusable JSON in `config.json`. `osdk` should not parse and
copy Docker auth material unless it later implements the credential-helper
protocol and an explicit, narrowly scoped authorization flow. Native delegation
allows Docker or another runtime to keep owning that interaction.

### Future authenticated-client requirements

If authenticated managed fetch is ever proposed, bearer tokens must remain
memory-only and be keyed at least by registry host, challenge realm, service,
repository, actions, and authenticated identity. The client must validate every
challenge realm and redirect, use HTTPS, restrict allowed realm origins, honor
token expiration, request least-privilege `pull` scope, and never persist token
responses. Different authentication identities must not share tag or visibility
metadata.

OCI Distribution intentionally does not define a complete universal auth
system. A registry may challenge with a separate token service. Supporting one
Docker-style bearer flow does not establish compatibility with every cloud
registry, identity provider, or credential helper.

### Pull-through credential hazard

The official Distribution mirror documentation warns that an upstream account
with private Docker Hub access can make all content visible to that account
available through the mirror unless the mirror has matching access control.
Harbor likewise notes that a proxy-cache endpoint credential can pull every
image that credential may access. `osdk` must surface this warning when
diagnosing an authenticated external cache and must never bootstrap one with a
broad personal credential.

### Proxy SSRF and routing controls

An experimental proxy binds to loopback and an ephemeral port by default. It
serves only configured upstream registry identities. It must:

- route from an exact configured Host/path or validated `ns` mapping, never an
  arbitrary URL supplied by the caller;
- reject userinfo, fragments, unapproved ports and schemes, and path traversal;
- resolve DNS under a rebinding-resistant policy and block loopback, link-local,
  metadata-service, multicast, and private destinations in `public-only` mode;
- validate redirect targets and challenge realms independently;
- cap headers, bodies, manifest graph depth, referrer pages, concurrency, and
  time;
- support read-only `/v2/`, manifest, blob, and referrers routes only; and
- emit no upstream secrets or sensitive URLs in logs, metrics, errors, or cache
  keys.

Listening on a LAN address, allowing private destinations, adding credentials,
or serving multiple trust domains moves the feature outside this embedded
public-cache design and should require an external registry product.

## Signatures, attestations, and referrers

Digest verification proves that received bytes match a selected digest. It does
not prove who selected or published that digest. A complete policy therefore
separates integrity from provenance.

The proposed verification policy is `off`, `if-available`, or `required`,
following the current CLI precedent. `required` is the production-safe mode for
publishers with a known signing contract. `if-available` is vulnerable to
downgrade when an untrusted mirror suppresses referrer discovery; authoritative
origin discovery or previously pinned verified evidence should therefore be
preferred.

For Cosign keyless verification, bind all of the following:

- the resolved subject digest;
- expected certificate identity or an anchored, reviewed identity expression;
- expected OIDC issuer;
- Fulcio chain and SCT;
- Rekor Signed Entry Timestamp, checkpoint, and Merkle inclusion proof or a
  valid offline bundle carrying equivalent evidence; and
- signing time and policy validity.

`verify-attestation` must additionally validate DSSE subject digest, predicate
type, and policy-specific claims. The mere existence of an attestation is not a
successful policy decision. Cached verification results are keyed by subject
digest, policy fingerprint, trust-root revision, verifier version, and evidence
digest. Offline verification replays cryptographic validation over cached
evidence; it does not trust a cached boolean.

OCI 1.1 uses `subject`, `artifactType`, and the Referrers API to discover
signatures, attestations, and SBOMs. A supply-chain-complete cache must preserve
and serve that graph, including pagination and artifact-type filtering. When a
registry returns `404` for the Referrers API, support the standardized referrers
tag fallback and, where required for Cosign compatibility, its legacy digest-tag
convention. Record which discovery path was used because tag-based fallback has
concurrent-update races.

For a multi-platform tag, the resolved top-level index is the primary subject. A
signature on an unrelated child does not authorize the index. Policy may also
require a signature for the selected child manifest, but the result must report
top-level and child decisions separately. Mirroring only layers and omitting
referrers must never be reported as a fully verified mirror.

The existing GitHub artifact verifier contains valuable Sigstore primitives but
is not directly reusable as an OCI verifier: its acquisition API, repository
identity, and DSSE claims are specific to GitHub release attestations. Extract
generic trust-root, bundle, Rekor, and signing-time helpers only after tests prove
that both callers retain their distinct policy bindings. Notation support is a
later policy provider; OCI referrers provide common transport, not common trust
semantics between Notation and Cosign.

## Proposed configuration

Container settings belong in their own top-level section:

```toml
[containers]
runtime = "auto"             # auto | docker | containerd
builder = "auto"             # auto or an explicit Buildx builder name
platform = "runtime"         # runtime or OS/ARCH[/VARIANT]
probe_timeout_ms = 1500
tag_ttl = "15m"

[containers.registries."docker.io"]
mirrors = ["https://mirror.example"]
anonymous_only = true
resolve = "upstream"         # upstream | mirror

[containers.registries."ghcr.io"]
mirrors = ["https://ghcr-cache.example"]
anonymous_only = true
resolve = "upstream"

[containers.cache]
mode = "native"              # native | managed-experimental
max_bytes = 53687091200
min_free_space = 10737418240
high_watermark_percent = 90
low_watermark_percent = 75
min_age = "1h"

[containers.verification]
policy = "if-available"       # off | if-available | required
include_referrers = true
```

Design rules:

- `[containers]` remains separate from `[sources]` and `[registries.npm]`.
- Project-level container configuration changes execution and network
  destinations, so it must pass the existing project trust gate.
- `runtime`, builder selection, registry mappings, `resolve`, TLS paths, proxy
  allowlists, and verification policy all participate in trust fingerprints.
- Credentials and bearer tokens are never stored here. Native credentials
  remain native.
- URLs must be HTTPS with a host and no userinfo, query, or fragment. Loopback
  HTTP may be allowed only for an explicitly started local proxy.
- `resolve = "upstream"` means a mirror is content-only. An adapter that
  cannot express separate resolution, notably Docker Engine's Hub mirror path,
  must reject or clearly downgrade that plan rather than claiming enforcement.
- Configured mirror order is policy order. Probe results are diagnostic by
  default. A future `selection = "fastest"` may reorder only a set of
  anonymous, equivalent, content-only endpoints with the same origin and trust
  policy; it must use a candidate fingerprint and never move an untrusted
  resolver ahead of the origin.
- CA and client-certificate **paths** may be future user-global options. Client
  key material must not be accepted from an untrusted project config.
- Unknown native runtime keys must survive planning and application. Unknown
  `osdk` container keys should produce a clear version/compatibility warning
  rather than being silently ignored once the schema is stabilized.

Environment overrides should initially be limited to non-secret selectors such
as `OSDK_CONTAINER_RUNTIME`, `OSDK_CONTAINER_BUILDER`, and
`OSDK_CONTAINER_PLATFORM`. Avoid a broad environment-variable surface for
mirror policy, TLS, or credentials.

## Proposed CLI

### Phase 1: inspection, testing, and plans

```text
osdk container doctor [--runtime auto|docker|containerd] [--builder NAME]
osdk container registry test REGISTRY [--image IMAGE] [--platform PLATFORM]
osdk container mirrors plan --runtime docker
osdk container mirrors plan --runtime containerd
osdk container mirrors plan --runtime buildkit --builder NAME
osdk container cache status [--runtime auto|docker|containerd|buildkit]
```

`doctor` reports runtime context, daemon endpoint/version/platform, rootless or
Desktop mode, containerd namespace and `config_path`, Buildx builder/driver,
effective mirrors, native store ownership, disk usage, GC settings, detected
legacy configuration, and any restart or privilege boundary.

`registry test` separates `/v2` reachability, auth challenge, tag resolution,
digest pull, Range behavior, media negotiation, platform selection, TLS, and
referrer support. It never prints secrets and never uses native private
credentials unless a later explicit authenticated mode is designed.

`mirrors plan` prints a semantic diff and machine-readable plan but performs no
write. Exit status distinguishes healthy, degraded, unsupported, invalid config,
and inaccessible target.

### Phase 2: explicit native operations

```text
osdk container pull IMAGE [--runtime ...] [--platform PLATFORM]
osdk container verify IMAGE[@DIGEST] --policy off|if-available|required
osdk container prune --runtime ... --dry-run
osdk container mirrors apply --runtime ... [--builder NAME]
```

`pull` prints the resolved digest when the native client exposes it. `verify`
resolves once, verifies the immutable subject, and reports top-level and selected
platform digests. `prune` requires a runtime-specific preview and confirmation.
`mirrors apply` consumes a just-generated plan, rejects stale inputs, validates
the result, and creates a rollback backup.

### Phase 3: managed OCI fetch and cache

```text
osdk container fetch IMAGE --platform PLATFORM --output oci-layout:PATH
osdk container fetch IMAGE --all-platforms --output oci-layout:PATH
osdk container cache pin IMAGE@DIGEST
osdk container cache unpin IMAGE@DIGEST
osdk container cache gc --dry-run
osdk container cache gc
osdk container export IMAGE@DIGEST --output oci-layout:PATH
osdk container import oci-layout:PATH --runtime docker|containerd
```

Tag input is allowed online but output always records the resolved digest. Cache
pinning requires a digest. Offline tag reuse visibly reports staleness. GC dry-run
and execution share one graph algorithm.

### Phase 4: experimental local proxy

```text
osdk container proxy serve \
  --listen 127.0.0.1:0 \
  --upstream docker.io \
  --public-only
```

The command remains visibly experimental. It is read-only, loopback-only by
default, and limited to an explicit upstream allowlist. It cannot accept or
forward credentials. Before serving a port, it checks that GC leases, quota,
request routing, redaction, crash recovery, referrers, and platform/media
fixtures have passed. For persistent or shared deployment it recommends an
external registry and exits unless an explicitly supported future service mode
exists.

All commands need English and Chinese help, errors, status labels, remediation,
and examples. Human-readable output should lead with the conclusion; stable JSON
output should include a schema version and structured redacted evidence.

## Module and ownership map

The container domain should be a sibling of backends, models, and package
registries:

```text
crates/osdk-core/src/container/
  mod.rs                 public domain API and feature gates
  reference.rs           image-reference parsing and canonicalization
  platform.rs            target-platform parsing and index selection
  runtime.rs             discovery types and native adapter trait
  docker.rs              Docker context/daemon inspection and config plan
  containerd.rs          versioned CRI/hosts.toml inspection and plan
  buildkit.rs            Buildx builder discovery and buildkitd plan
  registry.rs            bounded OCI Distribution client
  mirror.rs              registry test matrix and trust/capability policy
  auth.rs                challenge model and redaction; anonymous-only first
  verify.rs              digest, Cosign, attestation, and referrer policy
  oci_layout.rs          OCI image-layout import/export
  cache/
    mod.rs               store facade and paths
    metadata.rs          versioned descriptors, roots, resolutions, access data
    lease.rs             acquire, renew, expire, and release leases
    gc.rs                graph mark/sweep and quota policy

crates/osdk-cli/src/
  container.rs           command orchestration and output models
  cli.rs                 `container` command tree
  commands.rs            top-level dispatch only

crates/osdk-core/tests/fixtures/oci/
  ...                    pinned manifests, indexes, blobs, referrers, bundles
```

Supporting changes belong in:

- `config/mod.rs` for typed, layered `[containers]` configuration;
- `dirs.rs` for explicit OCI data/cache paths;
- `i18n/catalog.rs` for all English and Chinese strings;
- `trust.rs` for execution/network-affecting project settings; and
- paired README and VitePress documentation only when a user-facing phase is
  implemented.

Initially shell out to native CLIs for inspection and delegation, using the
existing process-control conventions. Do not add daemon SDK dependencies merely
to avoid subprocesses. Before implementing the managed registry client, evaluate
a maintained OCI protocol crate against authentication, redirect control, exact
byte access, referrers, maintenance activity, MSRV, and Windows GNU support. Wrap
it behind `registry.rs` so protocol-library choice does not leak through the
domain API.

## Test strategy

All tests must use temporary `HOME`, `DOCKER_CONFIG`, `OSDK_*`, runtime config,
store, cache, and build directories. Unit and integration tests must never edit
`/etc`, the user's Docker context, Docker Desktop settings, containerd state, or
Buildx builders. Runtime CLIs should be exercised through deterministic fixtures
or fake executables in narrow tests; opt-in live tests may use disposable daemons
and registries.

### Reference and platform tests

- Docker shorthand, explicit Docker Hub, GHCR, ports, IPv6, tags, digests, and
  canonical round trips.
- Reject credentials, URL query/fragment, traversal, malformed digests, and
  unsafe filesystem components.
- OCI and Docker media types, nested indexes, and bounded graph depth.
- `linux/amd64`, `linux/arm64/v8`, missing/wrong variant, unsupported ambiguity,
  Windows `os.version`/`os.features`, and no-match errors.
- Remote runtime platform wins over the client host; explicit `--platform` wins
  over both.

### Native adapter tests

- Docker Hub mirror plan succeeds and GHCR Docker Engine mirror plan is rejected
  with the correct alternative.
- Docker local, remote-context, rootless, and Desktop ownership are distinguished.
- containerd 1.x and 2.x plugin paths, namespace lookup, `_default`, `server`,
  `override_path`, TLS, and no-restart hosts changes.
- containerd untrusted mirror receives `pull` but not `resolve`; trusted mirror
  receives the explicit requested capabilities.
- deprecated inline CRI configuration is diagnosed, not generated.
- Buildx `docker`, `docker-container`, Kubernetes, and remote builders remain
  independent; the plan names the selected node and config boundary.
- Existing unknown JSON/TOML keys and comments survive apply where promised.
- Plan/apply detects a concurrent edit, validates before replacement, restricts
  backup permissions, and reports restart/recreate requirements.
- Delegated pull starts exactly once and preserves stdout, stderr, stdin, signals,
  and exit code.
- Prune defaults to dry-run/preview and cannot touch a native store directly.

### Registry protocol and corruption tests

Use a local fixture registry to cover:

- `/v2/` `200`, valid `401` challenge, `403`, `404`, `429`, and bounded `5xx`;
- manifest `HEAD` and `GET`, content negotiation, missing/incorrect
  `Docker-Content-Digest`, and oversized JSON;
- blob full pull and `Range` cases: valid `206`, ignored Range with `200`, `416`,
  malformed or wrong-start `Content-Range`, changed validator, truncated body,
  and inconsistent total size;
- descriptor digest, size, and media-type mismatch; corrupt data is never
  published;
- same digest across public repositories deduplicates to one blob;
- tag changes refresh after TTL and offline use reports a stale recorded digest;
- redirects remain inside policy; credentials and sensitive headers never cross
  origins;
- an auth challenge realm cannot redirect to an unapproved or private address;
- Docker Hub and GHCR path semantics; and
- bounded concurrency, retry, backoff, response size, and graph depth.

### GC and lease tests

- active lease protects partial and published-but-not-rooted content;
- released or expired lease becomes collectible;
- pins, layouts, retained tag records, indexes, children, configs, layers, and
  referrers form the correct reachability graph;
- shared blobs survive until the final root disappears;
- high/low watermarks, minimum age, maximum bytes, and minimum free space are
  enforced deterministically;
- simultaneous fetches of one digest publish once;
- GC cannot race between publication and root commit;
- crash recovery expires abandoned leases and cleans bounded partials;
- corrupt root, manifest, referrer, or lease metadata makes GC fail before any
  deletion; and
- dry-run and actual collection select the same objects from one snapshot.

### Signature and referrer tests

- valid Cosign key and keyless signatures;
- wrong certificate identity, wrong OIDC issuer, wrong repository, wrong subject
  digest, expired/invalid signing time, and untrusted root;
- missing, malformed, or tampered Rekor SET, checkpoint, and Merkle proof;
- valid and invalid DSSE predicate type and claim policy;
- cached bundle offline re-verification and policy/trust-root cache invalidation;
- `off`, `if-available`, and `required`, including mirror-suppressed referrers;
- native Referrers API, pagination, artifact-type filtering, standardized tag
  fallback, and legacy Cosign digest tags;
- top-level index and selected-child verification reported separately; and
- an unrelated platform child's signature cannot authorize the index.

### Security and service tests

- no Docker config, helper output, PAT, bearer token, cookie, or signed URL appears
  in logs, JSON, diagnostics, errors, cache keys, or backups;
- mirror and challenge redirects cannot receive origin credentials;
- proxy routing rejects arbitrary upstreams, encoded traversal, DNS rebinding,
  metadata-service targets, disallowed ports, and Host/`ns` confusion;
- default listener is loopback with a random available port;
- only read operations are accepted; upload, mount, delete, and catalog routes
  fail; and
- repository-scoped metadata cannot reveal private cache state.

### Required project validation for implementation phases

Each phase should run its narrowest relevant tests before its dedicated commit.
Before declaring any Rust implementation phase complete, run:

- formatting;
- focused unit and CLI integration tests;
- full workspace tests and Clippy;
- the MSRV check;
- installer and documentation checks when affected;
- VitePress production build for any documentation/navigation changes; and
- `./scripts/windows-wine-tests.sh`, as required by this repository for Linux
  validation of Rust code.

User-facing work includes synchronized `README.md`, `README.zh-CN.md`, Chinese
and English VitePress guide/implementation pages, navigation, CLI help, and i18n
catalog updates in the same feature commit.

## Phased implementation and acceptance gates

### Phase 0: contracts and fixtures

Deliver typed image references, platform matching, runtime discovery interfaces,
redaction, config schema, output schema, and a deterministic local OCI fixture
registry. No native configuration writes and no managed cache.

Acceptance:

- reference and platform fixtures cover Docker Hub, GHCR, multi-platform, and
  Windows cases;
- configuration merging and project trust are explicit;
- all diagnostic structures are secret-safe by construction;
- runtime adapters can be tested without a real daemon; and
- the official-source assumptions in this report are pinned in test comments or
  implementation documentation.

### Phase 1: native diagnostics and plans

Deliver `container doctor`, `registry test`, `mirrors plan`, and native cache
status. Keep all operations read-only.

Acceptance:

- Docker Engine plans a `docker.io` mirror but rejects transparent GHCR mapping;
- containerd reports and plans per-namespace `pull` versus `resolve`;
- BuildKit output proves which builder and driver were inspected;
- an anonymous GHCR test succeeds without reading or forwarding a PAT;
- registry tests report digest, platform, Range, TLS, and auth-challenge results;
- remote/unreachable/permission errors are distinct; and
- bilingual CLI/docs and Windows runtime tests pass.

### Phase 2: safe native apply and delegation

Deliver stale-plan-protected configuration application, exactly-once native pull,
native verification orchestration, and explicit native prune.

Acceptance:

- apply preserves unrelated configuration, validates the whole result, creates a
  protected backup, and reports restart/recreate steps;
- concurrent edits abort rather than overwrite;
- native credentials remain under the native client;
- delegated commands preserve streams, signals, and exit codes and run once;
- native prune cannot run without preview and explicit confirmation; and
- no test or implementation scans native private storage.

### Phase 3: digest-pinned OCI fetch, layout, and managed GC

Deliver anonymous public fetch, OCI-layout export/import, resumable blob ingest,
pins, leases, quota, dry-run GC, and offline use. There is still no HTTP proxy.

Acceptance:

- Docker Hub and GHCR fixture images fetch by digest;
- `amd64`, `arm64/v8`, and Windows selection produce recorded index and child
  digests;
- exact bytes are stored under OCI digests, separate from the BLAKE3 SDK CAS;
- valid `206` resumes and `200`/`416`/bad ranges restart safely;
- size, digest, media-type, or graph corruption never reaches the live store;
- cross-repository public blobs deduplicate without metadata-scope confusion;
- mutable tags revalidate and offline stale use is explicit;
- active leases survive GC and corrupt reachability metadata fails closed; and
- exported OCI layouts validate with an independent OCI tool.

### Phase 4: verification and supply-chain-complete cache

Deliver Cosign verification, policy-bound offline evidence, Referrers API and
fallback discovery, and optional referrer export.

Acceptance:

- good signatures and attestations pass; wrong issuer, identity, repository,
  digest, predicate, or Rekor evidence fails;
- `required` cannot be downgraded by a mirror omitting referrers;
- offline verification reruns cryptographic checks from cached evidence;
- Referrers API and legacy fallback fixtures both work; and
- multi-platform index and child decisions cannot be confused.

### Phase 5: experimental loopback proxy

Deliver the public-only, read-only, allowlisted loopback service. Keep it behind
an experimental flag and recommend an external registry for durable service.

Acceptance:

- exact Host/path/namespace routing works for Docker Hub and GHCR fixtures;
- no push/delete/catalog endpoint exists;
- no credentials can enter or leave the proxy;
- SSRF, redirect, DNS-rebinding, header/body, concurrency, quota, and crash tests
  pass;
- tags, manifests, blobs, platforms, Range requests, referrers, and signatures
  behave consistently through the proxy;
- concurrent fetch, lease renewal, and GC remain correct after forced crashes;
  and
- the feature refuses non-loopback or authenticated operation.

Authenticated proxying, multi-user isolation, LAN binding, and persistent service
installation require a new security review and product decision; they are not an
automatic Phase 6.

## Risks and mitigations

| Risk | Consequence | Required mitigation |
| --- | --- | --- |
| Confusing Docker Hub and GHCR | Configuration appears successful but does not accelerate the image | Runtime-specific capability validation and an explicit unsupported result |
| Configuring the wrong Buildx builder | Builds continue using the old path | Bind every plan to builder name, node, driver, endpoint, and config fingerprint |
| Native config overwrite | Daemon outage or lost custom settings | Semantic merge, complete validation, stale-input check, restricted backup, no implicit elevation |
| Credential forwarding | Registry account compromise or private-image exposure | Anonymous automatic paths only; origin-bound headers; native pass-through for private content |
| Mutable tag poisoning/staleness | Wrong image selected despite intact blobs | Origin-controlled resolution, TTL revalidation, digest pinning, explicit offline staleness |
| Mirror returns corrupt bytes | Pull failure or unsafe content | Exact size and digest verification before publication; quarantine; never bypass checksum |
| Wrong platform | Runtime failure or incompatible Windows base | Resolve against target runtime and evaluate variant/OS constraints |
| Signature downgrade | Valid bytes from an untrusted publisher | Digest-bound signature policy; authoritative referrer discovery; `required` mode |
| Referrers omitted | Signatures/SBOMs disappear through cache | Preserve and test Referrers API and fallback graphs |
| GC races with ingest | Live image becomes incomplete | Active leases plus global GC/mutation coordination and fail-closed traversal |
| Duplicated runtime storage | Disk use grows rather than shrinks | Native-first default; managed store opt-in with quota and import/export intent |
| Registry/protocol drift | A registry works differently after upgrade | Versioned fixtures, bounded compatibility layer, structured diagnostics, no silent fallback |
| Proxy SSRF | Access to internal or metadata services | Exact allowlist, public-address policy, DNS rebinding defense, redirect/realm validation |
| False performance promise | Mirror adds latency or throttling | Report measured phases and bytes; preserve policy order; do not equate reachability with speed |
| Cross-platform regressions | Windows CLI/config paths or subprocesses fail | Isolated Windows GNU Wine suite plus native CI and filesystem-safe keys |

## Decision summary

The implementation should begin with native inspection and configuration because
that produces immediate acceleration value while retaining mature runtime
semantics. Containerd is the strongest native option for arbitrary per-registry
mirror policy because it can separate `pull` from `resolve`. BuildKit must be
configured per builder. Docker Engine's mirror feature should be presented only
as Docker Hub acceleration; GHCR requires a different path.

The managed path becomes credible only after `osdk` can fetch and verify a
digest-pinned OCI graph into a separate store, select platforms correctly, resume
without publishing corrupt data, retain referrers, and protect active data with
leases. The proxy is the final experimental layer, not the starting point.

## Official references

The links below were reviewed for this report. Versioned OCI specifications are
preferred where available; containerd operational examples must still be matched
to the deployed containerd version.

### Docker Engine and credentials

- [Mirror the Docker Hub library](https://docs.docker.com/docker-hub/image-library/mirror/)
- [`docker image pull`](https://docs.docker.com/reference/cli/docker/image/pull/)
- [`dockerd` reference](https://docs.docker.com/reference/cli/dockerd/)
- [`docker login` and credential stores/helpers](https://docs.docker.com/reference/cli/docker/login/)
- [Docker CLI `config.json` properties](https://docs.docker.com/reference/cli/docker/#docker-cli-configuration-file-configjson-properties)
- [Prune unused Docker objects](https://docs.docker.com/engine/manage-resources/pruning/)
- [Docker credential-helper implementation and protocol](https://github.com/docker/docker-credential-helpers)

### containerd

- [Registry host configuration (`hosts.toml`)](https://github.com/containerd/containerd/blob/main/docs/hosts.md)
- [Versioned containerd 1.7 hosts documentation](https://containerd.io/docs/1.7/hosts/)
- [containerd CRI registry configuration](https://containerd.io/docs/2.3/cri/registry/)
- [containerd content flow](https://containerd.io/docs/2.2/content-flow/)
- [containerd garbage collection and leases](https://github.com/containerd/containerd/blob/main/docs/garbage-collection.md)

### BuildKit and Buildx

- [Configure BuildKit registry mirrors](https://docs.docker.com/build/buildkit/configure/#registry-mirror)
- [`buildkitd.toml` configuration](https://docs.docker.com/build/buildkit/toml-configuration/)
- [Upstream BuildKit daemon configuration](https://github.com/moby/buildkit/blob/master/docs/buildkitd.toml.md)
- [Buildx builders](https://docs.docker.com/build/builders/)
- [Buildx drivers](https://docs.docker.com/build/builders/drivers/)
- [Build garbage collection](https://docs.docker.com/build/cache/garbage-collection/)
- [Build cache storage backends](https://docs.docker.com/build/cache/backends/)
- [`buildctl` cache import/export](https://github.com/moby/buildkit/blob/master/docs/reference/buildctl.md)

### OCI image and distribution specifications

- [OCI Image Spec 1.1.1 descriptor](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md)
- [OCI Image Spec 1.1.1 image index and platform](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-index.md)
- [OCI Image Spec 1.1.1 manifest](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md)
- [OCI image-layout specification](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md)
- [OCI Distribution Spec 1.1.1](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md)
- [OCI Distribution pull](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#pull)
- [OCI resumable pull](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#resumable-pull)
- [OCI Referrers API](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#listing-referrers)
- [OCI referrers fallback](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#unavailable-referrers-api)
- [OCI registry proxying and credential boundary](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md#registry-proxying)
- [OCI 1.1 image and distribution release overview](https://opencontainers.org/posts/blog/2024-03-13-image-and-distribution-1-1/)

### Registry services and authentication

- [Docker Registry HTTP API V2](https://distribution.github.io/distribution/spec/api/)
- [Registry token authentication](https://distribution.github.io/distribution/spec/auth/token/)
- [Distribution pull-through cache](https://distribution.github.io/distribution/recipes/mirror/)
- [Distribution registry configuration](https://distribution.github.io/distribution/about/configuration/)
- [Distribution garbage collection](https://distribution.github.io/distribution/about/garbage-collection/)
- [Harbor supported registry endpoints](https://goharbor.io/docs/main/administration/configuring-replication/create-replication-endpoints/)
- [Harbor proxy cache](https://goharbor.io/docs/2.10.0/administration/configure-proxy-cache/)
- [GitHub Container Registry authentication](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry)

### Signatures and attestations

- [Cosign verification](https://docs.sigstore.dev/cosign/verifying/verify/)
- [Cosign signature specification](https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md)
- [`notation verify`](https://notaryproject.dev/docs/user-guides/cli-reference/notation_verify/)
- [Notary Project trust store and trust policy](https://github.com/notaryproject/specifications/blob/main/specs/trust-store-trust-policy.md)
- [Notary Project signature specification](https://github.com/notaryproject/specifications/blob/main/specs/signature-specification.md)
