# User Guide

osdk manages language runtimes, developer tools, project dependencies, model
snapshots, download sources, and shared caches through one command surface. The
sidebar groups pages by kind of feature; if this is your first time, start with
[Installation](./start/installation) and then [Quick Start](./start/quick-start).

For how requests are resolved, downloaded, verified, and materialized
internally, read [Internals](/en/internals/).

## Getting started

| Page | What it covers |
| --- | --- |
| [Introduction](./start/introduction) | What osdk solves and which platforms and tools it supports |
| [Installation](./start/installation) | Installers, shell setup, building from source, and self-update |
| [Quick Start](./start/quick-start) | The shortest path from installing a tool to reproducing it elsewhere |

## Configuration

| Page | What it covers |
| --- | --- |
| [Configuration Files](./config/files) | Where `config.toml` and `osdk.toml` live, the complete schema, and layer merging |
| [Environment Variables](./config/environment) | Every `OSDK_*` variable and credential variable |
| [Global Command-line Options](./config/cli-options) | Options that apply to every subcommand |
| [Directories and Storage](./config/storage) | Data, cache, and configuration directories and the content-addressed store |
| [Project Configuration Trust](./config/trust) | Which project keys need approval and when trust expires |

## Basics

| Page | What it covers |
| --- | --- |
| [Install, Switch, and Inspect](./basics/commands) | `install`, `use`, `list`, `exec`, aliases, and other everyday commands |
| [Shell Activation and Completions](./basics/shell) | Shims, per-directory switching, and completion scripts |
| [Choosing Which Commands Reach PATH](./basics/path-control) | Exact semantics of `include` / `exclude` / `expose` |

## Project workflows

| Page | What it covers |
| --- | --- |
| [Project Toolchains](./project/discovery) | Where version declarations come from and native version files |
| [External Project Configuration](./project/external-config) | Bind a directory as a project without writing `osdk.toml` into it |
| [Reproducible Lockfiles](./project/lockfiles) | `lock`, `install`, `outdated`, `upgrade`, and stale states |
| [Project Tasks](./project/tasks) | Replace Makefiles with `[tasks]`, including dependencies, parallelism, and platform variants |
| [Application Dependencies](./project/deps) | Drive package managers with `osdk deps` to install a project's own dependencies |
| [GitHub Actions](./project/github-actions) | Install osdk in CI, reuse caches, and reproduce the project environment |

## Languages and runtimes

| Page | What it covers |
| --- | --- |
| [Runtimes Overview](./runtimes/overview) | Shared command forms and a backend overview |
| [Node.js](./runtimes/node) · [Python](./runtimes/python) · [Java and JVM Tools](./runtimes/java) | Install options and runtime-specific commands |
| [Go](./runtimes/go) · [Rust](./runtimes/rust) · [Zig](./runtimes/zig) | Same as above |
| [Android SDK](./runtimes/android) | NDK, platform-tools, system images, licenses, and virtual devices |

## Package managers and developer tools

| Page | What it covers |
| --- | --- |
| [Package Managers](./tools/package-managers) | npm, pnpm, Yarn, Bun, Deno, registry preflight, and managed global installs |
| [npm Tools](./tools/npm) · [Cargo Tools](./tools/cargo) · [Go Tools](./tools/go) | Install CLI tools per ecosystem |
| [Python Tools](./tools/python) · [Conda Tools](./tools/conda) | Same as above |
| [GitHub Release Tools](./tools/github-releases) | Install from any public GitHub Release |
| [Direct HTTPS Artifacts](./tools/http-artifacts) | Install from an exact checksum-pinned HTTPS URL |
| [Declarative Backends](./tools/declarative-backends) | Describe a new tool in a data file, without code |

## Models and AI

| Page | What it covers |
| --- | --- |
| [Model Snapshots](./ai/models) | Download, verify, lock, and expose Hugging Face, ModelScope, and Civitai snapshots |
| [Agent Skills](./ai/skills) | Install skills for Claude Code, Codex, Cursor, and other agents |

## Sources and security

| Page | What it covers |
| --- | --- |
| [Download Sources and Mirrors](./sources/mirrors) | Mirror probing, pinning, custom sources, and failover |
| [Offline Mode and Pre-releases](./sources/offline) | Working from caches only, and the pre-release policy |
| [Integrity, Signatures, and Attestation](./sources/integrity) | Checksums, signatures, GitHub Artifact Attestation, and TLS |

## System integration

| Page | What it covers |
| --- | --- |
| [Container Runtimes](./system/containers) | Docker, containerd, and Buildx diagnostics, registry tests, and mirror apply |
| [System Package Managers](./system/packages) | `[sys.pkg]` declarations, winget and Linux package managers, mirror apply |

## Maintenance

| Page | What it covers |
| --- | --- |
| [Caches and Cleanup](./maintenance/cache) | Cache layers and the `cache` and `prune` commands |
| [Diagnostics and Install Verification](./maintenance/diagnostics) | `osdk doctor` and finding installs that changed after installation |
