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
      text: Get Started
      link: /en/guide/installation
    - theme: alt
      text: Explore Features
      link: /en/guide/features
    - theme: alt
      text: GitHub
      link: https://github.com/lejunyang/one-sdk

features:
  - icon: ◈
    title: One CLI, Many SDKs
    details: Manage Node.js, Python, Java, Go, Rust, pnpm, Yarn, Deno, Bun, and GitHub Release tools with one command model.
  - icon: ⧉
    title: Cross-Version Deduplication
    details: A BLAKE3 content-addressed store keeps one copy of identical files and safely materializes versions with hardlinks, reflinks, or copies.
  - icon: ⇄
    title: Automatic Fastest Mirrors
    details: Probe official sources and authoritative mirrors, select by speed, and fail over when metadata or artifact downloads fail.
  - icon: ⌁
    title: Reproducible Projects
    details: Read project version files and generate a platform-aware osdk.lock with exact versions, URLs, and verification data.
  - icon: ✓
    title: Integrity and Provenance
    details: Verify upstream checksums, enforce strict checksum policies, and validate GitHub Artifact Attestations with Sigstore.
  - icon: ⬡
    title: Offline-First Shared Caches
    details: Cache metadata and artifacts while sharing downstream npm, pip, Cargo, Go, Gradle, and other package caches.
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

[Read the complete installation guide →](/en/guide/installation)
