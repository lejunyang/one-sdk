# Introduction

osdk (one SDK manager) is a multi-language SDK version manager for Windows,
macOS, and Linux. It brings operations commonly spread across nvm, pyenv,
SDKMAN!, rustup, and similar tools into one command model, directory layout,
cache, and project configuration.

## Why osdk?

Modern development often requires JavaScript, Python, Java, Go, and Rust
toolchains at the same time. Each ecosystem brings a different version manager,
mirror setup, cache location, and activation mechanism. The result is repeated
downloads, wasted disk space, and environments that are difficult to reproduce.

osdk focuses on four problems:

1. **One interface:** install, switch, lock, upgrade, remove, and execute tools
   with consistent commands.
2. **Less duplication:** let installed versions reuse identical files and keep
   ecosystem caches in managed locations.
3. **Speed with trust:** automatically choose available sources, verify upstream
   checksums, verify signatures where a backend supports them, and optionally
   enforce GitHub Artifact Attestations.
4. **Unified model assets:** download, verify, cache, and lock Hugging Face,
   ModelScope, and exact Civitai LoRA-version snapshots.

## Supported platforms and tools

osdk runs natively on Windows, macOS, and Linux. It currently includes these
backends:

| Category | Supported today |
| --- | --- |
| Runtimes | Node.js, Python, Java JDK/JRE, Go, Rust, Zig, Deno, Bun |
| Package managers and JVM tools | npm, pnpm, Yarn, Maven, Gradle, Kotlin |
| Android SDK | NDK, platform-tools, build-tools, cmdline-tools, platforms, emulator, and system images |
| Other developer tools | npm CLI packages through `npm:<package>`, registry crates or HTTPS Git repositories through `cargo:...`, Go command packages through `go:<module-or-command-path>`, Python CLIs through `pypi:<project>`, conda packages through `conda:<package>`, public GitHub Releases through `github:owner/repo`, or exact checksum-pinned artifacts through `http:https://...{version}...` |
| Model providers | Hugging Face, ModelScope, Civitai LoRA versions |
| Project inputs | `osdk.toml`, `.tool-versions`, and common ecosystem version files |
| Shells | Bash, Zsh, Fish, PowerShell |

## Configuration precedence

The overall precedence is, from highest to lowest:

1. command-line options;
2. `OSDK_*` environment variables;
3. the discovered `osdk.toml` or `.osdk.toml`;
4. user-level `config.toml`;
5. built-in defaults.

The file layers are not recursively deep-merged. A project `[settings]` section
replaces the entire lower-precedence settings value, so omitted keys return to
built-in defaults. The top-level source selection, probe timeout, and TTL are
also replaced as a group; source entries merge by tool, but a same-tool entry is
replaced wholesale. `[registries]` replaces the lower section as a unit.
`[tools]` and `[alias.tools]` merge by key. See [Configuration Files](../config/files)
for the complete schema and exact rules, and [Environment Variables](../config/environment)
for every variable.

osdk also reads `.tool-versions` and native files such as `.nvmrc`,
`.python-version`, `go.mod`, and `rust-toolchain.toml`. Commands do not all
enumerate those files in the same way; see [Project Toolchains](../project/discovery#project-version-discovery).

## Next steps

- [Install osdk](/en/guide/start/installation)
- [Quick Start](/en/guide/start/quick-start)
- [Browse the user guide](/en/guide/)
- [Read the internals](/en/internals/)
- [Browse the source](https://github.com/lejunyang/one-sdk)
