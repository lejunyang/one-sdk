# Activation, Shims, and Lockfile Implementation

This page documents behavior in the current source. The main entry points are the [CLI command orchestration](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs), [activation renderer](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs), [shim generator](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs), [shim process](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs), and [lockfile module](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).

## Activation does not install or mutate its parent process

`osdk activate <shell>` only prints shell code; the caller must `eval` or source it. Bash uses `PROMPT_COMMAND`, Zsh registers `precmd_functions`, Fish listens for `PWD` and `fish_prompt`, and PowerShell uses a re-entry-guarded `PostCommandLookupAction`. Every implementation invokes the hook immediately, without waiting for the first directory change. See [`commands::activate`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L1091) and [`activation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L48).

Each hook invocation runs `osdk hook-env`. [`compute_env_delta`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L216) walks the backends, resolves versions again for the current directory, selects installed versions only, and collects real bin directories and backend environment variables. The CLI then layers shared package-manager cache variables and enabled model-provider variables on top. The emitted code rebuilds PATH from its saved original, restores variables that are no longer managed, and sets the current variables, so repeated refreshes do not accumulate path entries. Original values are tracked through `OSDK_ORIGINAL_PATH*`, `OSDK_ORIG_<KEY>*`, and `OSDK_MANAGED_ENV`; [`deactivation_script`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L107) restores them.

## Resolution order and shim priority

The active-version order is defined by [`resolve_active`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/version/resolver.rs#L154):

1. The nearest ancestor project configuration.
2. For npm, pnpm, and Yarn, only when no project-config package-manager selection applies, `package.json#packageManager` and then `devEngines.packageManager`.
3. The nearest ancestor `.tool-versions`.
4. Backend-declared idiomatic version files.
5. Structured Node version ranges in `package.json`.
6. User-global configuration.

Project and global configuration are not two lockfiles. `osdk use <tool>` writes the nearest project configuration, creating `osdk.toml` in the current directory when none exists; `osdk use --global <tool>` writes `config.toml` in the user config directory. [`config_edit`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/config_edit.rs) performs direct read-modify-write operations for both. There is no file lock or temporary-write-plus-rename boundary, so concurrent writes can lose updates and crashes do not get the publication boundary used by lockfile writes.

Activation uses shim-first PATH ordering, but inserts the shim directory only if at least one generated shim and an active real bin directory exist. Package-manager paths follow it, then Node, then other runtimes; see [`prioritize_managed_paths`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/activate/mod.rs#L300). Unix shims are symlinks to `osdk-shim`. Windows emits both a `.cmd` wrapper and an extensionless Git Bash wrapper. Installs generate shims and `reshim` rebuilds them. A missing shim binary is only a warning: activation can still expose real bin directories.

The shim reloads configuration and selects an installed version for its current working directory, without network access. It removes the shim directory from the child PATH to prevent recursion and adds the real backend bin; JavaScript package managers also receive managed Node. Dependency-fetching npm, pnpm, Yarn, Bun, and Deno commands run a registry preflight before execution. Bundled npm/npx can be routed from Node, but the Node backend does not take ownership away from the independent npm backend. See [`routed_bin_names`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/shim/mod.rs#L25) and [`osdk-shim::real_main`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs#L27).

A dynamic `npm:<package>` install records commands found in the synthetic
project's `.bin` and their relative paths in an inventory. At startup, the shim
scans inventories to recover backend ownership and adds managed Node to that
backend's PATH. With multiple backend owners, runtime dispatch routes only when
current configuration selects exactly one; otherwise it refuses to choose. CLI
generation and `reshim` always remove the ambiguous managed shim and error for
multiple installed backend owners. Multiple versions of one backend are resolved
by active version selection and are not an owner conflict. See
[npm developer tool implementation](./npm-tools#inventory-shims-and-conflict-rejection).

## Trust boundary

Both CLI initialization and the shim check trust before loading project configuration. A project file containing only `[tools]` and `[aliases]` needs no explicit trust. Top-level settings, sources, registries, or any other execution/network-affecting section require trust. Identity is the canonical file path plus a BLAKE3 hash of normalized TOML, so editing the content or moving the repository invalidates the record. `OSDK_TRUSTED_CONFIG_PATHS` can authorize canonical files or directories. See [`trust.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/trust.rs).

## `osdk.lock` read semantics

`osdk.lock` is a project lockfile; there is no separate global lockfile. [`find`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L128) walks from the current directory toward its ancestors and selects the nearest existing `osdk.lock`. Only `osdk install` with no tool arguments and no `-o` attempts to consume it. Explicit tools or any `-o` bypass the lock and gather requests from arguments/configuration.

Loading parses the whole TOML document and accepts legacy schema 1 or current schema 2; other versions are rejected. It then selects only the current platform key. If the file exists but lacks that platform section, the result is “no locked requests,” and the caller falls back to normal configuration resolution. A non-npm tool with a generic artifact table becomes a request for its saved version string with internal artifact URL, filename, optional checksum, and subdirectory options; npm tools explicitly cannot carry a generic artifact receipt. For a schema 2 npm tool, package, Node version, format, digest, and canonical path from the main lock identify `osdk.lock.d/npm/<sha256>.yaml`; only after size, symlink, UTF-8, and SHA-256 validation is the complete graph injected as an internal option. A schema 1 lock containing npm entries is not consumed and must be regenerated. Most backends save an exact version, while a floating Rust channel is still interpreted by rustup at install time. The original `request` is an audit field and does not drive this install's version selection. For backends with generic artifact receipts, a locked checksum is verified on fresh or forced reinstall when one exists; with no digest/evidence and `require_checksums=false`, installation may still proceed without cryptographic integrity verification. The normal CLI reuses an already complete installation before entering the pipeline, without checksum or manifest revalidation; only an invocation that actually reaches the pipeline can reverify requested attestation on its complete-install fast path. Lock evidence itself is not verification input. Top-level model records are written by `model pull`, but no-argument `osdk install` currently consumes only platform tool records and does not restore models from them. See [npm developer tool implementation](./npm-tools) for graph-sidecar generation, hashing, and frozen-install boundaries.

Platform keys are `os-arch`, with an additional `-musl` suffix for Linux musl. `osdk lock` honors a Node `arch` option when choosing the target platform key; ordinary `upgrade` writes the current host platform key.

## `osdk.lock` write semantics

[`project_lock_path`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs#L498) chooses the target. If a project config was loaded, the lock is written beside it. Otherwise an existing nearest-ancestor lock is reused; if none exists, `osdk.lock` is created in the current directory.

[`merge_resolved`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs#L492) reads the complete existing file and preserves other platform sections and top-level model entries, but it **clears and replaces the entire tools table for the target platform**. Consequently, `osdk lock node@20` is not a one-entry merge: tools omitted from this resolution are removed from that platform section. Writes always use schema 2. A schema 1 lock without npm entries upgrades on its next successful write; one containing npm entries is rejected for consumption or writing and must be regenerated. npm graph sidecars are atomically written and read back for validation before the main lock is atomically replaced; both must be committed. Internal `__osdk_*` options are not serialized. Locally linked Rust toolchains are rejected because they cannot form reproducible remote artifacts. Model pull instead uses `merge_model`, replacing only the same-name `[models]` entry while preserving platforms and other models, but it likewise refuses to migrate schema 1 npm entries.

Saving serializes the full document, writes a same-directory `osdk.tmp-<pid>`, and renames it to `osdk.lock`. This provides a temporary-write publication boundary, but there is no fsync/durability guarantee or portable guarantee that rename atomically replaces an existing destination. There is also **no process lock around the read-modify-write transaction**. Concurrent writers can both read stale state, with the last successful rename potentially overwriting the other merge; concurrent writes from one PID to the same path also share a temporary filename. Readers acquire no shared lock.

## Boundaries to keep in mind

- “Global” means user-level version configuration, not a global lockfile.
- Activation selects installed versions only. A missing match is skipped; activation never installs it.
- The lockfile records the saved resolution and artifact identity but does not serialize concurrent writers; floating Rust channels are not immutable versions. Reinstallation applies currently available or policy-required verification; a complete installation is not checksum-rehashed.
- The trust store also uses temporary-file rename without a read-modify-write lock, durability guarantee, or portable replace-atomicity guarantee, so concurrent trust/untrust operations can lose updates.
