# SDK sources and project registries

osdk has two independent network-selection mechanisms. They must not be conflated:

| Mechanism | What it obtains | Configuration | Decision point | Consumer retries |
| --- | --- | --- | --- | --- |
| SDK/tool source | Artifacts and release metadata for Node, Go, Python, manager binaries, `npm:<package>`, and others | `[sources]`, `[sources.<tool>]`, `--source` | before backend resolution and installation | artifact URLs may fail over; an external manager that has started is never rerun |
| Project dependency registry | npm-compatible packages resolved by npm/pnpm/Yarn/Bun/Deno project commands | `[registries.npm]` plus native manager configuration | before `osdk exec` or a shim starts the manager | an eligible invocation makes at most one launch attempt after selection; if every candidate is unhealthy, no launch is attempted |

The source for `osdk install pnpm@11` determines where pnpm itself comes from. The registry for a later `pnpm install` determines where project dependencies are resolved. `--source` never rewrites the project registry.

## SDK source ranking

[`effective_sources`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) starts with backend defaults, removes disabled entries, adds custom sources (a matching id replaces a built-in), drops `enabled=false`, and sorts by ascending `priority`.

[`ranked_source_list`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) then applies this algorithm:

1. A configured pin or one-shot `--source` moves the matching source first while keeping the others as fallbacks. The one-shot key currently uses the user-supplied tool name, so the invocation must use the canonical backend ID; aliases do not receive that override.
2. Offline mode skips probes; it reuses a cached order only when the candidate-set fingerprint matches, otherwise it uses priority order.
3. `ordered` and `pinned` selection also use that order directly.
4. `auto` first reads the per-tool probe cache; it is usable only when every cached result is within the TTL. Cache schema 2 also validates a candidate-set fingerprint, so URL, order, priority, enabled-state, credential-forwarding, or header changes cannot reuse stale results; header values are stored only as hashes.
5. When stale, all sources are probed concurrently, each bounded by `probe_timeout_ms`, reading at most about 1 MiB.
6. Successful probes are sorted by the composite score `throughput - ttfb_ms` in descending order. Failed probes are appended, so real downloads can still use them as final fallbacks.

Each backend owns metadata lookup and artifact URL construction, so a source must actually implement that backend's expected layout. The shared pipeline downloads in URL order; one URL gets up to three transient-error attempts before failover. A successful probe is not an integrity result: checksum or attestation verification remains a separate post-download step. See [`source/select.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/source/select.rs) and [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs).

Explicit `Source.headers` applies to metadata requests and source probes made by
osdk and is independent of `forward_credentials`. Headers are attached only when
the initial URL has the configured index/download origin, survive same-origin
redirects, and are permanently stripped after the first cross-origin redirect.
Only hashes of header values participate in metadata/probe cache identity; clear
values are not persisted. Aube 2.1's embedded API cannot safely accept arbitrary
source headers, so `npm:<package>` Aube package fetches do not forward
`Source.headers`. Project operations may use native trusted configuration;
global npm-tool installs reject authenticated/private native pass-through while
running in their isolated prefix.

## Project registry preflight

The planner lives in [`package_registry.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/package_registry.rs), called by [`apply_package_registry_plan`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs). It handles only an explicit allow-list of commands that may fetch npm packages and first identifies the manager family. An unknown Yarn major is conservatively passed through.

Every eligible invocation performs fresh concurrent anonymous probes; it does not reuse the SDK-source probe cache. The endpoint is the standard npm-compatible `<base>/-/ping` and must return a successful status plus a non-empty JSON object no larger than 64 KiB. Each request has a bounded timeout. The redirect chain may contain at most three URLs, meaning at most two redirects are followed, and it must remain on the original HTTPS origin with no downgrade, cross-origin target, URL credentials, or loop. Probes send no registry token, cookie, or Authorization extracted from native configuration. Ordinary system HTTP(S) proxy settings still apply.

Candidate selection is intentionally precise:

- Explicit `[registries.npm].urls` is the complete candidate set. Project configuration replaces the user-level block, preserves order, and chooses the first healthy entry.
- Without explicit URLs, a recognized anonymous public native registry precedes the built-in npmmirror/npmjs fallbacks.
- Only a purely built-in set chooses the lowest-latency healthy endpoint.
- URLs are normalized and deduplicated.

The selected URL is injected only into the single child process using its native variable: npm `npm_config_registry`, pnpm `pnpm_config_registry`, Yarn Classic `YARN_REGISTRY`, Yarn Berry `YARN_NPM_REGISTRY_SERVER`, Bun `BUN_CONFIG_REGISTRY`, or Deno `NPM_CONFIG_REGISTRY`.

## Single-launch and fail-closed behavior

For an eligible invocation that actually enters registry preflight, osdk makes at most one manager launch attempt after selecting a healthy candidate and never retries after launch; if every candidate is unhealthy, no launch is attempted. Process creation itself can still fail. Conservative pass-through invocations do not enter this fail-closed preflight path.

In the control flow, [`exec_cmd`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/commands.rs) calls `apply_package_registry_plan` before its single `Command::status()` call. The direct-shim path likewise invokes the same planner before the sole `exec` in [`osdk-shim::real_main`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-shim/src/main.rs). `RegistryPlan::Unavailable` becomes an error before startup. A non-zero exit after manager startup is returned directly, without selecting another registry or replaying lifecycle scripts. Unix `osdk exec` integration tests cover:

- [`exec_registry_fallback_injects_only_the_manager_variable_and_runs_once`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs): with an unhealthy primary and healthy fallback, each manager writes exactly one call marker;
- [`exec_registry_never_retries_a_failed_manager_command`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs): a manager exiting 42 still writes one marker only;
- [`exec_registry_all_unavailable_starts_no_manager_process`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs): all probes fail, no marker exists, and the error says the command was not started;
- planner test [`all_failed_candidates_are_unavailable`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/package_registry.rs): all failed probes produce `Unavailable`. The shim's single-launch property follows from control-flow inspection; these integration tests do not exercise the direct shim or Windows paths.

## Conservative pass-through and limits

The manager is passed through without probing, injection, or argument/config rewriting when there is an explicit registry CLI argument, an existing relevant registry environment variable, a strict offline flag, a command outside the network allow-list, an explicit cwd/config argument whose context cannot be reproduced safely, unreadable native configuration, a private or unknown registry, any scoped registry, authentication/TLS/native-proxy policy, or an unknown Yarn major. If osdk cannot establish “anonymous, public, and free of scope/auth policy,” it does not optimize.

A healthy registry proves only that its anonymous metadata endpoint works. Absolute tarball or Git URLs in lockfiles or metadata, local files, workspaces, Git dependencies, Deno JSR, and ordinary URL imports may bypass the selected default registry. osdk neither parses nor rewrites lockfiles and does not proxy credentials. An install pinned to a dead absolute URL may therefore fail, and it is never rerun after the manager starts even if another registry is healthy. The complete design boundary is documented in [`docs/package-registry-design.md`](https://github.com/lejunyang/one-sdk/blob/main/docs/package-registry-design.md).
