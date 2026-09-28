# GitHub Actions

The repository ships a composite `setup-osdk` action in its root `action.yml`.
It belongs here rather than in a separate repository because the action, release
installer, CLI flags, and cache layout form one versioned contract. A dedicated
repository would only be useful later if the action needs an independent release
cadence or a permanently moving major tag.

## Quick start

Checkout must happen first: the action reads `osdk.lock`, project configuration,
and native dependency locks when building the cache key and materializing the
environment.

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: lejunyang/one-sdk@main
  - run: osdk run ci
```

Use a release tag or full commit SHA instead of `main` in a production workflow.
When `version` is omitted, a semver action tag such as `v0.0.4` selects the same
osdk release; a branch or commit checkout falls back to the latest release.

The default setup performs these steps:

1. install the two checksum-verified release binaries without modifying a shell
   profile;
2. restore osdk's data and cache directories;
3. run `osdk install --no-deps`;
4. run `osdk deps --frozen --no-install-tools`;
5. export the selected tools, runtime variables, and package-manager cache
   variables to later workflow steps.

`--no-install-tools` is deliberate in CI: the package manager must come from the
reviewed project tool configuration or lock. Set `allow-deps-tool-install: true`
when a dependency provider should be allowed to acquire it.

## Inputs

| Input | Default | Meaning |
| --- | --- | --- |
| `version` | action semver tag, otherwise `latest` | osdk release to install |
| `repository` | `lejunyang/one-sdk` | repository publishing release assets |
| `download-base-url` | `https://github.com` | GitHub or trusted download mirror |
| `target` | detected | release target triple override |
| `working-directory` | `.` | project directory to materialize |
| `cache` | `true` | restore/save osdk data and package caches |
| `cache-key` | `default` | extra cache partition; expressions are accepted |
| `install-tools` | `true` | run the project tool installation |
| `install-deps` | `true` | install declared application dependencies |
| `frozen` | `true` | require each dependency provider's native lock |
| `allow-deps-tool-install` | `false` | let `deps` acquire a missing package manager |
| `jobs` | osdk default | concurrent downloads/installations |
| `source-mode` | `auto` | `auto` ranks mirrors; `env` requires environment configuration |
| `offline` | `false` | prohibit network use after cache restoration |
| `require-checksums` | `false` | reject project tool artifacts without checksums |
| `attestations` | `if-available` | `off`, `if-available`, or `required` |

The action outputs `osdk-version` and the underlying cache action's `cache-hit`
value.

For a monorepo sub-project:

```yaml
- uses: lejunyang/one-sdk@main
  with:
    working-directory: apps/api
    jobs: 4
    cache-key: ${{ hashFiles('tooling/company-policy.lock') }}
```

To install only osdk and export a cache-restored environment, disable both
materialization steps:

```yaml
- uses: lejunyang/one-sdk@main
  with:
    install-tools: false
    install-deps: false
```

## What is cached

The cache contains the action-scoped `OSDK_DATA_DIR` and `OSDK_CACHE_DIR` under
the runner temporary directory. Together these cover tool installations, the
content-addressed store, rustup/Cargo state, release downloads, source metadata,
and the native caches osdk assigns to npm, pnpm, Yarn, Bun, uv/pip, Go, Cargo,
Gradle, and Deno.

The key includes runner OS, architecture, the actual installed osdk version, the
custom `cache-key`, and a hash of supported project manifests and lockfiles. A
prefix restore deliberately allows an older dependency snapshot to seed a new
one: osdk still selects the requested identities and ignores non-matching cached
versions, while unchanged content can be reused. OS and architecture are never
mixed.

Project outputs such as `node_modules` and `.venv` are not cached. Their package
manager caches are; the frozen install reconstructs the project tree and avoids
restoring an opaque, potentially stale working-directory snapshot.

The nested cache action is pinned to the full commit for `actions/cache` v4.3.0.
v4 uses GitHub's current cache service while retaining compatibility with older
self-hosted runners than the Node 24 based v5/v6 releases require.

Treat a restored tool-install cache as trusted CI state. Normal osdk reuse checks
the recorded installation identity and completion marker but does not re-hash an
already complete static tool on every invocation. GitHub scopes caches by key and
branch, and fork pull requests cannot save into the base repository's cache; do
not combine `pull_request_target`, an untrusted checkout, and secrets. Set
`cache: false` or rotate `cache-key` when that trust boundary is inappropriate.

## Reproducible CI example

```yaml
permissions:
  contents: read

steps:
  - uses: actions/checkout@v4
  - uses: lejunyang/one-sdk@main
    with:
      frozen: true
      allow-deps-tool-install: false
      require-checksums: true
      attestations: required
  - run: osdk run test
```

`offline: true` is useful only when the restored cache is known to be complete;
it intentionally turns a missing object into a failure instead of reaching the
network.
