# Implementation internals

This section is for developers who need to audit, extend, or debug osdk. It breaks the current implementation down by capability. The user guide explains how to use a feature; these pages explain how a request is resolved, verified, and committed, including what each layer deliberately does not guarantee.

## The main architecture path

A typical archive-backed SDK installation crosses these boundaries in order:

```text
CLI and layered configuration
  -> project version discovery, alias expansion, and exact resolution
  -> backend source candidates and InstallPlan
  -> download cache and resumable transfer
  -> checksum / signature / optional attestation verification
  -> path-constrained extraction into scratch space
  -> BLAKE3 content-addressed store (CAS)
  -> hardlink / reflink / copy materialization
  -> receipt and completion marker
  -> shims, activation environment, and project lock
```

Not every backend is required to reuse this path literally. The uniform interface lets delegate backends such as Rust invoke a controlled upstream manager, while model assets use a separate provider, manifest, and snapshot flow. The common contracts live in the [`Backend` trait](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs), the [installation pipeline](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs), and the [CLI orchestration layer](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs).

## Read by capability

| Page | Focus |
| --- | --- |
| [Version resolution](./resolution) | Configuration discovery precedence, aliases, ranges, prereleases, and package-manager discovery |
| [Installation pipeline](./installation) | Concurrent orchestration, download, verification, extraction, commit, and failure cleanup |
| [Activation, shims, and lockfiles](./activation-lockfile) | Hot-path resolution, reversible shell state, project trust, and platform-aware locks |
| [Download sources and project registries](./sources-registries) | Two control planes, probe ranking, credential boundaries, and single-launch behavior |
| [HTTP artifact backend](./http-artifacts) | Strict inline URL parsing, exact identity, SHA-256 cache replay, safe extraction, and publication |
| [Native container diagnostics and operations](./containers) | Runtime selection, anonymous OCI validation, direct pull launch, native plan/preview identity, and cache ownership |
| [Storage and caches](./storage-cache) | SDK/model CAS, materialization fallbacks, download cache, and manager-native caches |
| [Verification and supply-chain boundaries](./verification) | Checksums, Minisign, GitHub Artifact Attestations, and archive-safety boundaries |
| [Backends and model providers](./backends-models) | Built-in, declarative, and GitHub backends plus Hugging Face and ModelScope snapshots |
| [npm developer tools](./npm-tools) | `npm:<package>` identity, project/global/isolated installers, script policy, native locks, inventory, and conflict rejection |
| [Cargo developer tools](./cargo-tools) | `cargo:` identity, exact Rust binding, controlled providers, native publication, and schema-4 replay metadata |
| [Go developer tools](./go-tools) | `go:` command-package identity, exact Go binding, proxy selection, isolated provider execution, and schema-4 replay metadata |
| [Reliability and concurrency](./reliability) | Locks, atomic publication, retries, offline fallback, idempotency, and GC boundaries |

## Boundaries to remember

- Lockfiles and installation receipts are reproducibility inputs and audit records, not trust anchors; floating Rust channels remain rustup channel names rather than immutable versions. Reinstallation from cached or locked artifacts still applies the active checksum/attestation policy. The normal CLI reuses an already complete installation before entering the pipeline and does not rehash its checksum; only an invocation that actually reaches the pipeline can reverify requested attestation on that fast path.
- GitHub Artifact Attestation policy defaults to `off` and applies only to GitHub artifact flows that can supply the required bundle and identity constraints.
- osdk's BLAKE3 CAS deduplicates verified SDK and model files. Native package caches remain manager-owned; Cargo tools build in a private staged Cargo home/target, while Go tools use osdk-controlled shared module/build caches. Both publish binaries outside CAS. There is currently no cross-manager package tarball CAS.
- `osdk cache clean` clears only osdk's download cache. It does not remove native package caches, installations, models, or the CAS.
- `osdk container cache status` queries runtime-owned aggregate interfaces.
  Native image pulls remain owned by the selected Docker or containerd control
  plane; scoped pruning is supported only for Docker and BuildKit. None of these
  paths reads or creates an osdk OCI store or scans implementation-private native
  stores.
- SDK source selection and project dependency registry selection are separate mechanisms. Registry preflight only chooses the environment before one launch: the manager runs at most once, and if every candidate is unhealthy the operation fails closed without starting it.
- Installation locks narrow contention to specific objects. CAS object publication, manifest publication, and garbage collection are not covered by one global serialization boundary; interpret the GC safety model as documented in [Storage and caches](./storage-cache).

## Code map

Start with the core crate map in [`osdk-core/src/lib.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/lib.rs). CLI arguments and command dispatch live in [`cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/cli.rs) and [`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs); the cross-process launcher starts in [`osdk-shim`](https://github.com/lejunyang/one-sdk/tree/main/crates/osdk-shim). Source code and tests are authoritative for implementation claims; research reports retain design context and historical decisions.
