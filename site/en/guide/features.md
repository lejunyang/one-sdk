# Feature Guide

osdk manages language runtimes, ecosystem tools, model snapshots, download
sources, and shared caches through one command surface. This page is only an
index; each topic page documents the complete command forms, parameters, and
current implementation boundaries.

## Start here

| Guide | What it covers |
| --- | --- |
| [Getting Started](./getting-started) | Global options, install/switch/query/remove workflows, and version aliases |
| [Projects and Configuration](./projects) | Project discovery, native version files, the complete configuration schema, merge rules, and trust |
| [Reproducible Lockfiles](./lockfiles) | How `lock`, `install`, `outdated`, and `upgrade` interact, including stale states |
| [Runtimes and Ecosystem Tools](./runtimes) | Node.js, Python, Java, Go, Rust, Maven, Gradle, and Kotlin |
| [JavaScript Package Managers](./package-managers) | npm, pnpm, Yarn, Bun, Deno, Node dependency handling, and registry preflight |
| [npm Developer Tools](./npm-tools) | Install, pin, execute, upgrade, and restore npm CLI packages with `npm:<package>` |
| [Cargo Developer Tools](./cargo-tools) | Install registry crates or HTTPS Git repositories with an exact managed Rust toolchain |
| [Go Developer Tools](./go-tools) | Install module command packages with one exact managed Go toolchain and a selected proxy |
| [Direct HTTPS Artifacts](./http-artifacts) | Install an exact checksum-pinned file or archive from a strict HTTPS `{version}` template |
| [Model Snapshots](./models) | Hugging Face, ModelScope, file filters, verification, locking, and environment adapters |
| [Sources and Supply-chain Security](./sources-security) | Mirrors, offline mode, pre-releases, checksums, signatures, attestations, and GitHub Releases |
| [Container Runtimes, Registries, and Native Operations](./containers) | Runtime/cache diagnostics, anonymous OCI registry tests, read-only mirror plans, direct native image pulls, and preview-bound cleanup |
| [Storage, Shell, and Extensions](./storage-shell) | CAS, caches, directories, shims, activation, temporary execution, completions, diagnostics, and declarative backends |

## Common paths

After [installing osdk](./installation), continue with the guide that matches
your goal:

- To define a repository's tool versions, read [Projects and Configuration](./projects) and [Reproducible Lockfiles](./lockfiles).
- To manage a language toolchain, read [Runtimes and Ecosystem Tools](./runtimes).
- To pin npm, pnpm, or Yarn, read [JavaScript Package Managers](./package-managers).
- To install Prettier, TypeScript, or a scoped npm CLI package, read [npm Developer Tools](./npm-tools).
- To install ripgrep or another Rust CLI from a registry or Git repository, read [Cargo Developer Tools](./cargo-tools).
- To install gopls or another Go command package, read [Go Developer Tools](./go-tools).
- To install a checksum-pinned executable or archive from a direct HTTPS URL, read [Direct HTTPS Artifacts](./http-artifacts).
- To download a model repository, read [Model Snapshots](./models).
- To configure a corporate mirror or strict verification, read [Sources and Supply-chain Security](./sources-security).
- To inspect Docker, containerd, Buildx, OCI registries, mirror plans, or native cache usage—or to pull an image or narrowly prune native data—read [Container Runtimes, Registries, and Native Operations](./containers).
- To configure your shell, reclaim space, or add a data-only backend, read [Storage, Shell, and Extensions](./storage-shell).

For the internal request path through resolution, download, verification, CAS
materialization, and shim dispatch, read [Implementation](./implementation/).
