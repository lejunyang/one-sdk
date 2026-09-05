# Release pipeline

The repository's [`.github/workflows/publish.yml`](https://github.com/lejunyang/one-sdk/blob/main/.github/workflows/publish.yml)
runs only when the head commit pushed to `main` explicitly contains the release
marker. Ordinary pushes do not publish. Update `[workspace.package].version`
before releasing; the prepare job refuses to continue if the corresponding
`v<version>` tag already exists.

## What one release publishes

The workflow first builds both programs for Linux x64/arm64, macOS
Intel/Apple Silicon, and Windows x64 in parallel and uploads temporary
artifacts. Once every build succeeds, it publishes crates.io packages in this
dependency order:

1. `osdk-core`;
2. wait until that version is visible through the crates.io API;
3. `osdk-cli`;
4. `osdk-shim`.

`osdk-cli` and `osdk-shim` both depend on an exact `osdk-core` version, so the
registry visibility wait is required. Core passes its dry-run before upload;
both dependents pass `cargo publish --locked --dry-run` together once core is
visible. The publish job removes the repository build-mirror configuration and
validates publishable dependencies against the official crates.io index and
the repository lockfile. The workflow
creates the GitHub tag and Release only after every crate has published, then
attaches five platform archives and `SHA256SUMS`. A crates.io failure therefore
cannot leave behind a GitHub Release that appears complete.

Install the primary commands from crates.io with:

```bash
cargo install osdk-cli --locked
```

This installs `osdk`. `osdk-shim` is a separate package. For a complete everyday
installation, the GitHub Release installer remains preferred because it places
both same-version programs in one directory.

## Binary size

What users download is these two executables, so the release profile is tuned
for size rather than for raw speed. Current sizes are roughly 8.9 MB for `osdk`
and 3.5 MB for `osdk-shim`.

The settings that get there, measured on this workspace:

| Profile | Combined size |
| --- | --- |
| `opt-level = 3`, `lto = "thin"` | 44.6 MB |
| `opt-level = 3`, `lto = "fat"`, `codegen-units = 1` | 37.0 MB |
| `opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"` | 16.7 MB |

Optimizing for size is safe for the shim, which is the latency-sensitive binary
because it runs on every `node` or `npm` invocation. Measured shim startup is
48.3 ms median at `opt-level = "z"` against 50.5 ms at `opt-level = 3`: process
creation dominates, so shrinking the code costs nothing observable here.

One place does pay, and it is not the obvious one. `opt-level = "z"` costs
sha2's portable backend about 65% of its throughput, dropping from 2300 MiB/s to
800 MiB/s when hashing 256 MiB. Every downloaded archive is checksummed, so left
alone this would slow down every install. The workspace manifest therefore pins
the hashing crates back to `opt-level = 3` with per-package profile overrides,
which restores full throughput for about 0.02 MB of size. BLAKE3 measured
unaffected because it ships hand-written SIMD, but it is pinned as well since it
hashes every file entering the content-addressed store.

Cargo only emits a warning, not an error, when a per-package override matches no
package. A dependency rename would silently give the slowdown back with a green
build, so `hashing_crates_are_pinned_to_a_fast_opt_level` in
`crates/osdk-core/src/pipeline/verify.rs` asserts the pins are present.

`panic = "abort"` applies to the shipped binaries only. Cargo ignores the
setting for test targets, so `catch_unwind`-based tests still work under
`cargo test --release`.

## Why the shim is much smaller than the CLI

Profile tuning is only half the story. The shim used to be nearly as large as the
CLI for a structural reason: it only ever reads state, but it holds
`Arc<dyn Backend>` values and `Registry::new` instantiates all thirteen backends,
so every `Backend` method landed in a vtable the linker could not prove
unreachable. That kept the whole install path alive inside the shim, including the
sigstore subtree that accounts for 240 of `osdk-core`'s 314 dependency crates.

The cost was measured by building probes that link `osdk-core` and exercise only
the read-only surface:

| What the probe links | Size |
| --- | --- |
| directory resolution only | 0.13 MB |
| plus `Config::load` | 0.74 MB |
| plus `http::client` | 1.83 MB |
| plus `Registry` and `dyn Backend` | 7.02 MB |

That last step looks conclusive but conflates two things: the vtables keeping the
install path alive, and the backends' own read-only code, which the shim needs
anyway. Calling the same thirteen backends through static dispatch, where the
linker can discard the unused `install` bodies, costs 1.83 MB. So the install path
alone accounted for about 5.15 MB.

The four install-only trait methods -- `list_remote_versions`, `resolve_version`,
`install` and `uninstall` -- are therefore behind a default-on `install` feature,
and the shim depends on `osdk-core` with `default-features = false`. The sigstore
crates are optional and pulled in by that feature, so the shim's dependency graph
drops from 982 crates to 441 and no longer contains a second copy of `reqwest`.

Compiling the install path out substitutes uninhabited stand-ins for
`GithubAttestation` and `VerificationEvidence`. Every verification call sits inside
`if let Some(attestation) = attestation`, and an `Option` of an uninhabited type is
always `None`, so those branches are statically unreachable while signatures,
struct fields and callers stay unchanged. The alternative -- `#[cfg]` on
thirty-odd references including public fields -- would be much harder to follow.

### The shim must be built in its own invocation

Cargo unifies features across a single `cargo build --workspace`, so building both
binaries in one command re-enables `install` for the shim and silently restores
the old size. The result still works, and nothing warns.

Release builds therefore run one invocation per binary:

```bash
cargo build --release -p osdk-cli
cargo build --release -p osdk-shim
```

Both may share one target directory; Cargo caches the two feature variants side by
side and does not rebuild when alternating between them. A compile-time assertion
in the shim fails the build, with an explanation, if the install path is ever
linked back in. It is limited to release builds so `cargo check`, `cargo test` and
`cargo clippy` still work across the whole workspace during development.

## First-release authentication

At the time of writing, none of the three crates exists on crates.io. crates.io
Trusted Publishing requires an existing crate, so the first release needs a
GitHub Environment named `crates-io` with a `CARGO_REGISTRY_TOKEN` secret. The
token needs the `publish-new` and `publish-update` scopes. Never commit it or expose it in
logs or ordinary configuration. Consider adding a required reviewer to the
Environment so the irreversible bootstrap publish has a human approval gate.

After the first release, add the same GitHub Actions Trusted Publisher in the
crates.io Settings page for each crate:

| Field | Value |
| --- | --- |
| Repository owner | `lejunyang` |
| Repository name | `one-sdk` |
| Workflow filename | `publish.yml` |
| Environment | `crates-io` |

After all three publishers are configured and the next release succeeds,
delete the long-lived `CARGO_REGISTRY_TOKEN` from the GitHub Environment.
Subsequent runs use `rust-lang/crates-io-auth-action` to exchange GitHub OIDC
identity for a job-scoped token that the action revokes when the job ends. The
workflow retains the first-release token as a bootstrap fallback while OIDC is
not configured; removing the secret leaves Trusted Publishing as the only
path.

## Release checklist

1. Update both the workspace version and the exact `osdk-core` version under `[workspace.dependencies]`, plus the relevant user-facing release notes.
2. Ensure CI, the Windows Wine workspace tests, and the docs build pass.
3. Ensure the `crates-io` Environment reviewer and credential are ready.
4. Push a `main` head commit that explicitly contains the release marker.
5. Verify that `osdk-core`, `osdk-cli`, `osdk-shim`, and the GitHub Release all carry the same version.

Crate versions cannot be overwritten. If a run publishes only part of the
workspace, fix the cause, bump the workspace version, and release again rather
than trying to replace an uploaded version.
