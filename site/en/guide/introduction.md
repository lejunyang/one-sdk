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
4. **Unified model assets:** download, verify, cache, and lock Hugging Face and
   ModelScope snapshots.

## Supported platforms and tools

osdk runs natively on Windows, macOS, and Linux. It currently includes these
backends:

| Category | Supported today |
| --- | --- |
| Runtimes | Node.js, Python, Java JDK/JRE, Go, Rust, Deno, Bun |
| Package managers and JVM tools | npm, pnpm, Yarn, Maven, Gradle, Kotlin |
| Other developer tools | npm CLI packages through `npm:<package>`, or public GitHub Releases through `github:owner/repo` |
| Model providers | Hugging Face, ModelScope |
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
`[tools]` and `[aliases]` merge by key. See [Projects and Configuration](./projects)
for the complete schema and exact rules.

osdk also reads `.tool-versions` and native files such as `.nvmrc`,
`.python-version`, `go.mod`, and `rust-toolchain.toml`. Commands do not all
enumerate those files in the same way; see [Project version discovery](./projects#project-version-discovery).

## Next steps

- [Install osdk](/en/guide/installation)
- [Get started](/en/guide/getting-started)
- [Browse the feature guide](/en/guide/features)
- [Read the implementation guide](/en/guide/implementation/)
- [Browse the source](https://github.com/lejunyang/one-sdk)
