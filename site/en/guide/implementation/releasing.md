# Release pipeline

The repository's [`.github/workflows/publish.yml`](https://github.com/lejunyang/one-sdk/blob/main/.github/workflows/publish.yml)
runs only when the head commit pushed to `main` explicitly contains the release
marker. Ordinary pushes do not publish. Update `[workspace.package].version`
before releasing; the prepare job refuses to continue if the corresponding
`v<version>` tag already exists.

## What one release publishes

The workflow first builds the three programs for Linux x64/arm64, macOS
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
