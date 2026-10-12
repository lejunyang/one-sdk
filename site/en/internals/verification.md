# Artifact verification and security boundaries

osdk treats “these bytes match,” “this metadata was signed by a known key,” and “this artifact was produced by a named GitHub repository” as three different claims. They complement but do not replace one another. The main entry points are [`pipeline`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) and [`verification`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/verification/mod.rs).

## Defaults

The built-in values are defined by [`Settings::default`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs):

| Capability | Default | Meaning |
| --- | --- | --- |
| `verify_signatures` | `true` | Use a backend's trusted signature path when one exists |
| `require_checksums` | `false` | Installation may continue when no checksum is available |
| `attestations` | **`off`** | GitHub artifact attestations are not queried, downloaded, or verified by default |

`OSDK_VERIFY_SIGNATURES`, `OSDK_REQUIRE_CHECKSUMS`, and `OSDK_ATTESTATIONS`, or their config equivalents, override these defaults. CLI `--require-checksums` and `--attestations` are applied after loaded configuration. See [`config/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/config/mod.rs) and [`app.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/app.rs).

## Checksums

[`pipeline::verify`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs) supports SHA-256, SHA-512, and BLAKE3. Files are read in 64 KiB chunks and compared as case-insensitive hexadecimal. npm SRI accepts `sha512-...` and `sha256-...`, preferring SHA-512 when several values are present. The generic GitHub backend discovers a digest in this order:

1. an online trusted minisign checksum manifest, when signature verification is enabled and one is available;
2. otherwise, a digest explicitly supplied by a static catalog;
3. otherwise, `<asset>.sha256`, `<asset>.sha256sum`, or `<asset>.sha256.txt`;
4. otherwise, `SHASUMS256.txt`, `SHA256SUMS`, `sha256sums.txt`, or `checksums.txt`.

After download and before extraction or execution, the pipeline recomputes the digest. A successful digest is stored in a sibling `.checksum` file so an offline reinstall can verify the artifact again. The URL and filename in `osdk.lock` reconstruct the locked install plan. When the lock contains a checksum, reinstall recomputes and compares it against the current artifact bytes. Historical evidence remains an audit record only. See [`github.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/github.rs), [`pipeline/mod.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs), and [`lockfile.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/src/lockfile.rs).

Security boundary: ordinary sidecars and shared checksum files do not authenticate an identity. They detect corruption or disagreement with that digest, but do not prove who published it. `require_checksums=true` requires a verifiable digest; a SHA-256 authenticated by a successful attestation also satisfies that gate. Because `require_checksums` defaults to false, an artifact with neither a checksum nor attestation can be installed.

## Minisign signatures

Signature verification authenticates a checksum manifest rather than defining a separate artifact-signature protocol. osdk verifies the minisign manifest with a public key compiled into the binary, reads the target artifact's SHA-256 only after that succeeds, then verifies the artifact in the normal pipeline. [`trusted_key`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs) is a closed table keyed by the exact `github:owner/repo`; it currently contains only explicitly supported distributions.

`verify_signatures=true` does not require every download to be signed. If no built-in key exists, or the manifest or signature cannot be fetched, checksum discovery can continue through other paths. Once both are obtained, however, an invalid signature is a hard error and is not downgraded to trusting the manifest. Offline mode does not fetch signature metadata and relies on a previously persisted digest that is recomputed against the cached artifact.

## GitHub artifact attestations

Attestations are wired only into the `github:owner/repo` backend. The policies are:

- `off`: the **default**; skip attestation entirely.
- `if-available`: an empty API result, or no cached bundle while offline, is allowed; a bundle that is present but invalid fails. API, proxy, and bundle-download failures also propagate, so this does not mean “ignore every network error.”
- `required`: a missing artifact, missing online or cached bundle, or any verification error blocks installation.

The verifier computes the artifact's SHA-256 and locates bundles by `owner/repo + digest`. Inline JSON returned by GitHub is used directly. For `bundle_url` responses, the initial URL and final response URL must use HTTPS, and compressed input and Snappy-decompressed output are each limited to 8 MiB. Inline bundles carried by the GitHub API response and locally cached bundle files currently have no equivalent read-size bound. This path does not enforce same-origin redirects or inspect every intermediate URL. The bundle cache is published via a same-directory temporary file and rename.

For a bundle with a Rekor entry, verification covers the embedded Sigstore public-good trust root, Fulcio certificate chain and SCT, GitHub Actions OIDC issuer, repository identity in the signing certificate, artifact signature and DSSE digest, Rekor canonical-body consistency, Signed Entry Timestamp, signed checkpoint, Merkle inclusion proof, and signing time. Exactly one transparency-log entry is required.

A GitHub TSA bundle without a Rekor entry follows a separate path: osdk checks the repository claim in the signed DSSE statement, then verifies the artifact digest and bundle with the embedded GitHub trust root. That path explicitly skips tlog and SCT checks because it uses the TSA structure rather than a Rekor proof; it must not be described as Rekor verification. Successful evidence is stored in `.osdk-artifact.json` and may be copied into the lockfile.

Further trust boundaries: GitHub credentials are sent only to `api.github.com`, never to a third-party proxy. A proxy is a transport, not a trust anchor; final trust comes from the digest, signature, or attestation. Embedded trust roots change only with an osdk release and are not refreshed online.

## Archive and path safety

[`extract.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/extract.rs) supports `tar.gz`, `tar.xz`, `tar.zst`, and ZIP. It expands into a cache scratch directory before optionally stripping a sole top-level directory. ZIP uses `enclosed_name()` and skips entries that cannot be safely enclosed by the destination; tar uses `tar::Archive::unpack(dest)` and its destination containment checks. A selected archive `subdir` must be relative and contain only normal components, rejecting absolute paths, `.`, `..`, and prefixes. Model file paths apply the same lexical rule in [`safe_relative_path`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/model/mod.rs).

Important limits: there is no extracted-size, file-count, or compression-ratio limit, so this does not defend against archive bombs. Tar links are handled by the archive library, and the CAS later recreates extracted symbolic-link targets verbatim; osdk does not explicitly reject absolute or out-of-tree link targets. Unsafe ZIP names are skipped rather than rejecting the whole archive. Artifacts from a publisher that is not fully trusted should be installed in an additional sandbox. An authenticated digest or attestation proves bytes/provenance; it does not make a malicious archive structure safe.

## Key tests

- [`pipeline/verify.rs` unit tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/verify.rs) cover digest vectors, manifest parsing, SRI, and successful and failed minisign checks.
- [`verification/mod.rs` tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/verification/mod.rs) use real fixtures for offline Sigstore verification, wrong repositories, tampered artifacts, missing or changed Rekor proofs, checkpoints and SETs, TSA bundles, and attestation-derived digest evidence.
- [`pipeline/mod.rs` tests](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/pipeline/mod.rs) cover offline cache re-verification, the strict checksum gate, and failure without a complete marker.
- [`isolated_cli.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-cli/tests/isolated_cli.rs) covers CLI policy overrides, rejection of a tampered locked checksum, and proof that `required` does not trust lockfile evidence without re-verifying a cached bundle.
