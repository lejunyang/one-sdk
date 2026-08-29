# Version resolution internals

This page describes how `osdk` turns project declarations or CLI input into an installable exact version. Version resolution chooses a tool and version only; source ranking and installation transactions happen in later stages.

## From request to exact version

The entry point is [`gather_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs). Explicit arguments such as `node@20` are parsed by [`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs). With no explicit tools, the CLI combines `[tools]` configuration, project package-manager declarations, and Node project metadata. Selecting npm, pnpm, Yarn, or a dynamic `npm:<package>` tool makes [`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) include Node in the same operation; it uses `latest` when the project has no Node declaration.

`npm:<package>` is parsed before the generic `tool@version` split so scoped
`npm:@scope/name@version` requests remain intact. Its dynamic backend ID is
canonicalized to `npm:<package>`, while bare `npm` remains the package-manager
backend. See [npm developer tool implementation](./npm-tools#identity-resolution-and-lifecycle-orchestration).

`cargo:` uses the same URL-aware syntax parser with a stricter namespace schema.
Registry subjects accept exact/latest/numeric-prefix selectors; canonical HTTPS
Git subjects accept only latest, tag, branch, or a full lowercase revision. A
Cargo request also injects or preserves exactly one configured/explicit exact
Rust request. Rust is resolved first and its exact version is bound to every
Cargo request before Cargo resolution continues. See
[Cargo developer tool implementation](./cargo-tools#resolution-and-exact-rust-binding).

`go:` has a dedicated namespace schema for canonical module/command paths,
semantic or pseudo-versions, tags, and a restricted build environment. It
injects or preserves one managed Go request, resolves that runtime first, and
then discovers the longest module root through ranked Go proxy metadata. See
[Go developer tool implementation](./go-tools#canonical-identity-and-selection).

[`version/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) defines `VersionSpec`:

- empty, `latest`, `stable`, and `current` mean the newest stable release;
- `lts` and `lts/<name>` mean the newest or named LTS line;
- a complete semver, including prerelease or build metadata, is exact;
- incomplete numbers such as `20` or `20.11` are component prefixes;
- Node project metadata may produce npm-style semver ranges with `||`;
- `system` is a reserved version spec. Generic backends do not currently resolve it to a PATH executable, and the Rust backend currently maps it to `stable`; it should not be presented as a working unmanaged/PATH mode.

Candidate lists are expected in ascending order. [`select_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) scans backwards for the highest match: `latest` and ranges select stable releases, while prefix matching compares dotted components rather than raw string prefixes. `select_version_with_prerelease` provides the documented policy semantics for backends that opt into it. Python, GitHub, and npm-package-backed custom resolvers apply prerelease policy explicitly; the generic resolver and some backends still use `select_version`, so current policy behavior is backend-specific.

## Working-directory precedence

[`resolve_active`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) walks from the current directory to its ancestors, but precedence is globally grouped by source kind rather than simply choosing the nearest file:

1. `[tools]` in `osdk.toml` or `.osdk.toml`;
2. `.tool-versions`;
3. backend-declared idiomatic files, preserving the backend's filename order;
4. Node `package.json#engines.node` or `devEngines.runtime`;
5. user-global `[tools]`.

Consequently, a parent `osdk.toml` beats a child `.nvmrc`. Plain idiomatic files use the first non-empty, non-comment value and strip a leading `v`; `go.mod` and `rust-toolchain.toml` have format-aware parsers. Regression coverage lives beside the implementation in [`version/resolver.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs).

## Project package managers

[`resolve_package_manager`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs) first walks for `npm`, `pnpm`, or `yarn` under `osdk.toml [tools]`, then walks for `package.json#packageManager`, and finally reads `devEngines.packageManager`. Only those three managers and exact semver values are accepted. Missing versions, URLs, paths, hash suffixes, and build suffixes fail explicitly. `packageManager` wins over `devEngines.packageManager`.

## Backend resolution and exceptions

After applying version aliases and one-shot backend options, the CLI calls [`Backend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs). The default implementation returns exact versions without querying a remote list and preserves every request option; non-exact requests call `list_remote_versions` and `select_version`. Skipping the version list for an exact request does not itself establish a cryptographic guarantee: installation applies the active checksum/attestation policy and may proceed with no evidence when `require_checksums=false`.

Some backends override the default. Node handles target architecture and npm ranges; Python handles implementations, variants, catalogs, and prerelease policy; Java handles distributions and JDK/JRE; Rust passes channels or versions to its isolated rustup. Cargo registry tools fetch paired metadata/index source data, remove yanked releases, and resolve exact/latest/numeric-prefix selectors, while Cargo Git selectors are retained verbatim. Go command tools resolve exact/latest/numeric-prefix/pseudo-version selectors through ranked Go proxies and bind the discovered module root. See [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs), [`python.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/python.rs), [`java.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/java.rs), [`rust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/rust.rs), [`cargo_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/cargo_package.rs), and [`go_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/go_package.rs).

## Lockfile fast path and boundary

`osdk install` with neither explicit tools nor extra options first reads the nearest `osdk.lock`. [`locked_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) restores saved version strings, public options, and locked artifact data for the current platform; normal project resolution is used when that platform section is absent. Most backends save an exact version. Floating Rust values such as `stable`, `beta`, and `nightly` remain channel names, so a later rustup install may obtain a newer toolchain. Platform-specific sections let Linux, macOS, and Windows resolutions coexist.

The lock records a resolution and artifact identity, not a claim that existing bytes are trusted. Reinstallation reruns any available or policy-required checksum/attestation checks. If the lock has no digest/evidence and `require_checksums=false`, installation may still proceed without cryptographic integrity verification. Supplying tools or `-o` bypasses this lockfile fast path.

## Verifiable invariants

- Precedence, ancestor walking, structured files, and invalid package-manager values are covered by the [`resolver` unit tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs).
- Prefix, range, LTS, and prerelease behavior is covered by the [`version` unit tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs).
- The exact-version option-preservation regression test is in [`backend/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs).
- Package-manager auto-selection, Node injection, and lock restoration are covered by [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs) and [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).
