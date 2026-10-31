# GitHub Release Tools

```text
github:owner/repo[@VERSION]
```

```bash
osdk use -g github:sharkdp/fd
osdk install github:cli/cli@2.62.0
osdk list-remote github:sharkdp/fd
```

## Asset selection options

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
reads the OS, architecture, and libc from each file name (Rust triples,
goreleaser's `linux_amd64` / `darwin_all`, `win64`, and similar spellings) and
chooses only among assets that fit the host, or the target given with `os=` /
`arch=` / `libc=`:

- a build for another OS is never a fallback, and a musl system is never given
  a glibc build;
- checksums, signatures, SBOMs, installer packages such as `.deb` / `.msi`,
  source bundles, and single-file `.gz` / `.bz2` compression osdk cannot
  extract are excluded;
- when both exist, Linux prefers the static musl build and Windows the MSVC
  build, and archives and non-debug builds win;
- on Windows arm64 and Apple Silicon, a release without a native build falls
  back to the x64 build the OS runs under emulation; an explicit `arch=`
  disables that fallback.

No fitting asset, or several different programs that fit equally well (say
`tool` and `toolctl`), fails with a hint to choose one with `asset-regex`; the
same build offered as `.tar.gz` and `.zip` is not ambiguous. Unknown archive
extensions are treated as bare binaries; Windows normalizes `.exe`.
For osdk-owned GitHub installs, the supported asset, platform, catalog-digest,
and layout options are part of the dynamic installation identity. `catalog-url` is accepted for acquisition but is
excluded from the persisted dynamic identity; its required `catalog-sha256` identifies the
catalog content. HTTP(S) catalog URLs containing userinfo, a query, or a
fragment are rejected so credentials cannot be persisted through this option. An
unknown public option is rejected before installation, and an existing
same-version install with legacy or different
option identity is not executed. Each canonical identity is recorded in
`.osdk-install.json` schema 1 with a `b3-v2:` `install_id` and receives its own
fingerprinted root, so same-version variants coexist. Exact configured identity
drives reuse, activation, shims, `where`, uninstall, and `reshim`; legacy
`.osdk-tool.json` state is detected but never reused or executed.

```bash
osdk install github:owner/repo@1.2.3 \
  -o 'asset-regex=^tool-.*-linux-x64\.tar\.gz$' \
  -o bins=dist/tool,dist/toolctl -o strip-components=1

osdk install github:owner/repo@1.2.3 \
  -o 'asset-template=tool-{version}-{os}-{arch}.zip' \
  -o bin=tool.exe -o rename=mytool -o os=windows -o arch=x64
```

## Static catalog

```bash
osdk lock github:owner/repo@latest \
  -o catalog-url=/approved/github-catalog.json \
  -o catalog-sha256=0123456789abcdef...
```

A catalog can be HTTP(S), `file://`, or a normal local path. Every schema 1
asset contains `name`, `url`, `checksum`, `os`, and `arch`, with optional `libc`;
artifact URLs must be HTTP(S). Online HTTP catalogs are cached by digest, while
offline mode reads that cache. Local files can be read offline directly.

With `catalog-url` configured, `latest` means **the newest version in that catalog**,
and explicit versions must also be present there. New GitHub Releases are not queried.
`osdk use` preserves existing tool options, so changing only `@VERSION` keeps catalog
mode enabled. Floating-version resolution warns that it uses a pinned catalog, and
missing-version errors explain the restriction. To resume live release discovery,
remove both `catalog-url` and `catalog-sha256` from the tool entry in the applicable
project `osdk.toml` or user `config.toml`, then run `osdk use` again. Clearing the
download cache does not change this configuration.

## GitHub access, failover, and tokens

Token priority is `OSDK_GITHUB_TOKEN`, `GITHUB_TOKEN`, then `GH_TOKEN`.
Authorization is sent only to the exact `api.github.com` host and never to a
proxy. The Releases API reads at most 10 pages of 100 entries. On anonymous rate
limiting, public Atom and expanded-assets HTML can provide best-effort discovery
of recent public releases; they are not a complete history.

Built-in `github` and `ghproxy` sources consistently cover API requests, release
assets, Raw/Gist files, checksums/signatures, and attestation bundles, with
ranked failover and no token forwarding. Proxy inputs are normalized to official
URLs first so pins, cache keys, and identities do not depend on proxy spelling.
