# External Project Config — Validation Record

Scope: `osdk project add/show/list/relocate/remove`, binding stores under
`<OSDK_CONFIG_DIR>/projects/<root-id>/`, and unified `ProjectContext` discovery
across CLI, shim, trust, lock, and inheritance.

- Base commit: `249306fa`

Each check below lists the commit it ran against, the command scope, the actual
result, and its log.

## Key behaviours covered by automated tests

- Discovery priority: the nearest ancestor wins. A same-root local `osdk.toml`
  shadows the external bundle; a nearer external child beats a far local parent.
- Binding integrity: `project.toml` manifest (schema 1) plus `osdk.toml` and
  `osdk.lock` are created by `add`. The lock writes `schema = 3`
  (`crates/osdk-cli/src/lockfile/mod.rs:2147`), the same value the ordinary
  lockfile uses — not an external-only schema.
- Trust gate: untrusted bundle refuses gated installs; rewriting a governed key
  invalidates the record; relocate moves the bundle and the new path is reviewed
  again from scratch.
- Shim forwarding: with a fake static install the shim resolves the external
  bundle, forwards stdout (marker), exit code (7), and keeps diagnostics on
  stderr only.
- Write targets: tool pins (`use`), `config set`, and the project lock are
  written to the bundle. Tasks, deps, and skills output resolve against the real
  project tree; only the config pin and its lock live in the bundle.
- Concurrency: 10-way `add` races and mixed `relocate`/`remove` races leave
  either zero bindings or one complete bundle.
- Junction binding resolves through to the canonical real root (Windows only).

## Environment evidence

- Linux: `cargo test --workspace --no-fail-fast` at `5495518`, RC 0, 15 test
  result lines all ok (core 1559 pass / 1 ignored, conda 2 pass / 3 ignored,
  zig 0 pass / 1 ignored). Log: `C:\Temp\osdk-lw\logs-win\final-5495518-workspace.log`.
  Commit attribution comes from the pre-run checkout, not from the log itself.
- Wine: `./scripts/windows-wine-tests.sh` (internally
  `cargo test --locked --workspace --target x86_64-pc-windows-gnu`) at `2fb869d`,
  Wine 11.15 amd64-wow64, 15 result lines, 0 failures. Log:
  `C:\Temp\osdk-lw\logs-win\wine-tests.log`. The log does not record a commit;
  attribution is from the pre-run checkout.
  `2fb869d..5495518` contains exactly: `931cf78` skills-reference doc,
  `337ec43` Unix-only fixture path fix, `5495518` product-code if-collapse.
  The diff has been read as semantically equivalent, but the Wine suite has
  **not** been re-run on top of `5495518`.
- Windows smoke: ran at an earlier feature revision (around `546979e`/`2dbc570`;
  reviewer later cites the `6e109ea` era). Exact commit and standalone log not
  retained; not re-run at `2fb869d` or `5495518`.
- Real npm: `osdk use npm:which@7.0.0` from an external binding exits 0, pins
  the tool in the bundle, leaves no `osdk.toml` in the project, no
  `node_modules`, and byte-identical `package.json` / native lock. Commit
  `2fb869d`; fixture script
  `<agent-logs>\fixture-use2.ps1`; successful run output not independently
  saved.
- VitePress production build: `osdk run site` green (33.73 s, bilingual new
  pages rendered).

## Gates run on this machine

- `cargo fmt --check` at `5495518`: exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings` at `5495518`: exit 0.

## Metrics

- Release binary size (bytes) at `2fb869d`: CLI 14,766,080; shim 3,982,848.
  Build commit for these exact bytes not separately traced.
- Clean-startup numbers 28.6 → 27.4 ms (hook-env) and 15.6 → 15.3 ms
  (`shim --version`) were measured on rebuilt `E:\final` binaries at
  `2fb869d`; the measured commit of the final binaries is not recorded. These
  measure bare process launch, not real forwarding.
- Benchmark: `cargo bench -p osdk-core --bench interactive_latency -- --nocapture`
  at base `249306fa` vs final `2fb869d`. Raw logs:
  `C:\Users\LJY\AppData\Local\Temp\bench-base.txt`,
  `C:\Users\LJY\AppData\Local\Temp\bench-final.txt`. Inventory scan 5.73 → 6.09 ms
  / 5.85 → 6.42 ms (single samples, cause unproven); deterministic 8-decoy
  prune assertions and activation_render (~0.4 µs) unchanged. The benchmark
  does not measure project discovery, so the external hash lookup cannot be
  attributed.

## Known baseline failures (Windows native; identical at `249306fa` and `2fb869d`)

- `osdk-core` package_registry: 5 failures.
  Final log: `C:\Users\LJY\AppData\Local\DoubaoWork\User Data\Default\.doubaowork\agent_mode\workspace\.sessions\38449122744973826\agents\s_000pVP0Ycc3\logs\final-core-reg.log`.
  Base log: `...\base-core-reg.log`.
- `osdk-core` package_sources: 3 failures.
  Final log: `...\logs\final-core-src.log`. Base log: `...\base-core-src.log`.
- `isolated_cli::github_action_windows_helpers_complete_the_ci_contract`: 1 fail.
  Final log: `...\logs\final-gha.log`. Base log: `...\base-gha.log`.

Root cause not yet localised. These reproduce identically at the base commit;
this work has not verified that every unrelated issue is absent.

Note: logs under `C:\Temp\` are in the system temp directory and are not
archived; they are not guaranteed to persist. Checks with no saved raw output
are described as observation-time notes, not log-backed evidence.
