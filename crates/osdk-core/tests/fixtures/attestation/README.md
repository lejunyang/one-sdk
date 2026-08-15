# Attestation Fixtures

The fixture pair represents one real, public Sigstore verification chain:

- `kubewarden-manifest.json.b64` is the OCI index fetched by digest from
  `ghcr.io/kubewarden/kubewarden-controller@sha256:c811d58de79c92f03214e63aa339484e488d694ae8a6283b5f3f17a9faf50172`.
- `kubewarden.sigstore.json.gz.b64` is `tests/data/bundle_v03.json` from the
  `sigstore` Rust crate version `0.14.0`, gzip-compressed and base64-encoded for
  a text-only fixture.

Decoded SHA-256 values:

- OCI index:
  `c811d58de79c92f03214e63aa339484e488d694ae8a6283b5f3f17a9faf50172`
- Sigstore bundle:
  `18cd3d06ef643c802ea60a392311a65649fa5e0598ed9bc5129681f67f84b83b`

Tests decode both files into temporary directories and never contact the
registry or GitHub.
