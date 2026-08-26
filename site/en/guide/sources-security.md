# Sources and Supply-chain Security

SDK/model download sources and project dependency registries are separate
control planes in osdk. This page covers download sources, offline mode,
pre-releases, checksums, signatures, GitHub attestations, and the generic GitHub
Release backend. For npm-compatible registries, see
[JavaScript Package Managers](./package-managers#registry-preflight).

## Source command reference

```text
osdk source list TOOL_OR_PROVIDER
osdk source test TOOL
osdk source test huggingface|modelscope --model owner/repo[@revision]

osdk source add TOOL_OR_PROVIDER
  --id ID
  --download-url URL
  [--index-url URL]
  [--forward-credentials]

osdk source remove TOOL_OR_PROVIDER ID
osdk source pin TOOL_OR_PROVIDER ID
osdk source unpin TOOL_OR_PROVIDER
```

| Command or argument | Effect |
| --- | --- |
| `list TOOL_OR_PROVIDER` | List effective sources, types, URLs, and the pin |
| `test TOOL_OR_PROVIDER` | Force a probe for an SDK backend and print throughput/TTFB ranking |
| `test TOOL_OR_PROVIDER --model ...` | Probe real Hugging Face/ModelScope repository metadata and a file sample |
| `add TOOL_OR_PROVIDER --id ID --download-url URL` | Add or replace a user-global custom source with that ID |
| `--index-url URL` | Use a separate metadata/index endpoint |
| `--forward-credentials` | Permit a custom model endpoint to receive provider credentials |
| `remove TOOL_OR_PROVIDER ID` | Remove a user-global custom source |
| `pin TOOL_OR_PROVIDER ID` / `unpin TOOL_OR_PROVIDER` | Set or remove a source pin in user configuration |

`add`, `remove`, `pin`, and `unpin` all edit user `config.toml`. `--source ID`
is an invocation-only preference that retains other sources as fallbacks. Tool
requests in that invocation must use the canonical backend ID, such as `node`
rather than `nodejs`, or the current implementation will not apply the override.
`--refresh-sources` forces re-probing for `install`, `use`, `upgrade`, and
`exec`. For `model pull`, it refreshes only when no explicit endpoint or pin
applies, selection is `auto`, and offline mode is disabled. It currently has no
effect on `lock`, `outdated`, or `list-remote`. Model `source test` fails without
`--model`, and `--model` is invalid for an SDK tool.

## Effective source list

```toml
[sources]
selection = "auto"       # auto|pinned|ordered
probe_timeout_ms = 1500
cache_ttl = "6h"

[sources.node]
pin = "corp"
disable = ["tuna"]

[[sources.node.custom]]
id = "corp"
kind = "custom"          # official|mirror|custom
download_url = "https://mirror.example/node/"
index_url = "https://mirror.example/node/index.json"
headers = [["X-Example", "value"]]
forward_credentials = false
priority = 0
enabled = true
```

The effective list is built-ins minus `disable`, plus `custom`. A custom source
with the same ID overrides a built-in, `enabled=false` entries are filtered, and
smaller `priority` values come first. Project source settings require
[explicit trust](./projects#project-configuration-trust).

`headers` is explicit source configuration and is separate from
`forward_credentials`. Metadata requests and source probes made by osdk attach
these headers only when the initial URL has the source's configured
index/download origin. They survive same-origin redirects, are permanently
removed after the first cross-origin redirect, and their clear values are not
written to cache. Aube 2.1's embedded API cannot safely receive arbitrary
`Source.headers`, so actual `npm:<package>` package fetches do not forward them.
Project package-manager invocations may use native trusted configuration, but
global npm tools reject authenticated/private native pass-through while their
prefix is isolated; use an anonymous configured registry for global installs.

## Selection, probing, and failover

| `selection` | Behavior |
| --- | --- |
| `auto` | Reuse a ranking inside the TTL or probe in parallel; rank primarily by throughput with a TTFB penalty |
| `ordered` | Keep `priority` order without probing |
| `pinned` | Behave like `ordered` when no concrete `sources.<tool>.pin` exists |

A concrete pin moves that source to the front but retains every other source as
a failure fallback; it is not strict “only this source” enforcement. Defaults are
a 1500 ms probe timeout and 6-hour cache TTL. An invalid TTL currently falls
back silently to 6 hours. SDK probes read at most about 1,000,000 bytes; model
probes sample at most 1 MiB.

When metadata or a download fails, the backend tries the remaining ranked
candidates. Online metadata access may use a stale cached value after a request
failure. Strict offline mode only reads an existing cache.

## Offline mode

```bash
osdk --offline install bun@1.3.14
osdk --offline install                    # may consume the current-platform lock
osdk --offline model pull qwen hf:Qwen/Qwen2.5-7B-Instruct@main
```

`--offline` or `OSDK_OFFLINE=true` strictly prohibits network access:

- metadata, SDK archives, model metadata, and every selected file must be cached;
- automatic source probes are skipped; `source test` and `--refresh-sources` on
  SDK-installing commands fail, `model pull` does not refresh, and commands that
  do not support the flag continue to ignore it;
- a cache miss fails explicitly instead of going online;
- for backends with a generic artifact receipt, a lock's artifact URL/checksum
  can support offline reinstall; bytes are reverified when the pipeline actually
  reinstalls with a checksum, while an existing complete installation is reused;
- `npm:<package>` does not use a generic artifact URL. Schema 3 `osdk.lock`
  stores only scope, installer, and optional native-lock identity, not the
  dependency graph, so that metadata alone cannot cold-restore the graph. A
  complete install can be reused; operations that support native-lock replay
  additionally need the installer-owned lock and a warmed cache/store. Schema 2
  graph sidecars are compatibility-read inputs only;
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

## Integrity, signatures, and Attestation

```bash
osdk --require-checksums install node@20
osdk --attestations if-available install github:cli/cli@latest
osdk --attestations required install github:cli/cli@latest
```

Ordinary checksums support SHA-256, SHA-512, and BLAKE3. npm SRI supports
`sha256-` and `sha512-`, preferring SHA-512 when both exist.
`--require-checksums` means an artifact must have either an ordinary checksum or
a trusted artifact SHA-256 from a verified attestation. A discovered checksum is
persisted with the archive cache and is checked again when the pipeline actually
performs an offline reinstall.

`settings.verify_signatures=true` enables signature verification by default;
set `OSDK_VERIFY_SIGNATURES=false` to disable it explicitly. This currently
applies to Minisign manifests for backends with a built-in trusted public key,
currently `github:jdx/mise`. A missing manifest/signature can fall through to
other checksum mechanisms, but an invalid signature is a hard failure.

GitHub Artifact Attestation policies are:

| Policy | Behavior |
| --- | --- |
| `off` | Default; do not query attestations |
| `if-available` | Absence is allowed; a discovered invalid, malformed, or repository-mismatched bundle fails |
| `required` | A valid bundle must be present |

Verification binds the artifact SHA-256, `owner/repo`, GitHub Actions OIDC
issuer, Fulcio certificate chain and SCT, DSSE subject, Rekor body/SET/checkpoint/
Merkle inclusion, and signing time. GitHub v0.3 TSA bundles instead verify the
embedded GitHub trust root, timestamp, certificate chain, signature, digest, and
repository claim. The proof API fetches at most 30 entries per request. Bundle
URLs and redirects must be HTTPS; compressed input and expanded JSON are each
limited to 8 MiB.

## Arbitrary GitHub Release tools

```text
github:owner/repo[@VERSION]
```

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

### Asset selection options

Pass every option through repeatable `-o|--opt KEY=VALUE`:

| Option | Values and effect |
| --- | --- |
| `asset-regex=REGEX` | Select by regex; exactly one asset must match |
| `asset-template=TEMPLATE` | Select an exact filename using `{version}`, `{os}`, `{arch}`, and `{libc}` |
| `bin=PATH` | Safe relative path to one binary inside an archive |
| `bins=P1,P2` | Multiple binary paths; mutually exclusive with `bin` |
| `rename=NAME` | Rename a single output binary; requires exactly one final bin |
| `strip-components=N` | Descend through N unique non-`.osdk-*` directories after extraction |
| `os=VALUE` | `linux|macos|darwin|windows` |
| `arch=VALUE` | `x64|x86_64|amd64|arm64|aarch64|x86|i686|arm|armv7` |
| `libc=VALUE` | `gnu|musl|none` |
| `catalog-url=URL_OR_PATH` | Use a schema 1 static catalog and bypass the Releases API |
| `catalog-sha256=HEX` | Required with `catalog-url`; verifies exact catalog bytes |
| `catalog-subdir=PATH` | Record/restore an archive subdirectory for a locked artifact |

`asset-regex` and `asset-template` are mutually exclusive. Without a rule, osdk
scores by host OS, architecture, and archive type while excluding checksum,
signature, and source assets. Zero or multiple matches fail. Unknown archive
extensions are treated as bare binaries; Windows normalizes `.exe`.

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1

osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

### Static catalog

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

A catalog can be HTTP(S), `file://`, or a normal local path. Every schema 1
asset contains `name`, `url`, `checksum`, `os`, and `arch`, with optional `libc`;
artifact URLs must be HTTP(S). Online HTTP catalogs are cached by digest, while
offline mode reads that cache. Local files can be read offline directly.

### GitHub access, failover, and tokens

Token priority is `OSDK_GITHUB_TOKEN`, `GITHUB_TOKEN`, then `GH_TOKEN`.
Authorization is sent only to the exact `api.github.com` host and never to a
proxy. The Releases API reads at most 10 pages of 100 entries. On anonymous rate
limiting, public Atom and expanded-assets HTML can provide best-effort discovery
of recent public releases; they are not a complete history.

Built-in `github` and `ghproxy` sources consistently cover API requests, release
assets, Raw/Gist files, checksums/signatures, and attestation bundles, with
ranked failover and no token forwarding. Proxy inputs are normalized to official
URLs first so pins, cache keys, and identities do not depend on proxy spelling.
