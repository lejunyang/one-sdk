# Cargo Developer Tool Implementation

This page describes the implementation behind the user-facing
[Cargo Developer Tools](../cargo-tools) workflow. Cargo tools are dynamic native
tools: osdk resolves their source and exact managed Rust dependency before
delegating one bounded install to `cargo-binstall` or `cargo install`, then owns
the resulting binary inventory and lifecycle.

## Dynamic identity and parsing

`ToolSpec::parse` routes the `cargo` namespace through the central
`CARGO_SCHEMA`; the dynamic registry factory then constructs one
`CargoPackageBackend` per canonical ID. The subject selects one of two source
types:

- a registry crate name is lowercased and limited to portable Cargo-name
  characters;
- an HTTPS Git URL preserves path case but must be canonical, credential-free,
  query-free, fragment-free, traversal-free, and have a non-root path without a
  trailing slash.

The parser distinguishes the selector after the repository URL from any `@`
that would otherwise appear in URL user information; user information is then
rejected by the Git URL validator. Registry selectors are limited to `latest`,
an exact semantic version, or a one/two-component numeric prefix. Git selectors
are limited to `latest`, `tag:<ref>`, `branch:<ref>`, and
`rev:<40 lowercase hex>`. Ref validation rejects ambiguous or unsafe Git ref
forms such as `..`, `@{`, `.lock`, control characters, and leading-dot path
components.

The option schema canonicalizes `features`, `default-features`, `bin`, `crate`,
and `locked`. Features are validated, sorted, and deduplicated; the default
values `default-features=true` and `locked=false` are omitted from canonical
identity. `crate` reuses the portable registry-crate validation and is permitted
only for Git sources. Every retained public option is material to the install
identity.

## Resolution and exact Rust binding

Registry resolution asks the normal ranked-source selector for Cargo metadata.
The defaults pair `https://crates.io/api/v1/crates` with
`sparse+https://index.crates.io/`, and rsproxy's API with its sparse index. The
chosen metadata endpoint and Cargo index therefore stay paired. Responses and
cached metadata are bounded to 8 MiB. Yanked releases are removed, versions are
sorted and deduplicated, and `latest`/prefix requests choose the highest stable
match. An exact request can select a non-yanked prerelease. The selected index
is carried as private resolution metadata and later passed to the provider. Git
selectors require no remote version-list request and are retained verbatim.

Before resolution, CLI orchestration detects any `cargo:` request and requires
exactly one managed `rust` request. An explicit Rust request wins; otherwise the
normal active/configured Rust selection is injected. The selection must parse as
an exact version, so floating `stable`, `nightly`, or `latest` values fail before
provider execution. Rust requests are partitioned and installed/resolved first;
the resulting exact version is injected into each Cargo request and resolved
version as private runtime metadata.

The backend then requires a complete, non-linked osdk Rust installation. It
locates `cargo` and `rustc` in that exact toolchain, follows rustup's
`manifest-rustc-*` payload list, and includes every regular file below the
selected target's sysroot library directory. Those bytes, the exact
version/platform, and the toolchain directory name form the native dependency
identity. A locked, atomic marker-adjacent receipt caches the resulting
`b3-rust-v2:` identity plus the complete sorted path/size/mtime inventory. The
hot path reuses the digest only while that inventory is unchanged; metadata drift
forces a full content rehash. A linked toolchain is rejected
both here and at lock writing because it has no stable managed artifact identity.
Changing any byte in those identity-bound build-critical files makes an existing Cargo
tool candidate fail runtime-identity validation.

## Install identity and material classification

`NativeToolLifecycle` builds an isolated `InstallIdentity` from:

- canonical Cargo backend ID and resolved selector/version;
- current platform and isolated scope;
- all canonical public Cargo options;
- the exact Rust version/platform plus its `b3-rust-v2:` build-critical identity;
- registry package and selected index, or Git URL and selector.

The resulting `b3-v2:` install ID determines both the physical root and the
cross-process lock. Different features, binary, workspace crate, lock policy,
build-critical Rust identity, registry index, repository spelling, or selector cannot alias the
same install. `cargo-resolution.json` schema 1 repeats the source kind, source,
version, backend, and replay classification inside the published root and is
validated during reuse.

This native path does not use the archive download/CAS pipeline. Cargo owns
source and dependency acquisition during the provider process; osdk owns the
staging root, durable binary identity, validation, and publication.

## Provider selection and the exit-94 boundary

`controlled_binstall` looks only for `cargo-binstall` under osdk's managed
`<data>/cargo/bin` (with the platform executable suffix). An ambient executable
on PATH is never eligible. The preferred path is used only when all of these are
true:

- osdk is online;
- the source is a registry crate other than `cargo-binstall`;
- no `features` option is present;
- `default-features` is not `false`;
- the controlled binary is a regular file.

`bin`, `locked`, exact version, install root, and the selected index are passed
to `cargo-binstall`; confirmation, telemetry, GitHub-token discovery, and its
compile/quick-install strategies are disabled. On success, osdk publishes its
output. Exit code 94 alone
means “no compatible binary artifact”: the stage is deleted and recreated under
the same held identity lock, then one `cargo install` attempt is made. Any other
exit code, spawn error, permission error, timeout, or capture failure is
terminal; no second provider is tried. Git sources and source-build options go
directly to `cargo install`.

`cargo install` receives an exact `=<version>` for registry crates. Git requests
translate the selector to no flag for HEAD, or to `--tag`, `--branch`, or
`--rev`. A Git-only `crate` value supplies Cargo's package argument. Both paths
pass the requested `features`, `--no-default-features`, `--bin`, and `--locked`
flags where applicable.

## Process isolation and bounds

Provider execution uses `CommandSpec::clear_env`, no shell, null stdin, concurrent
stdout/stderr draining, a one-hour wall-clock ceiling, and 1 MiB caps for each
captured output stream. The stage supplies the complete child environment:

```text
HOME, USERPROFILE        = <stage>/home
CARGO_HOME               = <stage>/cargo-home
CARGO_TARGET_DIR         = <stage>/target
CARGO_INSTALL_ROOT       = <stage>
RUSTUP_HOME              = osdk's managed rustup home
RUSTC                    = <exact-toolchain>/bin/rustc[.exe]
PATH                     = <exact-toolchain>/bin + sanitized system paths
TMPDIR, TEMP, TMP         = <stage>/tmp
CARGO_TERM_COLOR         = never
GIT_TERMINAL_PROMPT      = 0
```

The exact toolchain directory is always first; osdk removes its own shims and
Cargo-home proxies and deduplicates the rest while preserving system paths
needed by linkers and build helpers. Before publication, osdk removes the private home,
Cargo home, target, temp directory, and Cargo's tracking metadata; none becomes
part of the installed tool.

## Staging, publication, and reuse

The shared native-tool lifecycle acquires the exact identity lock, validates a
candidate if one already exists, and otherwise creates a unique sibling stage.
A complete but invalid or tampered root fails closed rather than being silently
overwritten. An incomplete root can be removed only while that identity lock is
held.

Publication rejects symlinks and provider-created reserved metadata. It requires
a regular `bin` directory, inventories each portable executable, and records its
size and SHA-256 in `.osdk-native-receipt.json`. It then writes
`.osdk-install.json`, the `.osdk-complete` marker, and an adjacent metadata seal
that binds the install ID to a BLAKE3 digest of published metadata. A final
no-replace directory rename exposes the installation atomically. Dropping an
unpublished stage removes it.

Reuse, activation, shims, listing, `where`, and uninstall all converge on the
same validation boundary: canonical root and identity, no symlinks, valid seal,
matching manifest and receipt, unchanged build-critical managed Rust bytes, and exact
binary paths, sizes, and SHA-256 values. Uninstall removes only that
identity-qualified root while holding its lock.

## Offline behavior

Resolution of a registry selector can read a previously cached metadata response,
and Git selectors need no metadata request. That does not make a cold install
offline-capable. `prepare` first permits a complete exact install to pass the
validation/reuse path. If staging is required while offline, the backend stops
before either provider runs because neither Cargo's native lock nor `osdk.lock`
contains the complete source graph.

This also means a warmed Cargo cache is not treated as a supported replay
contract. Offline success means exact complete-install reuse, not a provider
rebuild from opportunistic cache state.

## Lock schema 4 and replay truthfulness

Lock writing removes private `__osdk_*` fields from the public options table and
emits typed native metadata instead:

```toml
schema = 4

[platforms.linux-x64.tools.rust]
request = "1.91.1"
version = "1.91.1"

[platforms.linux-x64.tools."cargo:ripgrep"]
request = "14"
version = "14.1.1"
options = { locked = "true" }

[platforms.linux-x64.tools."cargo:ripgrep".native]
runtime = "rust"
runtime_version = "1.91.1"
replay = "version-only"
source = "sparse+https://index.crates.io/"
```

The native runtime entry must name `rust`, use an exact version, match the Rust
tool entry in the same platform table, and carry exactly one supported replay
classification. Registry entries also require the exact selected canonical
`sparse+https://.../` index in `source`; credentials, query strings, and
fragments are rejected, and Git entries reject that field:

- registry versions use `version-only`;
- a full lowercase 40-hex Git revision uses `immutable-revision`;
- Git HEAD, tags, and branches use `floating-ref`.

Loading injects this typed data into private request options, after which the
normal exact-identity path applies. Native entries cannot also carry generic
artifact or npm metadata. Lock schemas 1 through 3 cannot represent the runtime
binding, so any `cargo:` entry in those schemas is rejected and must be
regenerated as schema 4. The replay label describes selector strength; it is not
a dependency graph or cold-offline guarantee.

## Main verification points

The narrow regression suites cover:

- strict registry/Git ID, selector, option, and lock-key validation;
- non-yanked registry resolution and offline metadata-cache reads;
- exact Rust injection, Rust-first scheduling, and lock/runtime agreement;
- case-distinct Git identities and option-sensitive fingerprints;
- controlled `cargo-binstall` selection, success, exit-94 reset/fallback, and
  terminal handling of every other failure;
- cleared/private provider environments and provider argument construction;
- completed-install reuse, cold-offline rejection, binary/receipt/seal/runtime
  validation, atomic publication, and exact removal;
- schema-4 native round trips plus rejection in schemas 1 through 3.

The key implementation files are
[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs),
[`cargo_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/cargo_package.rs),
[`native_tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/native_tool.rs),
[`commands.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs),
and [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).
