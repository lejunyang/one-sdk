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

osdk focuses on three problems:

1. **One interface:** install, switch, lock, upgrade, remove, and execute tools
   with consistent commands.
2. **Less duplication:** keep identical content once in a BLAKE3
   content-addressed store.
3. **Speed with trust:** automatically choose fast sources while retaining
   checksum, signature, and optional GitHub Artifact Attestation verification.

## Supported platforms and tools

osdk runs natively on Windows, macOS, and Linux. It currently includes these
backends:

| Tool | Distribution mechanism |
| --- | --- |
| Node.js | Prebuilt nodejs.org archives with `SHASUMS256` |
| npm | Installed with Node.js |
| pnpm | Official npm platform package with SRI verification |
| Yarn | `yarn` / `@yarnpkg/cli-dist` npm packages |
| Python | python-build-standalone release index and Astral mirror |
| Java | Foojay JDK/JRE plus embedded Temurin LTS catalog |
| Maven / Gradle / Kotlin | Independent JVM tool backends with upstream checksums |
| Go | go.dev download index and SHA-256 |
| Rust | Isolated rustup toolchain home |
| Deno | Official npm platform package |
| Bun | Official npm platform package |
| GitHub Release | Generic `github:owner/repo` backend |

## How it works

Every installation passes through one pipeline:

1. Resolve the version request and user aliases.
2. Probe and select a source.
3. Download the artifact or reuse the cache.
4. Verify checksums, signatures, or attestations.
5. Extract safely.
6. Ingest files into the content-addressed store.
7. Materialize the installation with hardlinks, reflinks, or copies.
8. Generate shims so the project or global version is directly executable.

When the version directory and content store share a filesystem, osdk prefers
hardlinks. It automatically falls back when they are unavailable without
sacrificing correctness.

## Configuration precedence

Configuration is merged in this order, with earlier sources taking precedence:

1. Command-line flags
2. `OSDK_*` environment variables
3. The nearest `osdk.toml` or `.osdk.toml`
4. User-level `config.toml`
5. Built-in defaults

osdk also reads `.tool-versions` and ecosystem files such as `.nvmrc`,
`.python-version`, `go.mod`, and `rust-toolchain.toml`.

## Next steps

- [Install osdk](/en/guide/installation)
- [Explore the feature reference](/en/guide/features)
- [Browse the source](https://github.com/lejunyang/one-sdk)
