# Global Command-line Options

These options apply to every subcommand and may appear before or after it. Most also have an `OSDK_*` environment variable; the complete list is in [Environment Variables](./environment).

```text
osdk [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

| Option | Environment | Effect |
| --- | --- | --- |
| `-v`, `--verbose` | `OSDK_LOG` separately sets a tracing filter | Repeat for info, debug, then trace output |
| `-q`, `--quiet` | — | Hide download/install progress; it does not hide normal results or approve deletion |
| `-j N`, `--jobs N` | `OSDK_JOBS` | Maximum concurrent downloads/installations; CLI `0` does not override config, and execution uses at least 1 |
| `--model-jobs N` | `OSDK_MODEL_JOBS` | How many models `model sync` downloads at once (default 2); independent of, and multiplied by, `--jobs`, which parallelizes files within one model |
| `-y`, `--yes` | `OSDK_YES` | Approve uninstall, archive-cache cleanup, and real GC |
| `--source ID` | — | Put source `ID` first while retaining fallbacks; tool requests must use the canonical backend ID (for example, `node`, not `nodejs`) |
| `--refresh-sources` | — | Force re-probing for `install`, `use`, `upgrade`, and `exec`; `model sync` refreshes only with no explicit endpoint/pin, `auto` selection, and online mode; no effect on `lock`, `outdated`, or `list-remote` |
| `--source-mode MODE` | `OSDK_SOURCE_MODE` | `auto` (default) validates a mirror set in the environment and ranks it together with the built-in mirrors; `env` uses only that mirror and fails when it is missing or unusable |
| `--offline` | `OSDK_OFFLINE` | Prohibit network access and use cached metadata/artifacts only |
| `--require-checksums` | `OSDK_REQUIRE_CHECKSUMS` | Reject an artifact without a normal checksum or trusted attestation digest |
| `--attestations POLICY` | `OSDK_ATTESTATIONS` | `off`, `if-available`, or `required` |
| `--prerelease POLICY` | `OSDK_PRERELEASE` | `never`, `if-explicit`, or `allow` |
| `--lang LANG` | `OSDK_LANG` | `en` or `zh`; affects help and argument errors too |
| `-h`, `--help` | — | Show help |
| `-V`, `--version` | — | Show the version |

CLI boolean flags enable a behavior for that invocation; the same flag cannot
turn a configured `true` back to `false`. For example, disable signature
verification with `OSDK_VERIFY_SIGNATURES=false` or configuration.

`--download-cache-max-size BYTES|2GiB|unlimited` (`OSDK_DOWNLOAD_CACHE_MAX_SIZE`) overrides retained download-copy capacity for this invocation. `0` removes copies after success and never limits downloadable file size. See [Caches and Cleanup](../maintenance/cache).
