# Download Sources and Mirrors

SDK/model download sources and project dependency registries are separate control planes. This page covers the former: the source commands, mirrors set in the environment, the effective source list, and probing, ranking, and failover. See [Package Managers](../tools/package-managers#registry-preflight) for npm-compatible registries.

## Source command reference

```text
osdk source list TOOL_OR_PROVIDER
osdk source test TOOL
osdk source test huggingface|modelscope|civitai --model REFERENCE

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
| `test TOOL_OR_PROVIDER --model ...` | Probe Hugging Face/ModelScope repository or exact Civitai LoRA-version metadata and a file sample |
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
`exec`. For `model sync`, it refreshes only when no explicit endpoint or pin
applies, selection is `auto`, and offline mode is disabled. It currently has no
effect on `lock`, `outdated`, or `list-remote`. Model `source test` fails without
`--model`, and `--model` is invalid for an SDK tool.

## Mirrors set in the environment

Every toolchain has its own way to point at a mirror through the environment:
rustup reads `RUSTUP_DIST_SERVER` (and `RUSTUP_UPDATE_ROOT`), the go command
reads `GOPROXY`, and the npm family reads `npm_config_registry`,
`pnpm_config_registry`, `YARN_REGISTRY`, `YARN_NPM_REGISTRY_SERVER`,
`BUN_CONFIG_REGISTRY`, and friends.

By default (`mode = "auto"`) osdk validates such a value and then ranks it
**together with** its built-in mirrors, picking the fastest measured one, instead
of obeying it unconditionally:

- when validation fails (not a valid URL, not https, embedded credentials, or a
  query string or fragment) osdk prints a warning and ignores the value, falling
  back to its built-in mirrors rather than dropping it silently;
- when validation passes the value joins the probe as a candidate with the id
  `env`, visible in `osdk source list <tool>`; if it matches a built-in mirror's
  endpoint it is not listed twice;
- `GOPROXY` values such as `off`, `direct`, and comma- or pipe-separated
  fallback lists are legitimate go settings but are not a single probeable
  mirror, so they are skipped with an explanation.

To obey the environment unconditionally — for example a corporate mirror that
must be used even when it is slower — pass `--source-mode env`. In that mode a
missing or unusable value is an **error** rather than a silent fallback, so a
misconfiguration cannot pass unnoticed.

Precedence: an explicit choice always beats the environment. `osdk source pin`
and the one-shot `--source ID` still win; the environment only competes when
nothing was chosen deliberately.

## Effective source list

```toml
[sources]
selection = "auto"       # auto|pinned|ordered
mode = "auto"            # auto|env, see "Mirrors set in the environment"
probe_timeout_ms = 1500        # SDK/tool probes
model_probe_timeout_ms = 8000  # each model metadata/header/sample phase
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
smaller `priority` values come first. Civitai includes two official sources:
`official` (`civitai.com`) and `official-red` (`civitai.red`). `source pin` can fix either one, while auto mode ranks them for the exact requested version. Project source settings require
[explicit trust](../config/trust).

`headers` is explicit source configuration and is separate from
`forward_credentials`. Metadata requests and source probes made by osdk attach
these headers only when the initial URL has the source's configured
index/download origin. They survive same-origin redirects, are permanently
removed after the first cross-origin redirect, and their clear values are not
written to cache. The managed npm and pnpm delegates receive only a registry
override and an otherwise empty, osdk-owned configuration, so actual
`npm:<package>` package fetches do not forward `Source.headers`.
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
a failure fallback; it is not strict “only this source” enforcement. SDK/tool
probes default to 1500 ms. Model probes instead use
`model_probe_timeout_ms = 8000` as an independent budget for each metadata,
response-header, and 64 KiB sample phase. They choose the smallest non-empty
repository file. A successful response marks the source reachable even if sample
body reading exceeds its budget; throughput is then unknown, not `unreachable`.
An all-failed model probe set is not cached for the normal 6-hour TTL, so a brief
outage cannot pin every source as dead. An invalid TTL currently falls back
silently to 6 hours.

Downloads performed by osdk itself use the shared streaming pipeline rather than
failing after one request. Regular archives retain three attempts with 400 ms and
800 ms backoff. Model files default to six attempts with visible warnings and
1/2/4/8/8-second exponential backoff; `sources.model_download_attempts` and
`sources.model_download_retry_base_ms` are configurable through `osdk config set`.
`sources.model_read_timeout_ms` (default 60000) bounds the no-progress interval, so a
connection that stalls mid-transfer fails that request into the retry above instead of
hanging forever.
A `.partial` file and its ETag/Last-Modified metadata are retained, so a retry
resumes with `Range` + `If-Range`; an ignored or invalid range, changed object, or
changed source URL restarts safely. A source probe only ranks candidates; it does
not prove that an exact version exists, and even HTTP 200 can be a mirror error
page. For archives, standalone binaries, and self-update assets handled directly
by osdk, download, checksum/attestation verification, extraction, and required-file
checks are one candidate attempt. Failure at any stage discards that candidate and
tries the next ranked source; the receipt records only the URL that passed the full
attempt. Model downloads likewise try the remaining ranked sources after one source
exhausts its attempts. Exhausting every source remains terminal.

Delegated installers cannot all be handled by blindly rerunning a failed command,
because an install script may already have produced side effects. osdk therefore
fails over at safe, ecosystem-specific boundaries: Rust reruns the complete rustup
download operation per source; Cargo checks the exact crate version's `config.json`
download endpoint before committing to a sparse index; `pypi:` probes the requested
project page rather than a fixed `pip` page and passes the selected index to uv/pip;
and `go:` supplies one native `GOPROXY` chain joined with `|`. Ordinary project
npm/uv/pip commands still execute once and are not blindly replayed after a possible
lifecycle script. Online metadata access may use stale cache after a request failure;
strict offline mode only reads existing cache, while still re-verifying and unpacking
the cached artifact.
