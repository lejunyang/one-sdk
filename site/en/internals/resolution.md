# Version resolution internals

This page describes how `osdk` turns project declarations or CLI input into an installable exact version. Version resolution chooses a tool and version only; source ranking and installation transactions happen in later stages.

## From request to exact version

The entry point is [`gather_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands/install.rs). Explicit arguments such as `node@20` are parsed by [`ToolRequest::parse`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs). With no explicit tools, the CLI combines `[tools]` configuration, project package-manager declarations, and Node project metadata. Selecting npm, pnpm, Yarn, or a dynamic `npm:<package>` tool makes [`inject_node_dependency`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands/install.rs) include Node in the same operation; it uses `latest` when the project has no Node declaration.

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
- a leading `=`, as in `=android-36`, is a **verbatim pin**: it requires a character-for-character match among the published versions, never falls back to a looser tier, and does not require the text to be valid semver. This is distinct from "exact", which still degrades through the tiers below when the literal does not match. It exists because some catalogues use identifiers that are not versions and are not mutually exclusive under prefix matching: `android-36` and `android-36.1` are two different API levels, and dotted-component prefix matching makes the former match the latter. A verbatim pin also counts as an explicit request, so it can reach a pre-release without widening the policy for anything else;
- incomplete numbers such as `20` or `20.11` are component prefixes;
- Node project metadata may produce npm-style semver ranges with `||`;
- `system` is a reserved version spec. Generic backends do not currently resolve it to a PATH executable, and the Rust backend currently maps it to `stable`; it should not be presented as a working unmanaged/PATH mode.

Candidate lists are expected in ascending order. [`select_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs) scans backwards for the highest match: `latest` and ranges select stable releases, while prefix matching compares dotted components rather than raw string prefixes. A verbatim pin is compared character-for-character and fails to resolve when nothing matches. Exact requests match in three tiers: literal equality first, then semver core comparison ignoring build metadata (`21.0.12` matches `21.0.12+8`, prerelease identifiers must agree, and the highest build wins), and finally a dotted-component prefix fallback so `21.0.12` reaches a four-part PSU such as `21.0.12.1+1` only when no same-core release exists. The last tier mainly serves Java with its build numbers and PSU fourth segments; strict three-part semver backends never reach it. Before the component-prefix scan runs, the request text is checked against the published versions outright and a hit is returned directly: otherwise `android-36` would select `android-36.1` whenever both exist. `select_version_with_prerelease` provides the documented policy semantics for backends that opt into it; a verbatim pin counts as explicit, just like an exact version carrying a prerelease identifier. Python, GitHub, and npm-package-backed custom resolvers apply prerelease policy explicitly; the generic resolver and some backends still use `select_version`, so current policy behavior is backend-specific.

## Configurable declaration precedence

```toml
[settings.precedence]
tools = "osdk"
packages = "native"
tool_order = [".node-version", ".nvmrc", "osdk"]
tool_conflicts = "error" # default: prefer, select by order
npm = "osdk"           # optional override of packages; inherit clears this override
python = "native"
```

`tool_order` is one shared source-kind order for all tools, highest first; it is not a per-tool map. Only kinds applicable to the requested tool participate. For each filename, the nearest declaration is considered; `idiomatic` also preserves the fixed backend filename order below.
Accepted kinds: `osdk`, `global`, `.tool-versions`, `.nvmrc`, `.node-version`, `idiomatic`,
`package.json#packageManager`, `package.json#devEngines.packageManager`,
`package.json#engines.node`, `package.json#devEngines.runtime`. Omitted kinds are appended in
_default_ order, not disabled; unknown or duplicate entries fail. `[]` clears an inherited custom
order. `idiomatic` groups the other native filenames: Python uses `.python-version`; Go checks `go.mod` before `.go-version`, Rust checks `rust-toolchain.toml` before `rust-toolchain`, and Java checks `.java-version` before `.sdkmanrc`. These names are not independent `tool_order` entries. Backend filename priority applies before directory proximity between those filenames.
The default order is project osdk, .tool-versions, .nvmrc, .node-version, other idiomatic files,
packageManager, devEngines.packageManager, engines.node, devEngines.runtime, global.
`tools="native"` moves project osdk after native kinds. Custom order can explicitly promote global.

Project osdk uses only the nearest configuration and explicit `extends`, not implicit parent merging.
User, extends and project precedence settings merge by field; arrays replace; omitted fields inherit.
Overridden ancestors within a kind are shadowed evidence, not simultaneous conflict constraints; inactive version values are not validated. This includes superseded `extends` declarations: evidence retains their locations without copying their raw versions. Declarations belonging to another tool are ignored without interpreting their version or integrity values. Native manager fields choose the nearest identity across manager names; automatic manager selection rejects unsupported identities.
`osdk current` reports tool, path, field, category, winner/shadowed/ignored and reason from the same selection result.
Known structured fields, JSON fields and simple-file line numbers are reported; unknown paths remain
absent rather than guessed.

`tool_conflicts="error"` compares effective declarations across kinds: an exact version satisfying a
range is compatible; numeric prefixes, supported semver comparators and `||` use pairwise range
overlap after expanding configured aliases. This does not merge ranges into a new request: the
highest-priority declaration still selects the version. Different latest/LTS/channels or opaque selectors
that cannot be proved compatible offline produce a distinct unverifiable error, not a claimed
incompatibility or a network install. `prefer` selects by order but still validates eligible declarations. packageManager requires exact semver without URLs, hashes or build suffixes; devEngines.packageManager accepts a
single object or single-element array with a range, and rejects multiple alternatives explicitly.
Explicit `tool@version`, explicit deps provider/installer choices, and lock replay keep their authority. Shim ownership maintenance retains installed commands when project selection is invalid; it does not execute them. Implicit dispatch still enforces the project conflict policy. Built-in deps providers resolve only the tools they require.

`npm`, `python`, and `go` under `[settings.precedence]` override **package-source** policy for that ecosystem, not tool-version policy. They accept `osdk`, `native`, or `inherit` (use `packages` again); for example, `python="native"` preserves Python package indexes, not `.python-version` priority. They do not reorder native credentials, scope routing or TLS. uv discovery respects sibling uv.toml priority, nearest project configuration and
user/first-system candidates. Complex settings (named indexes, bindings and other unmodeled fields)
remain native as a group; injecting explicit osdk indexes into them under osdk precedence fails. An invocation environment override that prevents injection does not cause this error.
A possible uv workspace delegates the group intact, including membership/exclusions, without osdk
index injection; uv may still validate member metadata. Candidate files are not per-field winners. Requirements/constraint inputs with native options or unreadable context, positional `uv pip sync`/`compile` inputs, and explicit config/directory (`-C` included)/offline/dotenv invocation options, including options before subcommands, are delegated to uv;
false-like UV_NO_CONFIG/UV_OFFLINE/UV_NO_INDEX values do not disable injection merely by existing.
Native files are not rewritten and failed execution is never retried with a different installer.

## Backend resolution and exceptions

After applying version aliases and one-shot backend options, the CLI calls [`Backend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs). The default implementation returns exact versions without querying a remote list and preserves every request option; non-exact requests call `list_remote_versions` and `select_version`. Skipping the version list for an exact request does not itself establish a cryptographic guarantee: installation applies the active checksum/attestation policy and may proceed with no evidence when `require_checksums=false`.

Some backends override the default. Node handles target architecture and npm ranges; Python handles implementations, variants, catalogs, and prerelease policy; Java handles distributions and JDK/JRE; Rust passes channels or versions to its isolated rustup. Cargo registry tools fetch paired metadata/index source data, remove yanked releases, and resolve exact/latest/numeric-prefix selectors, while Cargo Git selectors are retained verbatim. Go command tools resolve exact/latest/numeric-prefix/pseudo-version selectors through ranked Go proxies and bind the discovered module root. See [`node.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/node.rs), [`python.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/python.rs), [`java.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/java.rs), [`rust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/rust.rs), [`cargo_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/cargo_package.rs), and [`go_package.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/go_package.rs).

## Lockfile fast path and boundary

`osdk install` with neither explicit tools nor extra options first reads the nearest `osdk.lock`. [`locked_requests`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs) restores saved version strings, public options, and locked artifact data for the current platform; normal project resolution is used when that platform section is absent. Most backends save an exact version. Floating Rust values such as `stable`, `beta`, and `nightly` remain channel names, so a later rustup install may obtain a newer toolchain. Platform-specific sections let Linux, macOS, and Windows resolutions coexist.

After a successful fallback to project configuration, `install` records that
exact resolution in the current-platform section; replaying an existing lock
does not rewrite it. Project `use` upserts the selected tool and injected
runtimes after installation, rolling its `[tools]` edit back if the lock cannot
be written. Explicit `install TOOL...` changes no project declaration and
therefore writes no lock.

The lock records a resolution and artifact identity, not a claim that existing bytes are trusted. Reinstallation reruns any available or policy-required checksum/attestation checks. If the lock has no digest/evidence and `require_checksums=false`, installation may still proceed without cryptographic integrity verification. Supplying tools or `-o` bypasses the lockfile read fast path; the latter still records new results after a successful bare project install.

## Verifiable invariants

- Precedence, ancestor walking, structured files, and invalid package-manager values are covered by the [`resolver` unit tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs).
- Prefix, range, LTS, and prerelease behavior is covered by the [`version` unit tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/mod.rs).
- The exact-version option-preservation regression test is in [`backend/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/mod.rs).
- Package-manager auto-selection, Node injection, and lock restoration are covered by [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs) and [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).

The shared execution adapter is `package_sources.rs`; version policy is in `version/resolver.rs`. Native file discovery is candidate evidence, not a claim about the native effective winner. See [package managers](../guide/tools/package-managers.md).
