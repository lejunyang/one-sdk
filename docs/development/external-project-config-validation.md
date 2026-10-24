# External Project Config — Validation Record

Scope: `osdk project add/show/list/relocate/remove`, binding stores under
`<OSDK_CONFIG_DIR>/projects/<root-id>/`, and unified `ProjectContext` discovery
across CLI, shim, trust, lock, and inheritance.

- Base commit: `249306fa`
- Validated head: `a5708c2`

## Key behaviours covered by automated tests

- Discovery priority: the nearest ancestor wins. A same-root local `osdk.toml`
  shadows the external bundle; a nearer external child beats a far local parent.
- Binding integrity: `project.toml` manifest (schema 1), `osdk.toml`, and
  `osdk.lock` (schema 3) are created atomically by `add`.
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
- Junction binding resolves through to the canonical real root.

## Environment evidence

- Linux (native cargo 1.98) at `5495518`: full workspace, every target zero
  failures.
- Wine at `2fb869d`: full suite, zero failures.
- Windows smoke: `scripts/windows-runtime-smoke.ps1` green.
- Real npm: `osdk use npm:which@7.0.0` from an external binding exits 0, pins
  the tool in the bundle, leaves no `osdk.toml` in the project, no
  `node_modules`, and byte-identical `package.json` / native lock.
- VitePress production build: `osdk run site` green (33.73 s, bilingual new
  pages rendered).

## Metrics

- Release binary size (bytes): CLI 14,718,976 → 14,766,080; shim 3,968,512 →
  3,982,848.
- Empty-state startup: hook-env 28.6 → 27.4 ms; `shim --version` 15.6 → 15.3 ms.
  These measure bare process launch, not real forwarding.
- Inventory scan benchmark 5.73 → 6.09 ms / 5.85 → 6.42 ms (single samples,
  cause unproven); deterministic 8-dynamic-decoy prune assertions and
  activation_render (~0.4 µs) unchanged. The benchmark does not measure
  project discovery, so the external hash lookup cannot be attributed.

## Known baseline failures (pre-existing, Windows native only; Linux/Wine green)

- `osdk-core` package_registry: 5 failures (22 pass).
- `osdk-core` package_sources: 3 failures (20 pass).
- `isolated_cli::github_action_windows_helpers_complete_the_ci_contract`: 1 fail.

Root cause not yet localised; not regressed by this work.
