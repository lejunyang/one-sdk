# Offline Mode and Pre-releases

This page explains how to work from local caches only, and how osdk treats pre-release versions.

## Offline mode

```bash
osdk --offline install bun@1.3.14
osdk --offline install                    # may consume the current-platform lock
osdk --offline model sync qwen
```

`--offline` or `OSDK_OFFLINE=true` strictly prohibits network access:

- metadata, SDK archives, model metadata, and every selected file must be cached;
- automatic source probes are skipped; `source test` and `--refresh-sources` on
  SDK-installing commands fail, `model sync` does not refresh, and commands that
  do not support the flag continue to ignore it;
- a cache miss fails explicitly instead of going online;
- for backends with a generic artifact receipt, a lock's artifact URL/checksum
  can support offline reinstall; bytes are reverified when the pipeline actually
  reinstalls with a checksum, while an existing complete GitHub installation is
  reused only when its receipt also matches the locked filename/checksum and its
  dynamic option identity matches;
- `npm:<package>` does not use a generic artifact URL. The schema-4 `osdk.lock`
  retains the schema-3 npm metadata model: it stores only scope, installer, and
  optional native-lock identity, not the
  dependency graph, so that metadata alone cannot cold-restore the graph. A
  complete install can be reused only when its recorded options match; operations
  that support native-lock replay additionally need the installer-owned lock and
  a warmed cache/store. Legacy lock-schema-2 graph sidecars are
  compatibility-read inputs only;
- `cargo:` tools can reuse an already complete installation only when source,
  selector, options, platform, and exact managed Rust identity all match. A cold
  offline install or repair is unsupported because neither the Cargo native lock
  nor `osdk.lock` contains the complete source graph;
- `go:` tools can likewise reuse only an exact complete installation. Their
  schema-4 lock records the selected Go proxy, discovered module root, public
  build options, and exact managed Go identity, but not the transitive module
  graph needed for a cold offline build;
- `attestations=required` additionally needs the proof bundle cached by artifact SHA-256; lock evidence cannot replace verification.

`OSDK_OFFLINE` controls osdk and compatible environment values managed by its
hooks. Whether a project subprocess is completely offline still depends on that
downstream tool's native options.

## Pre-releases

```bash
osdk install bun@canary
osdk install deno@beta
osdk install github:owner/repo@1.2.0-beta.1
osdk --prerelease allow install bun@latest
osdk --prerelease never install bun@canary
```

| Policy | Behavior |
| --- | --- |
| `never` | Reject every pre-release, including an explicit version or channel |
| `if-explicit` | Default; allow a pre-release only through an explicit version or `canary|nightly|beta` channel |
| `allow` | Permit `latest`, prefixes, and ranges to select a pre-release implicitly |

The policy applies to pre-release-aware Python, Bun, Deno, and GitHub backends.
`list-remote` still lists only stable versions. Locks preserve both the original
request and the exact resolved version.
Cargo registry resolution has its own fixed rule: yanked releases are always
removed, `latest` and numeric prefixes select stable releases, and only an exact
Cargo selector can select an explicit non-yanked prerelease.
