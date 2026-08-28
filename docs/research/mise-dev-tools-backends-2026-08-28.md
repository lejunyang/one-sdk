# mise dev-tools backends and `osdk` parity research

Date: 2026-08-28

Upstream snapshot: [`jdx/mise@6a13eb5d3f760eed673f34e182acb418c21dca6a`](https://github.com/jdx/mise/commit/6a13eb5d3f760eed673f34e182acb418c21dca6a), authored 2026-08-27

`osdk` baseline snapshot: `one-sdk@bd00af2`, including declarative
locked-artifact replay. Current-state notes also include the dynamic
option-identity implementation delivered with this report.

## Status of the 2026-08-15 audit

[`sdk-manager-audit-2026-08-15.md`](./sdk-manager-audit-2026-08-15.md) is a useful **historical pre-remediation snapshot**, not a description of the current repository. In particular, its backend inventory, feature-gap table, test counts, missing-test claims, and Sigstore/Rekor limitation are stale after the work landed between 2026-08-16 and 2026-08-28.

The old report should remain available as an implementation-history record. Current planning and parity claims should use this report, the completed [`remaining-gaps-roadmap-2026-08-16.md`](./remaining-gaps-roadmap-2026-08-16.md), and the current source tree instead.

## Executive summary

The main lesson from current mise is not merely that it supports more named tools. Its leverage comes from composing four layers behind one tool grammar:

1. a canonical, dynamic identity such as `backend:package[options]@selector`;
2. a shared lifecycle covering resolution, installation, activation, execution, uninstall, and locking;
3. backend-specific adapters ranging from HTTP metadata to native package managers and Lua plugins; and
4. option-aware caches and locks that keep two materially different installations from being treated as the same artifact.

At the pinned revision, mise recognizes 19 fixed backend types: `core`, `npm`, `pipx`, `cargo`, `gem`, `go`, `dotnet`, `spm`, `aqua`, `github`, `gitlab`, `forgejo`, `http`, `s3`, `conda`, `pkgx`, `asdf`, `vfox`, and deprecated `ubi`. It also supports dynamically named vfox backend plugins. `Unknown` exists in source as a resolver sentinel and is not a usable backend. There is no native `composer:` backend; Composer must be obtained through another backend or plugin.

`osdk` already has strong lifecycle depth: install/use/uninstall, local and remote listing, current/where, outdated/upgrade, one-shot execution, activation, shims, aliases, trust, cross-platform locks, offline replay, source probing and failover, native package-manager caches, a BLAKE3 content-addressed store, and unusually rigorous artifact verification. Its 13 fixed backends cover major runtimes and package managers, while dynamic `npm:<package>` and `github:<owner>/<repo>` support are substantial. The delivered Phase 0 identity work gives those two namespaces canonical `b3-v2:` identities, `.osdk-install.json` schema-1 records, fingerprinted roots, coexisting same-version identities, and exact fail-closed lifecycle selection.

The largest strategic gap is **ecosystem breadth backed by a complete stable
dynamic identity**, not basic download mechanics. `osdk` cannot yet express most
of mise's package namespaces, and its declarative plugins are intentionally much
narrower than mise's Lua tool/backend plugins. Existing npm and GitHub dynamic
installs now validate canonical identity in their per-install records and use
fingerprinted physical roots, but one canonical parser plus universal cache, lock-fingerprint, secret, and physical
coexistence rules remain open. Backend options that change the selected artifact,
install layout, dependency graph, or executable set must ultimately participate
consistently across all of those boundaries.

The recommended implementation sequence is therefore:

1. finish canonical dynamic identity and option fingerprints (Phase 0 is
   partially implemented for `npm:` and `github:`);
2. an inline generic `http:` backend;
3. `cargo:` and Go-module `go:` developer-tool backends;
4. `pipx:` with `uv` preference;
5. native Aqua registry consumption and verification;
6. expansion of the existing GitHub backend;
7. a secure, versioned plugin boundary; and
8. command, lock, cache, and concurrency parity across every dynamic backend.

This order reuses `osdk`'s strongest primitives while avoiding a collection of one-off backends with incompatible identity and replay behavior.

## Scope and evidence model

This report covers mise's **dev-tools backend subsystem** and the adjacent semantics needed to use it correctly: tool specifications, configuration scope, install/use/exec behavior, lockfiles, caches, registries, URL rewriting, and authentication. It does not treat every mise environment-manager or task-runner feature as mandatory SDK-manager scope, but those adjacent capabilities are called out where they create a real behavioral difference.

There are two kinds of upstream reference below:

- The official documentation URLs describe supported user-facing behavior and are expected to evolve.
- The pinned GitHub commit is the reproducible implementation snapshot used to settle taxonomy and test-count questions. In particular, [`BackendType`](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs#L16-L38) and [`BackendType::guess`](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs#L58-L82) are authoritative for the fixed prefix list.

The current `osdk` assessment includes commit `bd00af2`. A lock-restored
declarative tool now uses the locked URL, filename, checksum, and archive shape
before consulting current plugin templates.

No live public installation was needed for this research. Test counts are structural counts of Rust `#[test]` and `#[tokio::test]` annotations, not claims that the suites were executed in this documentation-only task.

## 1. Current mise backend model

### 1.1 Taxonomy

The [backend overview](https://mise.jdx.dev/dev-tools/backends/) and [backend architecture](https://mise.jdx.dev/dev-tools/backend_architecture.html) group the system conceptually, while the pinned enum gives the exhaustive fixed set.

| Family | Backends | Role | Important boundary |
| --- | --- | --- | --- |
| Native core runtimes | `core:<tool>` | Rust implementations for high-use runtimes such as Node, Python, Go, Java, Ruby, Rust, Swift, Zig, Bun, Deno, Erlang, Elixir, and .NET | `core` is a backend type even though the backend index documents core tools separately. Behavior and lock fidelity vary by tool. |
| Language package ecosystems | `npm:`, `pipx:`, `cargo:`, `gem:`, `go:`, `dotnet:`, `spm:` | Turn packages/modules into isolated executable tools | These often delegate installation to a language-native command, so version locking does not always imply artifact URL/checksum locking. |
| Registry/universal binary systems | `aqua:`, `pkgx:`, deprecated `ubi:` | Use curated registry metadata or release conventions to install binaries | Aqua is the preferred curated path; `pkgx` is experimental; `ubi` remains runnable but is deprecated in favor of `github:`. |
| Release and direct-source backends | `github:`, `gitlab:`, `forgejo:`, `http:`, `s3:`, `conda:` | Resolve and install release assets, direct URLs/objects, or package archives | These provide the strongest opportunity for platform URLs, sizes, checksums, and provenance in `mise.lock`. |
| Plugin systems | `asdf:`, `vfox:`, `<plugin-name>:` | Run legacy shell plugins, modern Lua tool plugins, or multi-tool backend plugins | asdf is legacy and generally not Windows-capable. vfox is preferred for private/custom plugins. Dynamic backend prefixes are plugin-defined. |

Three distinctions matter when comparing this list with `osdk`:

- A bare mise tool such as `node` is commonly resolved through the shorthand registry to `core:node`; a prefix is not always visible to the user.
- A namespaced backend such as `go:github.com/...` means “install a Go command”, while bare `go` means the Go runtime. `osdk` currently implements only the latter meaning.
- Runtime capability and registry admission policy are different. Explicit asdf, vfox, and ubi references still work even though mise discourages or rejects new public registry entries for those paths.

The official backend index is not exhaustive enough to be used as a machine-readable taxonomy: it omits `core`, and the generated `mise backends ls` example has lagged newer enum variants. The pinned source enum should be the compatibility oracle.

### 1.2 Tool identity and selector grammar

The general explicit shape is:

```text
backend:package[option=value,...]@selector
```

Examples include:

```text
npm:prettier@3
github:BurntSushi/ripgrep[matching=musl]@14.1.1
http:my-tool[url=https://example.test/tool-{{version}}.tar.gz]@1.2.3
cargo:https://github.com/acme/demo@rev:0123456789abcdef
```

The backend/package portion is a durable identity, not just parsing decoration. Registry shorthand and `[tool_alias]` can map a friendly key to that identity, while a dynamic backend plugin supplies its own prefix. Built-in compatibility normalizations include `nodejs -> node`, `golang -> go`, and `dotnet-core -> dotnet`; these are not substitutes for a general alias registry.

Selectors fall into two categories:

| Selector | Semantics |
| --- | --- |
| Exact | A concrete published version such as `20.11.1`. An exact release wins over implicit prefix interpretation. |
| Fuzzy | A value such as `20` or `20.11` selects the newest matching line. `mise use` stores fuzzy intent by default; `--pin` stores the resolved value. |
| `latest` and aliases | Select the backend's newest stable value or a tool/backend alias such as `lts`. |
| `prefix:<value>` | Forces recursive prefix matching even if an exact release with the same spelling exists. |
| `ref:<ref>` | Selects a VCS ref where supported. |
| `path:<path>` | Uses an existing external installation rather than downloading one. |
| `sub-<partial>:<selector>` | Resolves the inner selector, subtracts numeric version components, then resolves the resulting prefix. |
| `tag:`, `branch:`, `rev:` | Backend-specific VCS selectors, notably Cargo and SPM. Full commit IDs are the reproducible form. |
| `system` | Selects an unmanaged executable on `PATH` where supported. |

Options are semantically significant. `github:owner/repo[matching=client]@1` and the same release with `matching=server` may produce different binaries. Cargo features, pipx extras, Aqua vars, HTTP extraction rules, platform overrides, and package-manager choice can likewise change bytes or layout without changing the version string. Correct implementations therefore need an option fingerprint in at least:

- installation identity or a validated per-install manifest;
- remote-version and derived-metadata cache keys when options influence resolution;
- lock entries; and
- reuse checks.

This is the architectural prerequisite for safely widening `osdk` beyond its current two dynamic namespaces.

### 1.3 Backend selection

Mise normally resolves a short tool name through its bundled registry and chooses the first enabled backend. Users can force an explicit prefix, define `[tool_alias]`, disable backend families, or use `MISE_BACKENDS_<TOOL>` overrides. The current resolver gives the environment override the highest priority, despite older architecture prose implying that an explicit prefix wins.

Registry snapshots are bundled with a release by default so an installed mise binary is tested with a stable mapping. `registry_floating=true` opts into current remote mise/Aqua registries. This snapshot-versus-floating choice is important for `osdk`: a centrally maintained shorthand registry should be versioned and reproducible by default, not fetched implicitly on every resolution.

## 2. Command, configuration, lock, cache, and source semantics

### 2.1 Command boundaries

| Command | mise contract | Consequence for parity |
| --- | --- | --- |
| [`mise install`](https://mise.jdx.dev/cli/install.html) | Installs explicit requests or all configured tools. It does not select/activate or rewrite config. Explicit one-off versions do not update project lock state; config-driven installs maintain an enabled or existing lock. Installs are parallel by default. | Keep installation, selection, and lock mutation separate. |
| [`mise use`](https://mise.jdx.dev/cli/use.html) | Batch-installs one or more tools and writes desired selectors. The target can be project-default, `--global`, `--env`, or `--path`; `--pin` and `--fuzzy` control persisted precision. | A one-tool-only interface is a meaningful gap, especially for atomic project setup. |
| [`mise exec`](https://mise.jdx.dev/cli/exec.html) | Runs a command using the entire resolved project environment plus optional temporary tool overrides. It does not mutate config or the parent shell. Current flags can sandbox environment, network, reads, and writes. | `exec` should work with zero explicit tools, preserve the child exit status, and treat sandboxing as an execution concern rather than a backend concern. |
| [`mise lock`](https://mise.jdx.dev/dev-tools/mise-lock.html) | Resolves and writes without installing. It supports selected tools, cross-platform targets, global/local scopes, legacy-format upgrade, dry-run, selector bumping, and JSON change output. | Lock generation must be independently automatable and must not have hidden installation side effects. |

Related distinctions are equally useful: `uninstall` removes an installed version without editing config; `unuse` edits config and may prune an unreferenced install; `where` returns an install root; `which` returns a concrete active executable; `prune` removes unreferenced versions; and `reshim` rebuilds launchers.

Mise does not have a single named “lazy install” mode. On-demand installation is exposed through `exec`, tasks, and command-not-found integration. Merely entering a directory does not necessarily install every missing tool.

### 2.2 Global and project configuration

Mise merges configuration hierarchically rather than choosing only one project file. The user-global config supplies defaults, configuration is accumulated while walking ancestors, and nearer files override farther ones. A system config can provide organization-wide defaults. Within one directory, local and environment-specific variants participate in documented precedence.

Important scope rules are:

- `mise use` normally writes the lowest-precedence ordinary config in the highest-precedence directory, usually `mise.toml`, even if `mise.local.toml` also exists.
- `--global`, `--path`, and `--env` explicitly select other write targets.
- `MISE_ENV=test` activates files such as `mise.test.toml`; local variants get their own lockfiles.
- `[tools]`, `[env]`, and `[settings]` merge additively with nearer conflicts winning. A nearer task definition replaces the same named parent task.
- Tool versions and options may interpolate values derived from config vars and environment/source directives.
- `.tool-versions` and optional idiomatic version files are compatibility inputs, not the whole configuration model.

The current official [configuration reference](https://mise.jdx.dev/configuration.html) and [environment-specific configuration reference](https://mise.jdx.dev/configuration/environments.html) are the source of truth.

### 2.3 Lockfiles

Mise's lockfile is opt-in for creation but sticky once present:

- `lockfile=true` allows config-driven `use`/`install`/`upgrade` to create and maintain locks.
- If the setting is unset, existing lockfiles are still maintained, but new ones are not implicitly created.
- Plain `mise lock` targets the active project root. Global config is excluded unless `mise lock --global` is used. Local and environment-specific configs map to corresponding lockfiles such as `mise.local.lock` and `mise.test.lock`.
- `mise lock --platform linux-x64,macos-arm64,windows-x64` can populate other platform entries without installing them.
- `mise lock --bump` advances fuzzy selectors without installation or config changes. `--dry-run --json` is designed for dependency-update automation.
- Version 1 records the exact version, canonical backend, original specifiers, artifact-affecting options, and platform-specific URL/checksum/size where supported.
- Strict mode requires recorded URLs only for backends capable of producing them. External-installer backends are skipped, so “locked” does not mean equal artifact reproducibility across all backends.

The command-state matrix is concise:

| Operation | Installs | Writes config | Writes lock |
| --- | ---: | ---: | ---: |
| `use` | Yes | Yes | Yes when lock applies |
| Config-driven `install` | Yes | No | Yes when lock applies |
| Explicit one-off `install tool@version` | Yes | No | No |
| `upgrade` | Yes | Sometimes with `--bump`/incompatible explicit target | Yes |
| `lock` / `lock --bump` | No | Normally no | Yes |

The central lock documentation's backend-support summary lags some newer implementations. For example, pinned source shows Conda and pkgx recording richer package/artifact information than the summary table suggests. Dedicated backend documentation plus pinned source should win when these disagree.

### 2.4 Caches and sources

Mise and `osdk` solve different source-selection problems. Mise generally starts from one logical upstream URL and then layers registries, shared metadata services, URL replacement, and backend-native mirrors. `osdk` models multiple candidate sources explicitly and benchmarks them. These approaches can coexist; they should not be conflated.

Current mise behavior includes:

- **mise-versions:** public GitHub version lists, release metadata, and attestations are normally served through [`mise-versions.jdx.dev`](https://mise-versions.jdx.dev), reducing unauthenticated API calls. GitHub tokens remain relevant for fallback, private repositories, and enterprise hosts. See [GitHub tokens](https://mise.jdx.dev/dev-tools/github-tokens.html).
- **Daily remote caches:** tool version lists, aliases, idiomatic filenames, discovered bin paths, and post-install execution environment are cached under `MISE_CACHE_DIR`; remote versions refresh daily by default. Cache keys incorporate backend/tool/options and, where behavior depends on resolved configuration, an option/environment context rather than using a single global list.
- **Aqua registries:** mise embeds a tested Aqua registry snapshot. Ordered custom registry URLs can precede it; optional floating mode checks current upstream data. Remote custom registry data has a separate TTL (one week by default), and compiled cache names include the registry content hash.
- **URL replacements:** [`url_replacements`](https://mise.jdx.dev/url-replacements.html) performs first-match ordered string or regex rewriting for outgoing URLs, including Conda metadata/artifacts and plugin HTTP. This is a routing layer, not mirror speed selection.
- **Credential caveat:** authentication headers produced for the original URL are preserved when a URL is replaced. A replacement to an untrusted host can therefore leak GitHub/GitLab/Forgejo or other credentials. Anchored patterns and trusted proxy endpoints are mandatory. Destination-host `.netrc` credentials are applied after rewriting and override default authorization headers.
- **Cache lifecycle:** [`mise cache clear`](https://mise.jdx.dev/cli/cache/clear.html) clears all or selected tool caches; `mise cache prune` removes old entries. The [cache behavior reference](https://mise.jdx.dev/cache-behavior.html) warns that many entries become stale after roughly a day and that caching the whole directory in CI is often wasteful.
- **HTTP exception:** normal `http:` installs use a content-addressed extraction store under `MISE_DATA_DIR/http-tarballs`, outside `MISE_CACHE_DIR`, because live install symlinks refer to it. `mise cache clear` intentionally does not break those installs.

## 3. Backend-by-backend matrix

The “lock” column distinguishes reproducible artifact metadata from merely recording an exact resolved version.

| Backend | Identity and installation | Dependency / source behavior | Lock and security posture | Closest current `osdk` coverage |
| --- | --- | --- | --- | --- |
| `core` | `core:<tool>` or registry shorthand; native Rust implementations for popular runtimes | Tool-specific official metadata and archives | Varies by tool: some record URL/checksum/provenance; delegated .NET/Rust/Swift paths are exempt from strict URL enforcement | Strong dedicated Node, Go, Python, Java, Rust, Deno, Bun, npm, pnpm, Yarn; fixed Maven/Gradle/Kotlin |
| `npm` | `npm:<package>`; direct HTTP metadata plus embedded Aube by default; optional Aube CLI, npm, pnpm, or Bun | Default metadata/install needs no Node; the installed tool or scripts may. Honors npm registry/auth config | Exact-version lock, not a portable artifact URL; Aube denies dependency builds by default | Strongest dynamic parity: Aube/native modes, three scopes, compact lock metadata, Node binding, validated bins |
| `pipx` | PyPI, GitHub/Git, or HTTP Python CLI package; prefers `uv tool install`, falls back to pipx | Requires uv or pipx; extras, registry override, install env, Git refs | Version-only lock; installed environment depends on Python resolver and index state | Missing dynamic backend |
| `cargo` | crates.io crate or Git URL; prefers cargo-binstall | Falls back to `cargo install` **only** for cargo-binstall exit code 94; features/default-features can force source build; Git supports tag/branch/rev | Version-only lock; source builds depend on Cargo ecosystem state | Missing dynamic backend; managed Rust/Cargo runtime exists |
| `gem` | `gem:<name>` through `gem install` | Requires Ruby/Gem; may need reinstall after Ruby change | Version-only, no enforceable artifact URL | Missing |
| `go` | `go:<module/path>` through `go install` | Requires Go; forces an isolated `GOBIN`; supports install env and build tags | Version-only; pseudo-versions supported | Missing package backend; do not confuse with the strong bare Go runtime backend |
| `dotnet` | `dotnet:<NuGet-tool>` through `dotnet tool install` | Requires .NET runtime/SDK; prerelease opt-in | No documented full artifact lock | Missing package backend; no current dedicated .NET runtime either |
| `spm` | GitHub/GitLab shorthand or Git URL | Prefers matching Swift artifact bundles, otherwise builds with SwiftPM; `rev:`/`ref:` force source | Lock fidelity varies between release bundle and source build | Missing |
| `aqua` | `aqua:<owner>/<repo>` resolved through bundled/custom registry metadata | No Aqua CLI; prebuilt assets; registry templates/vars; limited arbitrary environment setup | Strong checksums plus optional GitHub attestations, Cosign, SLSA, and Minisign; full lock metadata where resolved | Missing; existing pipeline and Sigstore/Rekor primitives are reusable |
| `github` | `github:<owner>/<repo>` release asset | Unified platform matcher; explicit patterns, narrowing filters, version prefix, multiple supplemental assets, rename/bin/bin-path, enterprise API | Full URL/checksum/size locks; GitHub attestations and provenance | Good base implementation, but option surface, provider/auth behavior, supplemental artifacts, and cache identity remain narrower |
| `gitlab` | `gitlab:<namespace>/<repo>` release asset | Shares release matcher; GitLab/self-hosted token and API configuration | Full URL/checksum/size; no GitHub-style attestation path | Missing |
| `forgejo` | `forgejo:<owner>/<repo>`, Codeberg by default | Shares release matcher; self-hosted API and credentials | Full asset identity where available; provenance unavailable | Missing |
| `http` | `http:<logical-name>[url=...]@version` | Inline URL/platform templates; optional remote version list parsed as text, regex, JSON path, or expression; archives and bare files | Full URL/checksum/size; optional checksum URL/expression; content-addressed extraction cache | Declarative TOML plugins cover a safe subset, but there is no inline dynamic `http:` identity or comparable metadata/extraction option surface |
| `s3` | `s3:<logical-name>[url=s3://...]@version` | AWS credential chain, custom endpoints/regions, manifest or object-list discovery | URL/checksum/size options; implementation can record artifacts | Missing |
| `conda` | `conda:<package>[channel=...]` | Direct Anaconda API/package extraction; no conda executable; isolated `CONDA_PREFIX`; limited to single-package CLI use | Current source records the selected package plus dependency identities/artifact metadata, beyond the central summary | Missing |
| `pkgx` | `pkgx:<pantry-project>` | Experimental native pantry resolver, bottle downloader, runtime wrappers, npm-style ranges | Main and transitive bottle URLs/checksums can be locked | Missing |
| `asdf` | `asdf:<plugin>` / `asdf:<owner>/<plugin>` | Executes legacy shell hooks; usually Unix-only; arbitrary code and external dependencies | Exact version can be recorded, but no strong artifact URL/provenance contract | Declarative plugins are safer but far less expressive; no asdf compatibility runtime |
| `vfox` tool plugin | `vfox:<owner>/<plugin>` | Embedded Lua hook system with HTTP/JSON/archive/semver modules and cross-platform execution | Tool plugins can report URL, rolling checksums, and attestations | No equivalent executable/sandboxed plugin API |
| Dynamic backend plugin | `<installed-plugin-name>:<tool>` | One Lua plugin manages many tools via list/install/exec-env hooks | Backend plugins currently cannot report all URL/provenance data available to tool plugins | No equivalent; registry constructs only `npm:` and `github:` dynamically |
| `ubi` | `ubi:<owner>/<repo>` or direct URL | Embedded release-asset heuristics | Deprecated; partial checksum/size lock; migrate to `github:` | Existing `github:` is the intended analogue, not a reason to add `ubi:` |

There is **no native `composer:` backend** in the pinned `BackendType`. A Composer executable can be delivered by an HTTP/GitHub/Aqua/plugin route, and PHP tools can use plugins or other ecosystem-specific paths, but callers must not assume `composer:<package>` exists.

### Backend-specific official references

- [npm](https://mise.jdx.dev/dev-tools/backends/npm.html), [pipx](https://mise.jdx.dev/dev-tools/backends/pipx.html), [Cargo](https://mise.jdx.dev/dev-tools/backends/cargo.html), [Gem](https://mise.jdx.dev/dev-tools/backends/gem.html), [Go](https://mise.jdx.dev/dev-tools/backends/go.html), [.NET](https://mise.jdx.dev/dev-tools/backends/dotnet.html), [SPM](https://mise.jdx.dev/dev-tools/backends/spm.html)
- [Aqua](https://mise.jdx.dev/dev-tools/backends/aqua.html), [GitHub](https://mise.jdx.dev/dev-tools/backends/github.html), [GitLab](https://mise.jdx.dev/dev-tools/backends/gitlab.html), [Forgejo](https://mise.jdx.dev/dev-tools/backends/forgejo.html), [HTTP](https://mise.jdx.dev/dev-tools/backends/http.html), [S3](https://mise.jdx.dev/dev-tools/backends/s3.html), [Conda](https://mise.jdx.dev/dev-tools/backends/conda.html), [pkgx](https://mise.jdx.dev/dev-tools/backends/pkgx.html)
- [asdf](https://mise.jdx.dev/dev-tools/backends/asdf.html), [vfox](https://mise.jdx.dev/dev-tools/backends/vfox.html), [backend plugin development](https://mise.jdx.dev/backend-plugin-development.html), [deprecated ubi](https://mise.jdx.dev/dev-tools/backends/ubi.html)

## 4. Current `osdk` parity and gaps

### 4.1 What is already strong

The current command surface in `crates/osdk-cli/src/cli.rs` and handlers in `commands.rs` cover the essential tool lifecycle:

- `install`, including no-argument config/lock installation;
- `use` with project and global persistence;
- `uninstall`, `list`, `list-remote`, `current`, `where`, and `reshim`;
- `outdated` and `upgrade`;
- `exec --tool ... -- command`;
- shell activation/deactivation and completions;
- alias, trust, source, registry, cache, prune, and doctor commands; and
- dedicated Node, Python, Rust, and model workflows.

The core backend trait in `crates/osdk-core/src/backend/mod.rs` is appropriately small: canonical ID and aliases, default sources/probe URL, version listing/resolution, install/uninstall, installed inventory, bin paths/names, runtime environment, and idiomatic files. That is a good base for dynamic package backends.

Current differentiated strengths are worth retaining:

- **Source selection:** multiple explicit official/mirror/custom candidates, concurrent throughput/TTFB probes, TTL caching, deterministic fallback, per-tool pinning, and secret-safe candidate fingerprints (`source/mod.rs`, `source/select.rs`).
- **Offline and artifact replay:** URL-keyed metadata cache, resumable/failing-over downloads, persisted checksums, and lock-restored artifact plans.
- **Storage:** per-file BLAKE3 CAS with hardlink/reflink/copy materialization, manifests, receipts, and garbage collection (`store/mod.rs`, `store/link.rs`).
- **Verification:** checksums/SRI, signed checksum manifests, optional GitHub Sigstore evidence, and Rekor SET/checkpoint/inclusion-proof verification.
- **Trust:** project configuration that can influence execution or sources is content-bound and must be trusted.
- **Dynamic npm security:** build scripts are denied by default in the Aube path, options are preserved, bins/manifests are validated, and global publication/rollback is journaled.

### 4.2 Current backend inventory

`Registry::new()` in `crates/osdk-core/src/backend/registry.rs` registers 13 fixed IDs. `Registry::load()` adds declarative TOML definitions. `Registry::get()` creates only `github:` and `npm:` backends dynamically.

| `osdk` backend | Current behavior | Main limitation relative to mise |
| --- | --- | --- |
| `node` / `nodejs` | Multi-source Node downloads, LTS metadata, architecture option, checksum verification, optional Corepack, project metadata and idiomatic files | Cross-architecture resolution is lockable but installation rejects non-host architecture; narrower core-tool metadata conventions |
| `go` / `golang` | Go SDK releases, mirrors, SHA-256, `GOROOT`, idiomatic files | No distinct `go:<module>` dynamic command backend |
| `python` / `py` / `cpython` | Embedded and optionally refreshed PBS catalog; CPython, PyPy, GraalPy, Pyodide, variants, prereleases, checksums | No `pipx:`/PyPI tool isolation or general virtualenv workflow |
| `java` / `jdk` / `openjdk` | Foojay-compatible catalog, distribution and JDK/JRE options, checksum, `JAVA_HOME` | Only one configured catalog endpoint/source by default; `.sdkmanrc` is not fully parsed |
| `maven`, `gradle`, `kotlin` | Verified archive installs | One hardcoded version per tool; no real remote index |
| `rust` / `rustup` | Isolated rustup/Cargo homes, components, targets, check/repair, overrides, linked toolchains | Delegate path is outside archive CAS; stable/beta/nightly remain floating; generic `system` semantics are not implemented |
| `npm` | Independently managed npm CLI, SRI, managed Node launchers and cache | Runtime still needs managed Node |
| `pnpm` | Standalone platform package, SRI, version-aware store variables | Explicit supported platform matrix |
| `yarn` | Classic and Berry metadata/install, SRI, generated Node launchers | Needs managed Node; no broader Corepack ecosystem |
| `deno` | npm packument/platform packages, SRI, `DENO_DIR` | No musl Linux package path |
| `bun` | npm packument/platform packages, SRI, glibc/musl selection | Dedicated runtime only |
| `npm:<package>` | Embedded Aube and native npm/pnpm modes; isolated/project/global scopes; dependency graph metadata; `.osdk-install.json` schema 1 with `b3-v2:` identity and bin validation | The only fully developed language-package namespace; lock schema 3 stores public options and compact native-lock identity rather than the full dependency graph; osdk-owned same-version identities use distinct fingerprinted roots, while project-managed npm remains separate |
| `github:<owner>/<repo>` | API, Atom, and public-page fallback; platform scoring; static catalogs; archive/binary installs; checksum/minisign/attestation; `.osdk-install.json` binds asset/layout/material identity | Fewer portable asset controls and credential/provider paths than mise; no GitLab/Forgejo siblings; same-version identity variants coexist in fingerprinted roots |
| Declarative TOML | Safe static/line-list archive definitions with checksums, templates, bins, and idiomatic files | No inline namespace, bare binaries, JSON/regex/expression version parsing, env, dependencies, transformations, hooks, or plugin lifecycle |

Commit `bd00af2` adds locked-artifact replay before current templates are
rendered and includes an offline regression test, closing that specific
declarative-backend reproducibility gap.

### 4.3 Command differences

| Area | Current `osdk` | Material mise gap |
| --- | --- | --- |
| `install` | Explicit or resolved config; no-argument/no-option path consumes a matching platform lock | Explicit options bypass the lock; no automatic lockfile mode |
| `use` | One tool per invocation; project or `--global`; npm has rich project/global transactions | No batch use, `--env`, or arbitrary `--path` target |
| `exec` | Requires one or more `--tool`, installs them, composes PATH/env, runs a command | Cannot run the config-only project environment; no read/write/network/env sandbox flags; child nonzero status becomes a generic error instead of being preserved exactly |
| `lock` | Exact platform-aware `osdk.lock`; artifact and npm metadata | No `--platform`, `--bump`, `--json`, `--dry-run`, `--global`, or config-environment lock selection on the general command. Locking dynamic npm tools may install managed Node and prepare a native dependency graph, so it is not a universally resolve-only operation like `mise lock`. |
| Config | `config path/list`; mutation through specialized commands | No general settings/config set/unset and no effective file-order inspection |
| Cache | Shows env and clears downloads; CAS prune exists separately | No category/key/age clear, stats, or complete cache lifecycle UI |
| Product adjacency | Dedicated SDK/model workflows | No task runner, watch mode, general `[env]`, dotenv/templates, hooks, secrets, or task-scoped tools |

### 4.4 Global and project resolution differences

`osdk` currently applies CLI flags, environment variables, the nearest `osdk.toml`/`.osdk.toml`, user config, and defaults. It can fall back through `.tool-versions`, backend idiomatic files, Node project metadata, and global tool pins for active resolution.

Compared with mise, it lacks:

- merged ancestor project configurations;
- a system config layer;
- `local` and named environment profiles;
- include files and general template evaluation;
- multiple selected versions for one tool;
- general `[env]`, tasks, hooks, and path/source directives; and
- a functional general `path:`/`system` selection model.

The nearest-project-only behavior is visible in `find_project_config`: the upward search returns on the first match. Some nested settings are also replaced rather than deeply merged. This is adequate for the current product but should be made explicit before adopting mise-compatible config syntax.

### 4.5 Lock differences

`osdk.lock` lock schema 3 is strong in several respects: it partitions tools by platform (including musl), stores exact request/version/options, can attach artifact URL/name/checksum/subdirectory/evidence, stores compact npm installer/scope/native-lock identity, and preserves model manifests. Writes validate size/schema/path safety, publish via a temporary file, sync, and atomically replace.

Lock schema 3 is independent of `.osdk-install.json` schema 1. The lock persists
public options and backend replay metadata; the install record persists a nested
`identity` containing `tool`, `version`, `platform`, `scope`, `material_options`,
`dependencies`, `materials`, and canonical `b3-v2:` `install_id`. That identity
selects the physical root and gates local reuse and every lifecycle operation.
`.osdk-tool.json` schema 1 or 2 is legacy detection only and never authorizes
reuse or execution. References below to a schema-2 npm graph sidecar mean the
older lock compatibility format, not a dynamic install identity format.

Remaining gaps are semantic rather than serialization-only:

- general lock creation/maintenance is not a configurable policy;
- explicit install arguments or `-o` bypass the no-argument lock path;
- locking dynamic npm tools may install managed Node and materialize a dependency graph rather than remaining side-effect-free;
- one lock command cannot populate an arbitrary platform matrix;
- there is no selector-only bump or JSON diff workflow;
- models are recorded but no-argument tool install does not restore them;
- Rust channels are not immutable release identities;
- complete existing installs are reused without rehashing all installed content;
- project lock read-modify-write is atomically published but not serialized across competing processes; and
- compact npm metadata depends on the separately persisted native lock/cache for cold offline reconstruction.

### 4.6 Source and cache differences

`osdk`'s source model is stronger than mise's URL-rewrite model for availability and latency: it understands candidate identity, probes multiple sources, caches rankings against a secret-safe source-list fingerprint, and keeps failed candidates as later download fallbacks. Pinning is preferential, not strict, because fallback remains enabled.

Important current limits are:

- the source candidate fingerprint covers source configuration; npm/GitHub
  install reuse now has a manifest-gated option-identity contract, but tool
  options still do not form a universal remote-metadata/cache identity across
  backends;
- `--refresh-sources` is not uniformly consumed by lock, outdated, or list-remote;
- registry probing is a separate npm-only subsystem rather than a generalized package-registry abstraction;
- metadata, source-probe, artifact, native-manager, model, and CAS stores have different lifecycle controls; and
- `cache clean` clears only downloads, while the CLI does not expose category-specific inspection, size reporting, or age-based pruning.

Adopting mise-style `url_replacements` should be considered separately from source ranking. If added, `osdk` should default to stripping origin credentials on cross-origin rewrites and require an explicit trusted-forwarding policy. Blindly copying mise's header-preservation behavior would weaken `osdk`'s existing `forward_credentials` boundary.

## 5. Test audit

### 5.1 Current `osdk` coverage

At committed `HEAD`, the repository contains 539 Rust test annotations across the workspace:

| Area | Annotation count | What it demonstrates |
| --- | ---: | --- |
| `osdk-core` | 340 | Version/config parsing, source selection, HTTP failures, pipelines, extraction, CAS, verification, backends, models, trust, inventory |
| `osdk-cli` unit tests | 103 | Lock schema/validation, config edits, command planning, global npm transaction internals |
| `osdk-cli/tests/isolated_cli.rs` | 67 | Real subprocess behavior under isolated homes/state, including locks, trust, exec, registry routing, npm project/global flows, and rollback |
| `osdk-shim` unit tests | 3 | Shim-local helpers |
| `osdk-shim/tests/shim_contract.rs` | 26 | Arguments/stdin/stdout/stderr/status, recursion, dynamic inventory, registry routing, aliases, and cross-platform wrapper behavior |

These counts directly supersede the old audit's “62 inline tests” and “no CLI integration/backend contract/shim runtime tests” snapshot. Current high-value coverage includes:

- a real CLI subprocess suite with temporary `HOME` and `OSDK_*` state;
- offline lock resolution, cross-architecture lock selection, lock consumption, artifact tamper rejection, and evidence re-verification;
- dynamic npm project/global installs, native lock formats, rollback, crash recovery, bin ownership, registry selection, and offline/authenticated fail-before-mutation cases;
- HTTP 403/429/5xx/timeout/malformed/stale/offline behavior and source candidate-cache invalidation;
- interrupted download resume and failed-install cleanup;
- real registered backends installing/uninstalling locked fixture archives;
- isolated Rust lifecycle coverage;
- Unix shim subprocess contracts; and
- native Windows CI plus the full Windows GNU workspace suite under Wine.

The shared contract deserves one qualification. `all_builtin_backend_ids_satisfy_the_lifecycle_contract` loops over IDs using a synthetic backend. A second test invokes real registered backends for locked fixture install, bin discovery, marker/receipt creation, and uninstall. It does not run every real backend through normal remote list/resolve/source behavior. Dynamic npm and declarative plugins are not yet part of one end-to-end common matrix.

### 5.2 Pinned mise backend test annotations

The following are direct counts of `#[test]` and `#[tokio::test]` annotations in individual `src/backend/<name>.rs` files at `6a13eb5d`. They are a navigation signal, not a quality score: helpers, integration suites, snapshots, and shared matcher tests may live elsewhere, and a single annotation may exercise many cases.

| Backend file | Test annotations | Backend file | Test annotations |
| --- | ---: | --- | ---: |
| `npm.rs` | 70 | `aqua.rs` | 91 |
| `github.rs` | 34 | `pipx.rs` | 27 |
| `spm.rs` | 26 | `http.rs` | 21 |
| `cargo.rs` | 19 | `go.rs` | 18 |
| `asdf.rs` | 6 | `vfox.rs` | 5 |
| `gem.rs` | 3 | `dotnet.rs` | 3 |
| `ubi.rs` | 1 |  |  |

Additional pinned files contain 11 Conda, 8 S3, and 5 pkgx annotations. The much larger npm/Aqua suites are evidence that backend parity requires detailed policy and failure tests, not merely implementing a trait.

### 5.3 Remaining `osdk` test gaps

1. **Real-backend contract breadth.** Every registered backend should run its real list/resolve/locked-install/bin/execute/uninstall path against local fixture metadata. Dynamic `github:`, dynamic `npm:`, and declarative plugins need explicit rows.
2. **Dynamic identity breadth.** Focused npm/GitHub coverage proves canonical
   `b3-v2:` identities, fingerprinted-root coexistence, and exact lifecycle
   selection. A generic matrix is still needed to prove equivalent cache/lock
   behavior for every future namespace.
3. **Source command integration.** `source add/list/test/pin/unpin/remove` lacks a complete subprocess round trip, restart persistence, alias canonicalization, invalid-write rollback, and two-process mutation test.
4. **Cache guarantees.** `cache clean` tests deletion of downloads but not byte-for-byte preservation of CAS, metadata, probe caches, native-manager caches, model snapshots, and installations.
5. **Cross-process writers.** Lock and global npm code has atomic writes and strong in-process transaction tests, but project `lock/use/upgrade` races need independently spawned competing processes and lost-update detection.
6. **Remaining shim behavior.** Unix signal forwarding has no focused regression case.
7. **Declarative replay.** The focused lock-replay test landed in `bd00af2`;
   the declarative backend should still enter the shared lifecycle contract.

## 6. Recommended phased roadmap

Each phase below is independently shippable and should be one focused implementation series. Later backends depend on the identity and replay invariants established in phase 0.

### Phase 0 — Canonical dynamic identity and option fingerprints (partially implemented)

Introduce one parsed representation for `backend:package[options]@selector` and use it across CLI parsing, config aliases, registry lookup, install paths, inventories, cache keys, locks, source configuration, and shim resolution. Classify options as resolution-affecting, artifact-affecting, layout-affecting, execution-only, or secret; persist only the safe identity projection.

Do not make the raw version the sole reuse key. A backend must either use an option fingerprint in its physical install identity or validate a complete option fingerprint in a manifest before reuse. Canonicalization rules must handle case-sensitive ecosystems intentionally rather than applying npm's lowercase rule globally.

The current implementation completes the physical install-identity branch for
the existing osdk-owned `npm:` and `github:` namespaces:

- it allowlists and normalizes safe public identity options, rejects unknown
  public options before installation, and excludes internal `__osdk_*` replay
  metadata;
- it computes an order-independent, domain-separated BLAKE3 `b3-v2:` identity
  from `tool`, exact `version`, `platform`, `scope`, normalized
  `material_options`, `dependencies`, and `materials`;
- `.osdk-install.json` schema 1 stores those values in nested `identity` plus
  `install_id`, and the fingerprint selects the physical root;
- same-backend/version identity variants coexist, while reuse, activation, shim
  execution, `where`, uninstall, and `reshim` select only the exact configured
  identity; and
- old `.osdk-tool.json` schema 1 and 2 records are detection-only legacy state
  and never authorize reuse or execution. Project-managed npm remains separate.

This is deliberately not the whole phase. A single parsed identity
across CLI/config/aliases, option-aware remote metadata and cache keys, an
explicit secret/keyed-hash policy, fingerprinted lock identity beyond persisted
public options, and generalization beyond npm/GitHub remain open.

Full-phase acceptance criteria (not yet all met):

- One parser round-trips unscoped and scoped npm names, URLs containing `@`, inline options, and all supported selectors without ambiguity.
- `backend/package/version/options` has one canonical serialized form shared by config, inventory, lock, and cache code.
- Two artifact-affecting option sets at the same version never silently reuse or overwrite one another.
- Reordered equivalent options produce the same fingerprint; a changed secret changes a keyed hash without writing the secret to disk.
- Aliases preserve the user's config key but resolve to one canonical runtime backend identity.
- Unsafe path components, unknown dynamic prefixes, duplicate aliases, and conflicting identities fail before mutation.
- Existing `npm:`/`github:` config, inventory, and schema-3 locks migrate or fail with an actionable compatibility error.

### Phase 1 — Inline `http:` backend

Promote the safe archive pipeline behind an inline `http:<name>` backend. Reuse declarative definitions internally, but add first-class dynamic identity, bare-file installation, platform URL maps, checksums/sizes, checksum URLs, archive-format override, strip/bin/rename/bin-path controls, and remote version extraction from plain text, regex, and constrained JSON paths. Expression evaluation can follow later if a safe embedded language is not already available.

Acceptance criteria:

- Exact-version static URLs work completely offline after cache warmup and lock generation.
- URL templates are defined for version/OS/architecture and reject unknown or unsafe expansions.
- Both bare executables and supported archives install on Linux, macOS, Windows, and Wine test fixtures.
- Platform-specific URL/checksum/size/format options are included in the identity fingerprint and lock.
- Cross-platform lock generation fetches metadata/checksums but never downloads target artifacts.
- Redirects, timeouts, 404/429/5xx, truncation, resume, checksum mismatch, archive traversal, symlink escape, and ambiguous bin selection fail safely.
- Locked offline replay is independent of later config/template changes.

### Phase 2 — `cargo:` and Go-module `go:` backends

Add the two compiled package ecosystems together because both depend on a selected compiler/runtime, isolated output roots, source-build logs, and option-sensitive reuse. Keep their namespace separate from bare `rust` and bare `go` runtimes.

Cargo acceptance criteria:

- Support registry crates plus Git URLs with `tag:`, `branch:`, and `rev:` selectors.
- Prefer cargo-binstall when eligible and fall back to `cargo install` **only** on exit code 94. Other failures propagate unchanged.
- `features`, `default-features`, `bin`, `crate`, and `locked` affect identity and reuse.
- Full commit revisions are persisted for reproducible Git installs; floating branches are clearly marked non-reproducible.
- Installation uses an isolated target/root and does not mutate the user's `CARGO_HOME`.

Go acceptance criteria:

- Support module/cmd paths, exact semantic and pseudo-versions, build tags, and install environment.
- Force `GOBIN` to the staged install bin directory and validate that expected executables remain inside it.
- Use the selected managed Go runtime when configured without leaking to a user-global module/build cache unless explicitly selected.
- Exact module version, tags, relevant environment policy, and Go runtime identity participate in reuse/lock validation.

For both backends, local fake registry/proxy fixtures must cover dependency failure, compile failure, cancellation, concurrent installs, cleanup, and uninstall.

### Phase 3 — `pipx:` with `uv` preference

Implement isolated Python CLI tools, preferring managed/system `uv tool install` and falling back to pipx only under an explicit capability rule. Support PyPI names, Git URLs/refs, GitHub shorthand, direct archives, extras, package-name override, registry URLs, and installer arguments.

Acceptance criteria:

- Resolver and installer select uv when available and use pipx only when uv is unavailable or explicitly disabled.
- The Python runtime and installer used are recorded in the install manifest and validated before reuse.
- Extras, index URL, VCS commit, package name, and installer mode are fingerprinted.
- Each tool has an isolated environment and exposes only validated entry points.
- Updating the selected Python invalidates or intentionally rebuilds dependent tools.
- Private-index credentials never enter lockfiles, fingerprints, logs, or child arguments visible to unrelated processes.
- Offline behavior fails before mutation unless a complete locked wheel/sdist graph is available; do not claim cold-offline reproducibility from a version-only lock.

### Phase 4 — Native Aqua backend

Consume a versioned bundled Aqua registry snapshot without requiring the Aqua CLI. Add ordered custom registries and an explicit floating-registry mode, with content-hashed compiled caches. Reuse `osdk` verification primitives rather than invoking external verifiers.

Acceptance criteria:

- The bundled registry is pinned, provenance-recorded, and usable offline.
- Custom local/HTTPS registries are evaluated in order, with the bundled registry as an explicit fallback policy.
- Registry source/content/options participate in compiled cache keys; changing any of them invalidates cached package metadata.
- Platform templates, package aliases, files/bin filtering, required/default vars, and prerelease policy match fixture expectations.
- Checksums always verify; supported Minisign, Cosign, SLSA, and GitHub attestation declarations fail closed according to policy.
- Cross-platform lock entries record URLs/checksums/sizes; current-platform provenance is verified before recording.
- No registry or artifact credential is forwarded to an untrusted replacement/mirror.

### Phase 5 — Expand `github:` before adding sibling providers

Build on the current GitHub implementation instead of introducing deprecated `ubi:`. Add a shared release-asset matcher and the remaining portable rules: `matching`, `matching_regex`, `asset_pattern`, supplemental assets, `version_prefix`, platform-specific settings, `size`, `bin`, multi-rename, filtered bins, `bin_path`, `no_app`, prerelease, enterprise API URL, and per-tool attestation control.

Acceptance criteria:

- Autodetection has fixture coverage for OS/arch/libc synonyms, debug/source/checksum assets, bare executables, archives, macOS apps, and platform-neutral artifacts such as `.phar`.
- Explicit pattern or narrowing filters have documented precedence and must select exactly one primary asset.
- Supplemental artifacts are ordered, individually locked/verified, and cannot overwrite outside the staging root.
- Two aliases selecting independent binaries from one repository have distinct install identities.
- Token lookup is host-scoped; third-party mirrors never receive origin credentials unless explicitly trusted.
- Lock replay avoids GitHub APIs and re-verifies cached evidence; anonymous rate-limit fallback remains tested.
- The shared matcher is provider-neutral enough for later `gitlab:` and `forgejo:` adapters.

### Phase 6 — Secure plugin boundary

Define a versioned capability API before copying asdf compatibility. Prefer a cross-platform sandboxed/WASM or tightly capability-scoped embedded runtime; if Lua/vfox compatibility is chosen, expose structured HTTP, JSON, archive, semver, logging, and process APIs rather than unrestricted ambient execution. Separate one-tool plugins from multi-tool backend plugins.

Acceptance criteria:

- Plugins declare API version, backend prefix, capabilities, dependencies, config schema, and integrity metadata.
- A plugin can list versions, propose an install plan, declare bins/env, and uninstall without bypassing path, network, credential, checksum, or lock policies.
- Network hosts, executable spawning, filesystem roots, environment access, and credential forwarding are denied unless declared and trusted.
- Plugin install/update/remove/link/list operations are transactional and preserve versioned provenance.
- Dynamic plugin prefixes cannot shadow built-ins or one another.
- Plugin code/config changes invalidate the relevant option/cache fingerprint.
- Backend plugins can report enough immutable artifact identity for strict locks; otherwise the lock explicitly records a version-only/non-reproducible status.
- Windows, Unix, malformed-result, timeout, crash, cancellation, and malicious-path fixtures are mandatory.

Do not make Bash/asdf compatibility the primary extension boundary. If added for migration, label it legacy, require explicit trust, and isolate it from the modern contract.

### Phase 7 — Lifecycle, lock, and cache parity

Once several dynamic namespaces share the abstraction, close cross-cutting semantics rather than adding more one-off backends.

Acceptance criteria:

- `use` accepts batches and atomically commits config/lock state; supports project, global, environment, and explicit path targets.
- `exec -- command` works from resolved project config with no mandatory `--tool`, optional overrides, exact child exit-code propagation, and documented sandbox controls.
- Lock policy supports automatic maintenance, existing-lock stickiness, `--platform`, `--global`, local/environment scopes, `--bump`, `--dry-run`, and JSON changes.
- All lock/cache/install reuse paths match canonical backend identity plus artifact-affecting option fingerprint.
- Project config and lock writers use cross-process serialization and recovery journals where several files must commit together.
- `system` and `path:` are real external-tool selections with explicit non-reproducibility semantics.
- Cache commands report sizes and clear/prune by category, backend, key, and age without breaking CAS-backed or HTTP-backed live installations.
- The real-backend contract includes every fixed and dynamic backend. Adding a backend must fail CI until fixture lifecycle, offline, option identity, lock, and Windows cases are registered.
- Linux development runs the full Windows GNU workspace suite through `./scripts/windows-wine-tests.sh`; native Windows remains in CI. All tests use temporary `HOME`, `OSDK_*`, `CARGO_HOME`, `RUSTUP_HOME`, and build directories.
- Every user-facing change updates both READMEs and paired Chinese/English VitePress pages/navigation in the same feature commit.

### Deferred breadth after the platform is sound

After phase 7, add backends according to demonstrated demand: `gitlab:`, `forgejo:`, `s3:`, `conda:`, `pkgx:`, `gem:`, `dotnet:`, and `spm:`. Do not add a deprecated `ubi:` compatibility backend unless migration demand justifies it. Do not invent a native `composer:` backend for mise parity, because mise itself has none; route Composer through HTTP/GitHub/Aqua/plugin metadata or design it as a separately justified ecosystem backend.

## 7. Release-level definition of done

A phase is complete only when all of the following are true:

1. **Identity:** The canonical request, backend, resolved version, platform, and artifact-affecting options are unambiguous and stable across restart.
2. **Isolation:** Tests and installers cannot read or modify real user manager state.
3. **Reproducibility:** The lock states exactly what can and cannot be reproduced offline. A version-only delegate lock is never described as an artifact lock.
4. **Security:** Credentials, option secrets, and replacement URLs have explicit forwarding boundaries; unsafe paths and ambiguous assets fail before mutation.
5. **Transactions:** Failed, cancelled, and concurrent operations leave either the old valid state or the complete new state, never a mixed state.
6. **Lifecycle:** List, resolve, install, discover/execute binaries, uninstall, reinstall, and offline paths are covered using the real backend.
7. **Platforms:** Linux, macOS, native Windows, and Windows-under-Wine coverage matches the backend's claimed support. Unsupported targets fail at resolution, not after partial mutation.
8. **Documentation:** User workflow stays aligned in English and Chinese; implementation details live in paired VitePress implementation pages.
9. **Observability:** Errors identify the backend, canonical tool identity, selector, source/provider, phase, and safe remediation without logging secrets.
10. **Compatibility:** Existing `osdk.toml`, `.tool-versions`, inventories, installs, and locks either continue to work or produce a tested, actionable migration error.

## 8. Historical audit reconciliation

The following current-state claims from the 2026-08-15 audit are specifically superseded:

| Historical claim | Current state on 2026-08-28 |
| --- | --- |
| Nine built-in backends; npm is bundled only | 13 fixed backends, independent npm, dynamic `npm:<package>` and `github:`, plus declarative TOML plugins |
| Bun/Deno/Python rely on the GitHub API as described there | Bun/Deno use npm packuments/platform packages; Python uses embedded/verified PBS catalogs; GitHub API behavior has fallback and caching |
| Source config is not honored consistently | Source handling was repaired; npm registry selection is a separate, tested subsystem |
| Jobs, prompts, resume, Rust isolation/uninstall, and checksum policy are absent | These are implemented and tested |
| No lockfile, offline mode, outdated/upgrade, exec, aliases/completions, trust, or concurrent install | All are implemented |
| Node/Python/Java/Rust/Deno/Bun/GitHub gaps listed there are open | Most were completed in the August 16 roadmap; current gaps are narrower and documented above |
| Rekor inclusion proof and SET are not verified | Current verification checks SET, checkpoint, and Merkle inclusion proof |
| 62 inline tests; no CLI/shim/backend contract or Windows runtime tests | 539 Rust test annotations, including 67 CLI subprocess and 26 shim integration annotations, backend contracts, network failure matrices, native Windows, and Wine execution |

The old document's dated upstream revision table, rationale, and remediation history remain valuable. Its present-tense inventory and gap language must not be copied into new planning documents without revalidation.

## 9. Official source index

Core references:

- [Backends](https://mise.jdx.dev/dev-tools/backends/)
- [Backend architecture](https://mise.jdx.dev/dev-tools/backend_architecture.html)
- [Dev tools and tool options](https://mise.jdx.dev/dev-tools/)
- [Configuration](https://mise.jdx.dev/configuration.html)
- [`mise install`](https://mise.jdx.dev/cli/install.html)
- [`mise use`](https://mise.jdx.dev/cli/use.html)
- [`mise exec`](https://mise.jdx.dev/cli/exec.html)
- [`mise lock`](https://mise.jdx.dev/dev-tools/mise-lock.html)
- [GitHub tokens and mise-versions behavior](https://mise.jdx.dev/dev-tools/github-tokens.html)
- [URL replacements and credential warning](https://mise.jdx.dev/url-replacements.html)
- [Cache behavior](https://mise.jdx.dev/cache-behavior.html)
- [Pinned backend-type source](https://github.com/jdx/mise/blob/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend/backend_type.rs)
- [Pinned backend source directory](https://github.com/jdx/mise/tree/6a13eb5d3f760eed673f34e182acb418c21dca6a/src/backend)

Local `osdk` evidence entry points:

- `crates/osdk-core/src/backend/registry.rs` — fixed and dynamic registry behavior
- `crates/osdk-core/src/backend/mod.rs` — shared backend contract
- `crates/osdk-core/src/version/mod.rs` and `version/resolver.rs` — selector and project resolution
- `crates/osdk-core/src/source/mod.rs` and `source/select.rs` — source candidates and ranking
- `crates/osdk-core/src/store/` and `pipeline/` — CAS, downloads, extraction, receipts, and verification
- `crates/osdk-core/src/inventory.rs` — dynamic installed-tool identity
- `crates/osdk-cli/src/cli.rs`, `commands.rs`, and `lockfile.rs` — command and lock semantics
- `crates/osdk-cli/tests/isolated_cli.rs` and `crates/osdk-shim/tests/shim_contract.rs` — subprocess contracts
- `scripts/windows-wine-tests.sh` and `scripts/windows-runtime-smoke.ps1` — Windows runtime validation
