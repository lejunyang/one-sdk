# Activation, Shims, and Lockfile Implementation

This page documents behavior in the current source. The main entry points are the [CLI command orchestration](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs), [activation renderer](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs), [shim generator](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs), [shim process](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs), and [lockfile module](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).

## Activation does not install or mutate its parent process

`osdk activate <shell>` only prints shell code; the caller must `eval` or source it. Bash uses `PROMPT_COMMAND`, Zsh registers `precmd_functions`, Fish listens for `PWD` and `fish_prompt`, and PowerShell uses a re-entry-guarded `PostCommandLookupAction`. Every implementation invokes the hook immediately, without waiting for the first directory change. See [`commands::activate`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L1847) and [`activation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L48).

Each hook invocation runs `osdk hook-env`. [`compute_env_delta`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L217) walks the backends, resolves versions again for the current directory, selects installed versions only, and collects real bin directories and backend environment variables. The CLI then layers shared package-manager cache variables and enabled model-provider variables on top. The emitted code rebuilds PATH from its saved original, restores variables that are no longer managed, and sets the current variables, so repeated refreshes do not accumulate path entries. Original values are tracked through `OSDK_ORIGINAL_PATH*`, `OSDK_ORIG_<KEY>*`, and `OSDK_MANAGED_ENV`; [`deactivation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L107) restores them.

## Resolution order and shim priority

The active-version order is defined by [`resolve_active`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs#L154):

1. The nearest ancestor project configuration.
2. For npm, pnpm, and Yarn, only when no project-config package-manager selection applies, `package.json#packageManager` and then `devEngines.packageManager`.
3. The nearest ancestor `.tool-versions`.
4. Backend-declared idiomatic version files.
5. Structured Node version ranges in `package.json`.
6. User-global configuration.

Project and global selections each start from configuration. `osdk use <tool>` writes the nearest project configuration, creating `osdk.toml` in the current directory when none exists; `osdk use --global <tool>` writes `config.toml` in the user config directory. For a project-aware `npm:<package>`, local `use` also updates `osdk.lock` in the real Node project root. `use --global npm:<package>` instead updates `$OSDK_CONFIG_DIR/osdk.lock`; project/global `go:<path>` use does the same in its selected scope. [`config_edit`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/config_edit.rs) publishes one configuration through a unique temporary file, sync, and replace. A project configuration read-modify-write still has no process lock, while global npm mutation uses a dedicated global-state lock to serialize publication of the user lock, shims, and configuration. Go-tool config and lock writes are separate atomic replacements rather than one multi-file transaction.

Activation uses shim-first PATH ordering, but inserts the shim directory only if at least one generated shim and an active real bin directory exist. Package-manager paths follow it, then Node, then other runtimes; see [`prioritize_managed_paths`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L300). Unix shims are symlinks to `osdk-shim`. Windows emits both a `.cmd` wrapper and an extensionless Git Bash wrapper. Before invoking a managed `.cmd` or `.bat`, `osdk-shim` resolves its Windows short path so `cmd.exe` can execute installations whose full path exceeds legacy `MAX_PATH`; argument, standard-stream, and exit-code forwarding stay unchanged. Installs generate shims and `reshim` rebuilds them. A missing shim binary is only a warning: activation can still expose real bin directories.

The shim reloads configuration and selects an installed version for its current working directory, without network access. It removes the shim directory from the child PATH to prevent recursion and adds the real backend bin; JavaScript package managers also receive managed Node. Dependency-fetching npm, pnpm, Yarn, Bun, and Deno commands run a registry preflight before execution. Bundled npm/npx can be routed from Node, but the Node backend does not take ownership away from the independent npm backend. See [`routed_bin_names`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs#L25) and [`osdk-shim::real_main`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs#L27).

Isolated and global `npm:<package>` installs record commands found under their
controlled install roots and relative paths in an inventory. A real-project
install instead validates bins declared by the requested package and publishes
a curated `.osdk/npm-bin/generations/<identity>/bin` directory. Trusted project
activation exposes that validated generation, never the complete
`node_modules/.bin`. At startup, the shim
scans managed inventories to recover backend ownership and adds managed Node to that
backend's PATH. With multiple backend owners, runtime dispatch routes only when
current configuration selects exactly one; otherwise it refuses to choose. CLI
generation and `reshim` always remove the ambiguous managed shim and error for
multiple installed backend owners. Multiple versions of one backend are resolved
by active version selection and are not an owner conflict. See
[npm developer tool implementation](./npm-tools#inventory-shims-and-conflict-rejection).

osdk-owned dynamic `npm:<package>`, `cargo:<crate-or-https-url>`,
`go:<module-or-command-path>`, and `github:owner/repo` installs use
`.osdk-install.json` schema 1. Its nested `identity` records `tool`, `version`,
`platform`, `scope`, `material_options`, `dependencies`, `materials`, and the
canonical `b3-v2:` `install_id`. That fingerprint is part of the physical root,
so same-backend/version variants can coexist. Before activation adds paths or a
shim executes a command, osdk derives the exact configured identity and selects
only its root. Reuse, `where`, uninstall, and `reshim` use the same selection. A
missing, legacy, or mismatched identity fails closed; `.osdk-tool.json` is legacy
detection only, and neither its schema 1 nor schema 2 authorizes execution. This
is distinct from the bin-owner ambiguity check above. Project-managed npm
activation remains on the separately validated `.osdk/npm-bin` generation. A
Cargo native candidate has additional receipt, metadata-seal, binary-digest,
and exact Rust version/platform plus bounded build-critical runtime checks; see
[Cargo developer tool implementation](./cargo-tools).
A Go native candidate applies the same boundary with its exact managed Go
version/platform and bounded build-critical runtime identity; see
[Go developer tool implementation](./go-tools).

## Trust boundary

Both CLI initialization and the shim check trust before loading project configuration. A project file containing only `[tools]` and `[aliases]` needs no explicit trust. Top-level settings, sources, registries, or any other execution/network-affecting section require trust. Identity is the canonical file path plus a BLAKE3 hash of normalized TOML, so editing the content or moving the repository invalidates the record. `OSDK_TRUSTED_CONFIG_PATHS` can authorize canonical files or directories. See [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs).

## `osdk.lock` read semantics

The current writer uses schema 4. It adds an optional typed `native` table for
delegated compiled tools with the managed runtime id, exact runtime version,
and a `version-only`, `immutable-revision`, or `floating-ref` replay grade. A
Cargo registry entry also records its canonical, credential-free sparse HTTPS
index in `native.source`; a Cargo Git entry must not carry that field.
Go entries record their canonical selected proxy in `native.source` and their
discovered module root in `native.module`; both are required for truthful
version-only replay.
Established schema 1 through 3 locks remain readable for non-native tools and
upgrade on their next successful write. Because those schemas cannot express
runtime binding, `cargo:` and Go-module `go:` entries in schemas 1 through 3
fail with an actionable request to regenerate the lock as schema 4. Native
metadata is restored as internal request options and is never duplicated in
the public `options` table.
Native Cargo and Go candidates also revalidate provider receipts, metadata
seals, binary digests, and their exact managed runtime identity before reuse or
execution.

A project lockfile is `osdk.lock` in a project root or one of its ancestors. [`find`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L255) walks from the current directory toward its ancestors and selects the nearest existing file. Only `osdk install` with no tool arguments and no `-o` attempts to consume this project-lock path. Explicit tools or any `-o` bypass it and gather requests from arguments/configuration. Separately, `use --global npm:<package>` and `use --global go:<path>` maintain `osdk.lock` in the user configuration directory. That user lock does not participate in ancestor lookup and is not an input to argument-free project `install`.

Loading parses the whole TOML document and accepts established lock schemas 1 through 3 plus current lock schema 4; other versions are rejected. It then selects only the current platform key. If the file exists but lacks that platform section, the result is “no locked requests,” and the caller falls back to normal configuration resolution. A non-npm tool with a generic artifact table becomes a request for its saved version string with internal artifact URL, filename, optional checksum, and subdirectory options; npm tools explicitly cannot carry a generic artifact receipt. A lock-schema-3 or schema-4 npm tool restores public options plus package, installer, scope, optional exact Node version, and native-lock identity; the main lock has no graph payload or path. Lock schema 2 remains a compatibility-read format: its npm entry identifies `osdk.lock.d/npm/<sha256>.yaml`, and the complete graph is injected as an internal option only after size, symlink, UTF-8, and SHA-256 validation. This legacy lock-schema-2 graph sidecar is unrelated to `.osdk-install.json` schema 1. A lock-schema-1 file containing npm entries is not consumed and must be regenerated. Most backends save an exact version, while a floating Rust channel is still interpreted by rustup at install time. The original `request` is an audit field and does not drive this install's version selection. For backends with generic artifact receipts, a locked checksum is verified on fresh or forced reinstall when one exists; with no digest/evidence and `require_checksums=false`, installation may still proceed without cryptographic integrity verification. The normal CLI reuses an already complete installation before entering the pipeline, without checksum revalidation; dynamic npm/GitHub/Cargo/Go reuse additionally requires the exact `.osdk-install.json` identity. Only an invocation that actually reaches the pipeline can reverify requested attestation on its complete-install fast path. Lock evidence itself is not verification input. Top-level model records are written by `model pull`, but no-argument `osdk install` currently consumes only platform tool records and does not restore models from them. See [npm developer tool implementation](./npm-tools) for npm metadata, install-identity schema 1, and legacy lock-schema-2 sidecar compatibility boundaries.

Platform keys are `os-arch`, with an additional `-musl` suffix for Linux musl. `osdk lock` honors a Node `arch` option when choosing the target platform key; ordinary `upgrade` writes the current host platform key.

## `osdk.lock` write semantics

[`project_lock_path`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L614) chooses the target. If a project config was loaded, the lock is written beside it. Otherwise an existing nearest-ancestor lock is reused; if none exists, `osdk.lock` is created in the current directory.

[`merge_resolved`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L771) reads the complete existing file and preserves other platform sections and top-level model entries, but it **clears and replaces the entire tools table for the target platform**. Consequently, `osdk lock node@20` is not a one-entry merge: tools omitted from this resolution are removed from that platform section. Project-aware npm `use`, global npm `use`, and project/global Go-tool `use` instead use upserts and preserve other tools in the same platform section. npm upserts additionally record Node and, for global native installers, the selected npm/pnpm manager; Go upserts record the selected Go runtime and tool together. Writes always use lock schema 4, including public options that are replayed into later requests and checked against dynamic inventory identity. Established non-native schema 1 through 3 locks remain readable and upgrade on their next successful write. There is deliberately no migration for unpublished pre-schema-4 `cargo:` or Go-module `go:` entries: they must be regenerated. Native replay metadata is restored through internal request options and is never duplicated in the public options table. A schema 1 lock containing npm entries remains rejected for consumption or writing. A schema-2 npm sidecar is read back and validated before migration; current writes atomically replace only the main lock, create no new sidecar, and do not delete an old one. Internal `__osdk_*` options are not serialized. Locally linked Rust toolchains are rejected because they cannot form reproducible remote artifacts. Model pull instead uses `merge_model`, replacing only the same-name `[models]` entry while preserving platforms and other models, but it likewise refuses to migrate schema 1 npm entries.

Saving serializes the full document, writes a unique sibling temporary file containing the PID and a process-local serial, syncs it, replaces `osdk.lock`, and also syncs the parent directory on Unix. The project-lock read-modify-write still has **no process lock**: two concurrent writers can both read old state and the last successful replace may overwrite the other's merge. Readers take no shared lock either. Global npm `use` is the exception: it holds `global-npm-state.lock` while publishing the user lock, shims, and user configuration. Global Go-tool `use` does not yet share that multi-file journal/lock.

## Boundaries to keep in mind

- “Global” means an osdk-controlled user-level selection. Global npm and Go-tool `use` write a user `osdk.lock`, but never rewrite the current project lock. npm uses a distinct global install scope; Go tools keep the same identity-qualified native install roots across project and global selection.
- Activation selects installed versions only. A missing match is skipped; activation never installs it.
- A lockfile records the saved resolution and backend-specific reproduction identity. Project locks do not serialize concurrent writers, while the global npm `use` user-lock write runs under the dedicated state lock. Floating Rust channels are not immutable versions. Reinstallation applies currently available or policy-required verification; a complete installation is not checksum-rehashed.
- The trust store also uses temporary-file rename without a read-modify-write lock, durability guarantee, or portable replace-atomicity guarantee, so concurrent trust/untrust operations can lose updates.
