# HTTP artifact backend

The built-in `http:` dynamic namespace is a deliberately small direct-artifact
backend. Its contract is “one exact semantic version, one HTTPS URL template,
one mandatory SHA-256, and an optional safe layout projection.” It does not try
to be a release service, package registry, or general authenticated downloader.
User syntax is documented in [Direct HTTPS Artifacts](../http-artifacts).

## Registration, parsing, and resolution

[`dynamic.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs)
registers `HttpBackendFactory` beside the other built-in dynamic namespaces. The
full URL template is the namespace subject, so the canonical backend ID itself
looks like `http:https://host/tool-{version}.zip`.

[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)
owns the namespace schema. It accepts only the public options `sha256`, `kind`,
`bin`/`bins`, `subdir`, `rename`, and `strip-components`; user requests cannot
inject internal `__osdk_*` replay fields. Every public option participates in
dynamic installation identity. `bin` canonicalizes to `bins`, lists are sorted
and deduplicated, kinds and digests are lowercased, filesystem paths are
normalized to safe relative slash-separated paths, and Windows-reserved names
are rejected.

The template validation is fail-closed:

- input is at most 4096 characters and has no surrounding/embedded whitespace, control characters, backslashes, or `@`;
- it parses as an absolute HTTPS URL with a host and path, without userinfo, query, or fragment;
- its parsed URL must retain exactly the canonical spelling, and a literal IP must pass the implementation's conservative public-address policy;
- one to eight placeholders are permitted, every placeholder is exactly `{version}`, and placeholders occur only in the path;
- the rendered selector is an exact semantic version, at most 128 characters, and contains only ASCII alphanumerics plus `.`, `-`, `_`, and `+`.

[`HttpBackend::resolve_version`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
therefore needs no metadata request: it accepts the exact selector and validated
options directly. `list_remote_versions` always returns an error. There are no
default sources or probe URL, so source ranking, `--source`, and source failover
are outside this backend.

## Artifact selection and identity

Without replay metadata, the backend substitutes the exact version into every
`{version}`, validates the rendered URL again, takes a safe filename from its
last path segment, and requires the configured 64-hex SHA-256. `kind` recognizes
`tar.gz`, `tar.xz`, `zip`, and `file`; if omitted, the three archive suffixes are
recognized and any other safe filename becomes a bare file. `tar.zst` is
explicitly rejected. An archive option set is invalid unless canonical `bins`
contains at least one entry (`bin` canonicalizes to that same field).

An [`InstallIdentity`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs)
contains the canonical backend ID, exact version, host platform, isolated scope,
all canonical public options, plus the selected artifact filename,
`sha256:<hex>`, and a domain-separated BLAKE3 hash of the actual artifact URL as
materials. The resulting `b3-v2:` install ID gives each URL, layout, and digest
variant a separate root. Internal replay fields stay outside public options, but
the locked URL is still bound through that material hash; the full actual URL is
retained in the receipt and lock.

## Network and cache boundary

[`http.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
constructs a dedicated client only after resolving the artifact host. DNS runs
in a blocking task with a 10-second timeout. Literal IPs and every returned DNS
address pass the same conservative public-address predicate; one forbidden
address rejects the complete result. Accepted addresses are sorted, deduplicated,
and pinned with `resolve_to_addrs`, and `.no_proxy()` disables environment and
system proxies. This closes proxy and DNS-rebinding paths to local, link-local,
metadata-service, documentation, transition, and other rejected address ranges.

The client has a 15-second connect timeout, a 10-minute HTTP request timeout,
and a 30-second idle-pool timeout. The request timeout is not an end-to-end
installation deadline: DNS has its separate bound, while local verification,
archive scanning, extraction, copying, and publication are outside it. Redirects
must remain HTTPS and on the original host and effective port; credentials,
query strings, fragments, loops, and origin changes are rejected. A redirect is
also rejected once ten prior URLs exist. Because the host cannot change, the
pinned address set remains authoritative. The backend has no option for request
headers or credentials.

The download cache path includes the complete install identity and filename, so
different URLs, checksums, or layouts cannot share unverified bytes merely
because the URL filename matches. A cache entry must be a regular, non-symlink
file no larger than 512 MiB. `prepare_cached_artifact` always recomputes SHA-256.
A valid offline hit performs no DNS lookup; an offline miss or mismatch fails.
For one existing regular, within-limit online cache entry with the wrong digest,
the backend deletes it, performs exactly one bounded refetch, and verifies the
replacement once. It does not loop or refetch again after another mismatch;
oversized, symlinked, and non-regular entries fail instead of being evicted.

`download_bounded` rejects `Content-Length` above 512 MiB and independently
counts streamed bytes with overflow checking. It writes to a process-specific
partial file, calls `sync_all`, and renames only on success; failure removes the
partial file.

After preparation, both branches invoke shared pipeline helpers in forced
offline mode. This is an internal single-fetch guarantee: the verified cache
entry is the only input to materialization, and the generic helper cannot make a
second request. It does not mean that an online HTTP installation skips its
initial download.

## File and archive materialization

For `kind=file`, the prepared bytes are copied to `bin/<name>`, where `<name>` is
the URL filename or `rename`. Unix permissions are made executable. On Windows,
`.cmd` and `.bat` are rejected case-insensitively, an existing `.exe` is retained,
and other output names receive `.exe`. This is name enforcement, not validation
of PE bytes. File mode rejects archive-only layout options.

Archive mode first pre-scans the verified bytes:

- tar entries must use safe relative paths and be only regular files or directories; links and special entries are rejected;
- ZIP entries must have enclosed relative paths and must not be Unix symlinks.
- both formats allow at most 16,384 entries and 2 GiB in cumulative declared expanded/uncompressed entry sizes; directories count toward the entry limit, and the expanded-size bound is not measured post-extraction disk usage.

The shared extraction pipeline then materializes `tar.gz`, `tar.xz`, or ZIP with
copy link mode. `subdir` optionally chooses a safe extracted subtree. The HTTP
postprocessor rejects any remaining symlink, descends `strip-components` levels
only through a unique non-`.osdk-*` directory at each level. Because archive
validation requires `bin` or `bins`, the postprocessor always copies an explicit
set of selected regular files into the installation's top-level `bin/`; there is
no implicit archive command discovery. A configured source is rejected if
canonicalization leaves the install root.
`rename` requires exactly one archive binary. Declared Windows archive binaries
follow the same `.exe` output-name rule and reject `.cmd`/`.bat`, guaranteeing an
explicit `.exe`-named selected output inventory.

Finalization verifies the resulting top-level command inventory. Unix candidates
need an executable bit; Windows candidates must have a corresponding `.exe` name.
An empty inventory is rejected as a defense-in-depth check. Only then does
finalization atomically write `.osdk-install.json` and publish `.osdk-complete`
last; any finalization failure removes the incomplete root.

## Lock replay, reuse, and concurrency

The generic artifact receipt records the actual URL, filename, SHA-256, and any
verification evidence. The CLI lock stores canonical public options separately
from that receipt. On replay, internal fields restore the receipt; the HTTP
backend prefers its URL, filename, and checksum to template rendering and
requires the locked checksum to be canonical lowercase SHA-256. The lock is
metadata, not an artifact bundle, so a cold offline reinstall still needs the
identity-specific download cache.

Every install takes the identity-specific file lock. A complete existing root is
reused only after validating the install ID/root relation, regular manifest,
regular receipt, completion marker, exact option and material identity, receipt
filename/checksum, recorded command paths, and absence of symlinks. An invalid
completed root fails closed instead of being reused or overwritten. Inventory-
based lookup also rejects ambiguous matches. These common dynamic-install rules
live in [`backend/dynamic.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/dynamic.rs).

## Deliberate omissions

The minimal backend currently has no remote-version extraction, ranges, aliases,
platform placeholders, authenticated requests, custom headers, signed query
URLs, proxies, private/internal destinations, cross-origin redirects, mirrors,
size declarations, checksum discovery,
signatures, attestations, or non-SHA-256 digest algorithms. It supports only one
artifact URL per declaration and only bare files, `tar.gz`, `tar.xz`, and ZIP.
All installs are isolated.

The focused tests in
[`backend/http.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/backend/http.rs)
cover strict templates, public-address rejection, redirect rejection, archive
traversal, links and resource caps, locked offline file and archive replay,
checksum failure without publication, Windows output-name policy, and
same-identity concurrent serialization. Namespace parser and option interaction tests live in
[`tool.rs`](https://github.com/lejunyang/one-sdk/blob/main/crates/osdk-core/src/tool.rs).
