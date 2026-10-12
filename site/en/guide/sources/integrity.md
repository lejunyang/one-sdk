# Integrity, Signatures, and Attestation

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

## TLS certificate verification

TLS certificates are verified against the operating system trust store: the
Windows certificate store, Keychain on macOS, and the usual OpenSSL locations on
Linux. osdk does not carry its own copy of the root certificates.

The practical consequence is that certificate trust follows the machine. A
corporate CA installed system-wide, or a revoked root removed by an OS update, is
picked up without waiting for an osdk release, which is what makes osdk usable
behind a TLS-inspecting proxy. In exchange osdk depends on the host being
provisioned: a minimal container image with no `ca-certificates` package will
fail every HTTPS download until root certificates are installed.

Verification cannot be turned off. Downloads from an untrusted, expired,
self-signed, or wrong-host certificate fail before any bytes are written, and the
error names the specific reason.
