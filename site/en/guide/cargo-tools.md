# Cargo Developer Tools

osdk installs Rust command-line applications from crates.io-compatible registries
or canonical HTTPS Git repositories through the `cargo:` namespace. Each tool is
bound to one exact osdk-managed Rust toolchain, so selecting a different Rust
version or changing a build option produces a different installation identity.

::: tip Distinguish the two names first
`rust@1.91.1` installs the Rust toolchain. `cargo:ripgrep@14.1.1` installs the
`ripgrep` crate with that toolchain. A `cargo:` request cannot use an ambient
Cargo, a linked toolchain, or a floating Rust selection.
:::

## Install a registry tool

Select an exact managed Rust version first, then install or pin the Cargo tool:

```bash
osdk use rust@1.91.1
osdk use cargo:ripgrep@14.1
eval "$(osdk activate bash)"
rg --version
```

The two requests may also be installed together without changing project
configuration:

```bash
osdk install rust@1.91.1 cargo:ripgrep@14.1.1
osdk exec --tool rust@1.91.1 --tool cargo:ripgrep@14.1.1 -- rg --version
```

Cargo tools require exactly one explicit or configured Rust request, and it must
be exact. `rust@stable`, `rust@nightly`, `rust@latest`, and linked Rust
toolchains are rejected for this workflow. A project configuration can express
the same selection directly:

```toml
[tools]
rust = "1.91.1"
"cargo:ripgrep" = { version = "14.1", features = ["pcre2"], locked = true }
```

The Cargo tool records the exact managed Rust version/platform and a content identity of
its Cargo/rustc launchers, rustc component payload, and complete selected-target
sysroot library payload as an installation dependency. A metadata receipt avoids
rehashing those large files on every shim launch while still forcing a rehash when
their path, size, or modification time changes. Installing the same crate with another Rust
version therefore does not silently reuse the first build.

## Registry versions

Registry requests use this form:

```text
cargo:<crate>[@SELECTOR]
```

| Request | Selection |
| --- | --- |
| `cargo:ripgrep` or `cargo:ripgrep@latest` | Latest stable, non-yanked release |
| `cargo:ripgrep@14` | Highest stable, non-yanked `14.x` release |
| `cargo:ripgrep@14.1` | Highest stable, non-yanked `14.1.x` release |
| `cargo:ripgrep@14.1.1` | That exact non-yanked release |

Registry crate names are canonicalized to lowercase. Exact semantic versions,
including an explicitly requested prerelease, are accepted when the release
exists and is not yanked. Semver ranges, wildcards, a leading `v`, and Cargo
requirement syntax such as `^14` are not accepted.

`osdk list-remote cargo:ripgrep` lists the versions visible through the selected
Cargo metadata source. osdk currently has crates.io and rsproxy defaults and
uses the normal source probing and cache policy before choosing one.

## Install from an HTTPS Git repository

Use the repository URL as the `cargo:` subject:

```text
cargo:https://git.example.com/team/tool.git@latest
cargo:https://git.example.com/team/tool.git@tag:v1.2.3
cargo:https://git.example.com/team/tool.git@branch:release/1.x
cargo:https://git.example.com/team/tool.git@rev:0123456789abcdef0123456789abcdef01234567
```

Omitting the selector is equivalent to `latest` and installs the repository's
current default-branch HEAD. Tags and branches are passed as explicit Git refs.
Only `rev:` followed by exactly 40 lowercase hexadecimal characters represents
an immutable Git revision.

The repository must be an absolute, canonical HTTPS URL with a repository path.
Credentials, query strings, fragments, backslashes, path traversal, and trailing
slashes are rejected. URL path case is preserved and remains part of the tool
identity.

For a workspace repository, use `crate` to select the package that Cargo should
install:

```toml
[tools]
rust = "1.91.1"
"cargo:https://git.example.com/team/workspace.git" = {
  version = "rev:0123456789abcdef0123456789abcdef01234567",
  crate = "workspace-cli",
  bin = "workspace-cli",
  locked = true,
}
```

`crate` is valid only for a Git source. Git requests do not have a remote version
catalog, so `list-remote` is useful for registry crates only.

## Build options

Options can be stored in a structured tool entry or supplied with `-o` for a
one-shot command. All supported options are part of the installation identity.

| Option | Default | Effect |
| --- | --- | --- |
| `features` | none | Comma-separated feature names; TOML arrays are also accepted and normalized |
| `default-features` | `true` | `false` passes Cargo's `--no-default-features` |
| `bin` | all package binaries | Selects one portable binary name |
| `crate` | repository default | Selects a package in a Git repository; rejected for registry requests |
| `locked` | `false` | `true` passes Cargo's `--locked` |

For example:

```bash
osdk install rust@1.91.1 \
  'cargo:ripgrep[features=pcre2,default-features=false,bin=rg,locked=true]@14.1.1'
```

Changing the feature set, default-feature policy, binary, workspace crate,
`locked` setting, exact Rust dependency, source, or selector selects a distinct
fingerprinted install. Reuse, activation, shims, `where`, `uninstall`, and
`reshim` all require the matching identity.

## Installer choice and isolation

For an eligible online registry install, osdk prefers a controlled
`cargo-binstall` located under its managed Cargo home. Git sources,
`cargo-binstall` itself, requests with `features`, requests with
`default-features=false`, and offline installs go directly to the selected
managed toolchain's `cargo install`. If `cargo-binstall` runs, osdk falls back to
`cargo install` only for exit code 94, the provider's documented "no binary
available" result. Every other non-zero exit, launch failure, or timeout is
terminal.

Both providers run with the ambient environment cleared. osdk supplies a staged
private `HOME`, `CARGO_HOME`, target directory, temporary directory, and install
root; `PATH` starts with the selected managed Rust toolchain's bin directory
and retains sanitized system paths for linkers and build tools, while `RUSTC`
points to that toolchain. The provider cannot install into the user's normal
Cargo home or shadow the selected Cargo/rustc through PATH.

Only validated binaries are published from the stage into the fingerprinted
osdk install root. Cargo source and build directories are temporary provider
workspace and are removed before publication.

## Lifecycle commands

Cargo tools use the normal managed-tool lifecycle:

```bash
osdk current cargo:ripgrep
osdk list cargo:ripgrep
osdk list-remote cargo:ripgrep
osdk where cargo:ripgrep@14.1.1
osdk outdated cargo:ripgrep
osdk upgrade cargo:ripgrep
osdk --yes uninstall cargo:ripgrep@14.1.1
osdk reshim
```

The selected exact Rust toolchain must remain installed and unchanged for a
Cargo tool installation to validate. If multiple matching managed-Rust
identities would otherwise be ambiguous, select the tool through its lockfile.

## Offline behavior and lock replay

An already complete Cargo tool with the exact matching source, selector, public
options, platform, and managed Rust identity can be validated and reused
offline without launching a provider. A cold offline installation, repair, or
rebuild is deliberately unsupported, even if Cargo has some cached content:
Cargo's native lock and osdk's lock do not contain the complete source graph.

Current `osdk.lock` schema 4 records the exact Rust runtime binding and one of
three honest replay classifications:

| Source and selector | Replay classification | Guarantee |
| --- | --- | --- |
| Registry crate at a resolved exact version | `version-only` | Pins the top-level crate version, not its complete dependency/source graph |
| Git `rev:<40 lowercase hex>` | `immutable-revision` | Pins the repository selector to one full commit revision |
| Git `latest`, `tag:`, or `branch:` | `floating-ref` | Records the ref, which may resolve to different source later |

The lock also preserves public build options, the selected canonical,
credential-free sparse HTTPS index for a registry crate, and a matching exact
`rust` entry in the same platform
section. It is sufficient to select and reuse an already complete matching
installation, but it is not an offline source bundle and does not turn a
floating Git ref into an immutable commit.

For parsing, provider selection, identity validation, publication, and lock
schema details, see [Cargo developer tool implementation](./implementation/cargo-tools).
