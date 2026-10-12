---
layout: home

hero:
  name: osdk
  text: One manager for every language SDK
  tagline: A cross-platform CLI that unifies versions, mirrors, caches, lockfiles, and project environments
  image:
    src: /logo.svg
    alt: osdk logo
  actions:
    - theme: brand
      text: Install
      link: /en/guide/start/installation
    - theme: alt
      text: User Guide
      link: /en/guide/
    - theme: alt
      text: Internals
      link: /en/internals/
    - theme: alt
      text: GitHub
      link: https://github.com/lejunyang/one-sdk

features:
  - icon: ◈
    title: One command model for every runtime
    details: Node.js, Python, Java, Go, Rust, Zig, the Android SDK, and tools published on npm, Cargo, Go, PyPI, Conda, or GitHub Releases all use the same install / use / exec.
    link: /en/guide/runtimes/overview
    linkText: Runtimes overview
  - icon: ⌁
    title: Reproducible projects
    details: osdk.toml declares tools and tasks, a platform-partitioned osdk.lock records exact versions and artifact identity, and one install reproduces it for the team and CI.
    link: /en/guide/project/lockfiles
    linkText: Lockfiles
  - icon: ⇄
    title: Fastest mirror, automatically
    details: Probes official sources and authoritative mirrors, ranks them by measured speed, and fails over when an artifact does not verify.
    link: /en/guide/sources/mirrors
    linkText: Sources and mirrors
  - icon: ✓
    title: Integrity and provenance
    details: Verifies upstream checksums and signatures, can require GitHub Artifact Attestations, and asks you to approve security-relevant project configuration.
    link: /en/guide/sources/integrity
    linkText: Integrity and signatures
  - icon: ⧉
    title: Deduplicated, reinstallable offline
    details: Verified files go into a content-addressed store shared across versions; after one online install, --offline reinstalls from cache.
    link: /en/guide/config/storage
    linkText: Directories and storage
  - icon: ⬡
    title: Models, skills, and the system
    details: Lock Hugging Face, ModelScope, and Civitai model snapshots, install skills for AI agents, and diagnose container runtimes and system package managers.
    link: /en/guide/ai/models
    linkText: Model snapshots
---

## Start in three steps

```bash
# Install osdk
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh

# Install Node.js and set the global default
osdk use -g node@20

# Use it directly
node --version
```

[Read the complete installation guide →](/en/guide/start/installation)
