# Backend and Model Provider Implementation

This page is for maintainers who need to understand or extend osdk's download capabilities. SDKs and models share networking, source selection, and CAS infrastructure, but use two distinct domain interfaces: SDKs implement `Backend`, while model repositories implement `ModelProvider`. Models are not special SDK backends in disguise.

## SDK backend contract

[`Backend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs) is the uniform boundary for every SDK. An implementation supplies a canonical ID, optional aliases, default sources, a probe URL, remote versions, installation logic, executable paths, and executable names. It may also override version resolution, uninstall, post-install behavior, activation environment, and idiomatic version files. The default resolver handles latest, prefix, range, and exact requests and always preserves `-o/--opt` options in `ToolVersion`.

`Ctx` carries directory layout, target platform, merged configuration, the HTTP client, CAS, and progress preference. Archive backends generally produce an `InstallPlan` and delegate to the [shared installation pipeline](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs):

1. obtain a best-first source list using a pin, configured order, or cached probes;
2. download into the shared cache and fail over in source order;
3. verify SHA-256, SHA-512, SRI, or authenticated attestation evidence;
4. safely extract into a scratch directory;
5. ingest content into the BLAKE3 CAS and materialize with hardlink, reflink, or copy;
6. write an artifact receipt and `.osdk-complete` marker for idempotent and offline reinstall behavior.

The [`Registry`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/registry.rs) registers built-in backends and aliases and recognizes `github:owner/repo`, `npm:<package>`, strict `http:https://...{version}...`, `cargo:<crate-or-https-url>`, and `go:<module-or-command-path>` IDs dynamically. It also loads declarative backends from `plugins/*.toml` in the user config and data directories. Duplicate IDs or aliases are rejected, so an external definition cannot shadow a built-in backend.

These dynamic namespaces share an option-identity contract for osdk-owned
installs. Before resolution or installation, osdk projects the supported public options into a
canonical map, rejects unknown public keys, excludes internal `__osdk_*` lock
replay metadata, and computes an order-independent, domain-separated BLAKE3
`b3-v2:` identity over `tool`, exact `version`, `platform`, `scope`, canonical
`material_options`, `dependencies`, and `materials`. `.osdk-install.json` schema 1
stores those fields in a nested `identity` together with `install_id`, and the
fingerprint is part of the physical root. Same-backend/version identities can
therefore coexist. Reuse, activation, shim execution, `where`, uninstall, and
`reshim` require the exact configured identity and never fall back to another
fingerprint. `.osdk-tool.json`, in either old schema 1 or 2 form, is legacy
detection only and cannot authorize reuse or execution. Project-managed npm
packages remain outside this osdk-owned install identity.

## Built-in backend matrix

| Backend | Resolution and acquisition | Integrity and installation semantics | Notable behavior or limitation |
| --- | --- | --- | --- |
| `node` (`nodejs`) | Node index; official, npmmirror, TUNA, and USTC archives | `SHASUMS256.txt`; shared archive pipeline | Optional `arch` and `corepack`; Corepack is a post-install action |
| `npm` | `npm` registry packument/tarball | npm SRI, always required; generates `npm`/`npx` launchers | Installed independently of Node, but needs an active Node at runtime |
| `pnpm` | Complete `pnpm` JavaScript distribution | npm SRI; osdk-generated Node launcher | Adds managed Node automatically; store variable depends on major version |
| `yarn` | `yarn` for 1.x, `@yarnpkg/cli-dist` for 2+ | npm SRI; generates Node launchers | Manages Classic and Berry directly instead of delegating to Corepack |
| `go` (`golang`) | go.dev JSON index; mirrors may reuse the official index | Per-file SHA-256; archive pipeline | Exports `GOROOT` |
| `python` (`py`, `cpython`) | Built-in PBS release index, Astral, and GitHub proxy | Per-release `SHA256SUMS` | CPython, PyPy, GraalPy, Pyodide, and variants; historical releases can pin `tag` |
| `java` (`jdk`, `openjdk`) | Foojay Disco API, defaulting to Temurin | Vendor checksum; archive pipeline | `distribution`, `package-type=jdk\|jre`; exports `JAVA_HOME` |
| `maven` (`mvn`) | Built-in single-release record | Fixed SHA-512 | Current catalog contains one version |
| `gradle` | Built-in single-release record | Fixed SHA-256 | Current catalog contains one version |
| `kotlin` (`kotlinc`) | Built-in single GitHub release, with proxy candidate | Fixed SHA-256 | Current catalog contains one version |
| `rust` (`rustup`) | rustup channel/version; official, rsproxy, and TUNA | SHA-256 for rustup-init, then delegated to isolated rustup | Toolchains bypass archive CAS; supports `profile`, `components`, and `targets`; exports isolated `RUSTUP_HOME`/`CARGO_HOME` |
| `deno` | `deno` packument plus `@deno/<platform>` | npm SRI | Platform package; exports `DENO_DIR` |
| `bun` | `bun` packument plus `@oven/bun-<platform>` | npm SRI | Platform package; exports `BUN_INSTALL_CACHE_DIR` |
| `npm:<package>` | npm packument; isolated installs use a managed npm subprocess, while project/global `use` can plan npm or pnpm | A native lock carries transitive integrity; scripts denied by default; `.osdk-install.json` schema 1 binds installer/build identity in fingerprinted osdk-owned isolated/global roots | Discovers `.bin` dynamically, adds managed Node, and records scope, installer, optional native-lock identity, and public options in lock schema 4 |
| `cargo:<crate-or-https-url>` | crates.io-compatible metadata with paired sparse index, or canonical HTTPS Git URL | Exact osdk-managed Rust dependency; isolated `cargo-binstall`/`cargo install`; native receipt, inventory, and metadata seal | Registry exact/latest/prefix or Git latest/tag/branch/full revision; schema 4 records runtime, replay class, and registry source |
| `go:<module-or-command-path>` | Go proxy `@latest`, version-list, and exact `.info` metadata, with longest-module-root discovery | Exact osdk-managed Go dependency; one isolated `go install`; native receipt, inventory, and metadata seal | Exact/latest/prefix/pseudo-version; schema 4 records runtime, `version-only`, selected proxy, and module root |
| `github:owner/repo` | GitHub API with Atom/public release-page fallback on rate limiting; optional static catalog | Checksums, optional minisign, GitHub artifact attestations; `.osdk-install.json` schema 1 binds asset/layout/material identity in fingerprinted roots | Selects a host asset; supports archives and bare binaries; regex/template/bin/rename/strip rules handle complex releases |

These implementations live under [`backend/`](https://github.com/lejunyang/one-sdk/tree/main/crates/osdk-core/src/backend/). The npm-backed implementations share packument, version, and SRI handling in [`npm.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm.rs). Generic source ranking is in [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs).
See [npm developer tool implementation](./npm-tools) for the complete dynamic
backend project/global/isolated installation, cache, metadata-only lock, legacy
lock-schema-2 sidecar compatibility, and shim boundaries.
See [Cargo developer tool implementation](./cargo-tools) for strict selectors,
exact Rust binding, controlled provider fallback, and native publication.
See [Go developer tool implementation](./go-tools) for module-root discovery,
proxy routing, build-environment policy, runtime binding, and replay boundaries.

## Declarative and GitHub backends

[`DeclarativeBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/declarative.rs) is a constrained schema-1 TOML extension point. It supports static or URL version lists, platform template variables, `tar.gz`/`tar.xz`/`tar.zst`/`zip`, fixed or remote checksums, `strip_root`, binary paths, and idiomatic version files. Platform templates expose both `{arch}`, osdk's short token, and `{arch_llvm}`, the CPU part of an LLVM target triple, because compiler and toolchain archives are normally published as `x86_64`/`aarch64` rather than `x64`/`arm64`; the latter reuses [`Arch::llvm_token`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/platform.rs) instead of introducing a second naming table. Definitions are limited to 1 MiB, remote lists to 10,000 versions, and URLs, filenames, relative paths, and checksums are strictly validated. It intentionally cannot execute hooks or arbitrary commands; every installation goes through the shared verification and CAS pipeline.
When a project lock supplies a generic artifact receipt, the backend consumes
the recorded URL, filename, checksum, and subdirectory before consulting its
current templates. This gives declarative tools the same metadata-free offline
reinstall contract as built-in archive backends.
An optional `[env]` table lets a definition describe the environment its
toolchain needs, which is what makes a compiler usable: build systems locate a
cross compiler through `CC`, `SYSROOT`, and similar variables rather than through
`PATH`. Values are rendered from `{install_path}`, `{version}`, and `{id}` only,
and `exec_env` fails closed if rendering would leave an unresolved placeholder.
Names are validated as conventional environment identifiers; `PATH` and the
dynamic-loader variables (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
`DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`) are reserved case-insensitively,
and absolute paths, `..`, and control characters are rejected at parse time, so a
data-only definition cannot point a child process outside its installation root.

[`GithubBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/github.rs) is a namespaced backend constructed at runtime. It reads up to 1,000 paginated releases, ignores drafts, applies prerelease policy, and scores assets for OS, architecture, and libc. Explicit rules handle non-standard asset names. Online, when signature verification is enabled, an available trusted minisign checksum manifest overrides a preloaded static digest; otherwise the static digest is used before ordinary sidecar/shared checksum discovery. The configured GitHub attestation policy is applied independently. GitHub API, page, Raw, release asset, and attestation URLs all use the same normalized source candidates, while credentials are sent only to the official API host.
Its supported asset, platform, catalog-digest, rename, bin, and strip options are
validated as public identity inputs and stored in the schema-1 dynamic install manifest.
`catalog-url` is accepted for acquisition but deliberately omitted because the
required `catalog-sha256` identifies content without persisting the catalog
location in the dynamic inventory. HTTP(S) catalog URLs containing userinfo,
query parameters, or fragments are rejected.
Consequently, a complete marker alone cannot reuse a GitHub install produced by
different options or by a legacy inventory. Locked replay additionally matches
the persisted artifact receipt's filename, checksum, and subdirectory. The
caller must uninstall and reinstall that version after a mismatch. Inventory is
published before the completion marker so an interrupted finalization cannot
be treated as reusable.

## Models are separate and provider-specific

A model reference must include a provider: `hf:owner/repo@revision` or `ms:owner/repo@revision`. [`ProviderId` and `ModelRef`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) make provider part of identity, and even the defaults differ: `main` for Hugging Face and `master` for ModelScope. [`ModelProvider`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/mod.rs) only standardizes the output as a resolved revision plus file manifest; it does not assume compatible service APIs.

| Semantic | Hugging Face | ModelScope |
| --- | --- | --- |
| Implementation | [`huggingface.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/huggingface.rs) | [`modelscope.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/provider/modelscope.rs) |
| Metadata API | `/api/models/{repo}/revision/{revision}?blobs=true` | `/api/v1/models/{repo}/repo/files?Revision=...&Recursive=true` |
| File URL | `/{repo}/resolve/{commit}/{path}` | `/api/v1/models/{repo}/repo?Revision=...&FilePath=...` |
| Immutable revision | Commit SHA returned by the service | Requested revision plus a BLAKE3 digest of the sorted path/size/SHA-256 manifest |
| File digest | LFS entries carry SHA-256; a missing regular-blob digest is computed after download | The API must return a valid SHA-256 for every file or resolution fails |
| Token | `OSDK_HF_TOKEN` → `HF_TOKEN` → `HUGGING_FACE_HUB_TOKEN`; Bearer | `OSDK_MODELSCOPE_TOKEN` → `MODELSCOPE_API_TOKEN`; Bearer plus `m_session_id` cookie |
| Default endpoint | `https://huggingface.co` | Prefer `https://modelscope.cn`, then `https://www.modelscope.ai` |

Consequently, **ModelScope is not a Hugging Face mirror implemented by swapping a base URL**. Metadata schemas, download URL construction, authentication headers, default revisions, and immutable snapshot derivation are different. Each provider implementation also rejects a `ModelRef` belonging to the other provider. Automatic ranking and failover remain inside one provider's endpoint set; osdk never silently substitutes a same-named repository from the other provider.

## Model resolution, download, and materialization

The CLI entry point for `osdk model pull <name> <reference>` is in [`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs), with the core flow in [`model/pull.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/pull.rs):

1. resolve an explicit `--endpoint` or provider endpoint environment variable; otherwise use that provider's default and custom sources;
2. in auto mode, [`model/source.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/source.rs) fetches a real repository manifest and performs a Range request of at most 1 MiB against the largest probeable file; ranking is cached by provider, repository, revision, and source configuration;
3. let the provider resolve its remote manifest, then apply `--include`/`--exclude` globs; `--variant` is a snapshot identity label only;
4. download selected files concurrently up to `settings.jobs` into a provider/repository/revision-separated cache, with resume support and size/SHA-256 verification;
5. have [`ModelStore`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs) verify again, ingest each file into the shared CAS, finish the snapshot in a hidden temporary directory, rename it to `<models>/<logical-name>/snapshots/<snapshot-key>`, and update `current.json` through another temporary-file rename; these renames have no portable replace-atomicity or durability guarantee;
6. unless `--no-lock` is used, record provider, repository, requested/resolved revision, endpoint, variant, and every file's size/SHA-256 in top-level `[models]` in `osdk.lock`. Tokens and short-lived download URLs are never persisted.

`model list/path/verify/remove` operate on the current logical name. Verification checks both the CAS BLAKE3 hash and SHA-256. Removal deletes all snapshots under that logical name, then runs CAS GC with SDK installs and models as roots. Offline pull still requires cached provider metadata and every selected download, after which it can rematerialize a removed snapshot.

## Provider environment persistence

`osdk model env enable [provider] [--force]` writes only `sources.<provider>.env` and optional `env_force` to the **user-level** config; project config cannot override those switches. Activation behavior lives in [`model/env.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/env.rs):

- Hugging Face exports `HF_ENDPOINT`, `HF_HOME`, `HF_HUB_CACHE`, `HF_XET_CACHE`, and `HF_ASSETS_CACHE`; osdk offline mode additionally exports the officially supported `HF_HUB_OFFLINE=1`.
- ModelScope exports `MODELSCOPE_ENDPOINT` and `MODELSCOPE_CACHE`; osdk does not invent a `MODELSCOPE_OFFLINE` variable.
- Existing user variables win by default; `--force` permits replacement. Shell activation captures original values so disable/deactivate can restore them.
- Custom endpoints default to `forward_credentials=false`. When osdk manages such an endpoint, it clears provider token variables and uses an isolated anonymous home to prevent local login cookies or tokens from leaking. Download requests carry credentials only for recognized official endpoints or after explicit `--forward-credentials`. Tokens are never stored in osdk configuration.

## Boundaries and caveats

- SDK locks are platform-keyed; model locks are top-level because model files are normally platform-independent. A model `variant` is a caller-supplied label: it neither infers a quantization format nor changes file selection.
- Provider identity is present in references, metadata/ranking/download caches, snapshot keys, manifests, and locks, so models are verified to be provider-specific. However, the top-level `models` map and local `current.json` are keyed by the caller's logical name. Pulling another provider under the same logical name switches that name's current snapshot and replaces its lock entry, although stored snapshots remain provider-distinct.
- A non-LFS Hugging Face blob may lack a server-provided SHA-256. osdk computes and locks one after download, but that is not an independent digest supplied by the service. ModelScope requires a valid SHA-256 in its API manifest.
- Online metadata failures may fall back to stale cache. A custom endpoint must implement the selected provider's actual API; hosting compatible files or replacing only the domain is insufficient.
- GitHub asset scoring is heuristic. Use explicit asset rules or a trusted static catalog when names are ambiguous or a release contains several similar artifacts.
- Rust is a delegate backend: isolated rustup owns the toolchain, so it does not get ordinary archive backends' per-file CAS deduplication. Maven, Gradle, and Kotlin currently expose a built-in one-version catalog rather than a complete remote version index.
