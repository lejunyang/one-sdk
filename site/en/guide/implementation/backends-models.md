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

The [`Registry`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/registry.rs) registers built-in backends and aliases and recognizes `github:owner/repo` and `npm:<package>` dynamically. It also loads declarative backends from `plugins/*.toml` in the user config and data directories. Duplicate IDs or aliases are rejected, so an external definition cannot shadow a built-in backend.

## Built-in backend matrix

| Backend | Resolution and acquisition | Integrity and installation semantics | Notable behavior or limitation |
| --- | --- | --- | --- |
| `node` (`nodejs`) | Node index; official, npmmirror, TUNA, and USTC archives | `SHASUMS256.txt`; shared archive pipeline | Optional `arch` and `corepack`; Corepack is a post-install action |
| `npm` | `npm` registry packument/tarball | npm SRI, always required; generates `npm`/`npx` launchers | Installed independently of Node, but needs an active Node at runtime |
| `pnpm` | `@pnpm/<os>-<arch>` platform package | npm SRI; standalone executable | Does not require Node; store variable depends on major version |
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
| `npm:<package>` | npm packument; isolated installs use embedded Aube, while project/global `use` can plan Aube, npm, or pnpm | A native lock or Aube graph carries transitive integrity; scripts denied by default | Discovers `.bin` dynamically, adds managed Node, and records only scope, installer, and optional native-lock identity in schema 3 |
| `github:owner/repo` | GitHub API with Atom/public release-page fallback on rate limiting; optional static catalog | Checksums, optional minisign, GitHub artifact attestations | Selects a host asset; supports archives and bare binaries; regex/template/bin/rename/strip rules handle complex releases |

These implementations live under [`backend/`](https://github.com/lejunyang/one-sdk/tree/main/crates/osdk-core/src/backend/). The npm-backed implementations share packument, version, and SRI handling in [`npm.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/npm.rs). Generic source ranking is in [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs).
See [npm developer tool implementation](./npm-tools) for the complete dynamic
backend project/global/isolated installation, cache, metadata-only lock, schema 2
sidecar compatibility, and shim boundaries.

## Declarative and GitHub backends

[`DeclarativeBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/declarative.rs) is a constrained schema-1 TOML extension point. It supports static or URL version lists, platform template variables, `tar.gz`/`tar.xz`/`tar.zst`/`zip`, fixed or remote checksums, `strip_root`, binary paths, and idiomatic version files. Definitions are limited to 1 MiB, remote lists to 10,000 versions, and URLs, filenames, relative paths, and checksums are strictly validated. It intentionally cannot execute hooks or arbitrary commands; every installation goes through the shared verification and CAS pipeline.

[`GithubBackend`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/github.rs) is a namespaced backend constructed at runtime. It reads up to 1,000 paginated releases, ignores drafts, applies prerelease policy, and scores assets for OS, architecture, and libc. Explicit rules handle non-standard asset names. Online, when signature verification is enabled, an available trusted minisign checksum manifest overrides a preloaded static digest; otherwise the static digest is used before ordinary sidecar/shared checksum discovery. The configured GitHub attestation policy is applied independently. GitHub API, page, Raw, release asset, and attestation URLs all use the same normalized source candidates, while credentials are sent only to the official API host.

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
