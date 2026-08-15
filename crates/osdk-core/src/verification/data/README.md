# Embedded Sigstore Trust Root

`sigstore-public-good-trusted-root.json` is the Sigstore public-good trusted
root distributed by the audited `sigstore` Rust crate version `0.14.0` at:

`trust_root/prod/trusted_root.json`

SHA-256:

`f44a1b88128e55ebfb62189becbc0fa48d4ec9915c65ac54ba0e46a008b12d5b`

The verifier parses this checked-in root without invoking the crate's network
TUF updater. Updating the dependency or trust root requires reviewing the new
material and updating this digest.
