# Direct HTTPS Artifacts

Use the built-in `http:` backend when a tool is published as one predictable
HTTPS file or archive, but has no dedicated osdk backend. The declaration keeps
the URL template, exact version, SHA-256 digest, and executable layout together.
Despite the namespace name, plain HTTP is not accepted.

## Required contract

Every request has this shape, with the option block before the version selector:

```text
http:https://host/path-{version}[sha256=64_HEX_CHARACTERS,...]@1.2.3
```

The following parts are mandatory:

- an absolute `https://` URL template containing `{version}` in its path;
- an exact semantic version such as `1.2.3`;
- `sha256`, containing exactly 64 hexadecimal characters for that artifact.

`latest`, ranges, partial versions such as `1.2`, and other template fields such
as `{os}` or `{arch}` are rejected. The digest belongs to the exact rendered
artifact, so update it whenever the version or artifact bytes change. Quote the
complete request in a shell so brackets and other punctuation are passed
literally.

## Install a bare executable

Use `kind=file` for one executable. `rename` is optional; without it, the last
URL path segment becomes the command name. Replace the example digest with the
publisher's digest for the exact file before running the command.

```bash
osdk install \
  'http:https://downloads.example.com/acme-{version}[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=file,rename=acme]@1.2.3'
```

The file is installed in the identity's `bin/` directory and made executable on
Unix. On Windows, `.cmd` and `.bat` names are rejected; `.exe` is retained and
other final names receive an `.exe` suffix. This is a filename policy, not PE
content inspection. File artifacts do not accept `bin`, `bins`, `subdir`, or
`strip-components`.

## Install an archive

The archive kinds are `tar.gz`, `tar.xz`, and `zip`. Every archive request must
declare at least one executable with `bin` or `bins`. The following example
expects a single top-level `acme-1.2.3/` directory containing `bin/acme`:

```bash
osdk install \
  'http:https://downloads.example.com/acme-{version}.tar.gz[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=tar.gz,strip-components=1,bin=bin/acme,rename=acme]@1.2.3'
```

`kind` can be inferred from a `.tar.gz`, `.tar.xz`, or `.zip` filename for a
simple archive install. Specify it explicitly when using archive-only layout
options so validation does not treat an otherwise unknown artifact as a file.

| Option | Meaning |
| --- | --- |
| `sha256=HEX` | Required SHA-256 of the downloaded bytes; input is normalized to lowercase |
| `kind=tar.gz\|tar.xz\|zip\|file` | Artifact type; a recognized archive suffix is inferred when omitted, otherwise the artifact is a file |
| `bin=PATH` | Copy one safe relative executable path from an archive into the installed `bin/` directory |
| `bins=P1,P2` | Copy multiple archive executables; mutually exclusive with `bin` |
| `subdir=PATH` | Materialize only this safe relative directory after extraction |
| `strip-components=N` | Descend through `N` single-directory wrapper levels before resolving `bin`/`bins` |
| `rename=NAME` | Rename a file artifact or the one selected archive binary |

`strip-components` is intentionally narrower than the similarly named tar
option. At each level, the current directory must contain exactly one
non-osdk child and that child must be a directory. It does not independently
remove path segments from every archive member. If `subdir` is also set, that
subtree is materialized first and the unique-directory descent happens inside
it.

Archive requests without `bin` or `bins` are rejected before installation. This
makes the executable inventory explicit: each selected regular file is copied to
the installation's top-level `bin/` directory. On Unix the copied output is made
executable; on Windows each selected output must be `.exe`-named after the output
name rule is applied. `rename` on an archive requires exactly one selected
binary.

## Declare the tool in `osdk.toml`

Quote the complete dynamic backend ID because it contains punctuation. The
structured form keeps the exact selector and artifact options readable:

```toml
[tools."http:https://downloads.example.com/acme-{version}.tar.gz"]
version = "1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
kind = "tar.gz"
strip-components = "1"
bin = "bin/acme"
rename = "acme"
```

For several commands, use `bins` as a TOML string array and omit `rename`:

```toml
bins = ["bin/acme", "bin/acmectl"]
```

All public HTTP options participate in the installation identity. Changing the
digest, artifact kind, selected subtree, selected binaries, or rename produces
a different isolated installation instead of silently reusing an incompatible
one.

## Lock and replay offline

A practical project workflow is:

```bash
# Download and verify the exact artifact declared in osdk.toml.
osdk install

# Record the exact version, public options, and installed artifact receipt.
osdk lock

# Later, consume the current-platform lock without network access.
osdk --offline install
```

After a successful installation, the generated lock can preserve the actual
artifact URL, filename, and `sha256:` checksum. Lock replay gives those fields
precedence over rendering the template again. Offline installation never falls
back to the network: it reuses only the cache entry for that exact installation
identity and verifies SHA-256 again. If those bytes are absent, it fails with an
offline artifact cache miss.

An existing cache entry is verified before use. Offline mode fails immediately
on a checksum mismatch and performs no DNS lookup. Online mode deletes one
regular, within-limit cache entry whose checksum is wrong, downloads the
artifact once, and verifies the replacement once; it has no further HTTP retry
loop. Oversized, symlinked, or non-regular cache entries fail closed instead of
being repaired automatically.

The lock does not embed the artifact bytes. Keep the osdk download cache warm
when a machine must rebuild the installation offline; `osdk cache clean` removes
that cache. An already complete, identity-matching installation can still be
reused without downloading it again. See [Reproducible Lockfiles](./lockfiles)
for the general rule that a no-argument `install` consumes the current-platform
lock.

## Security boundary

The backend requires a canonical URL spelling and rejects URL credentials, query
strings, fragments, non-HTTPS URLs, literal non-public IP addresses, and
cross-origin or HTTPS-to-HTTP redirects. Before an online request, it resolves
the host for at most 10 seconds, rejects the complete result if any address is
outside its conservative public-address policy, and pins the client to the
accepted addresses. System and environment proxies are disabled. Same-host,
same-effective-port redirects therefore continue to use that pinned destination
set rather than resolving a new host.

The connect timeout is 15 seconds and the HTTP request timeout is 10 minutes.
The latter covers the request and body transfer, not DNS or local checksum,
extraction, copying, and publication work. A downloaded or cached artifact may
be at most 512 MiB; both `Content-Length` and the actual streamed byte count are
bounded. Partial downloads are not published as cache entries.

It always verifies the required SHA-256, including cached bytes. An archive may
contain at most 16,384 entries and at most 2 GiB in cumulative declared
expanded/uncompressed sizes. The 2 GiB limit is based on entry metadata, not a
measurement of post-extraction disk use, and directories count toward the entry
limit. Archive paths must stay inside the extraction root; tar links and
non-file/non-directory entries are rejected, ZIP symlinks are rejected, and the
materialized install is checked again for symlinks before publication. Configured
executable paths must resolve to regular files inside the install root. If final
discovery finds no executable, publication fails and the incomplete install root
is removed.

These checks do not establish who published the digest. Obtain SHA-256 through
an authenticated channel you trust and review project configuration before
trusting it. The backend does not currently support signatures or GitHub
Artifact Attestations.

## Current limits

- There is no remote version list or automatic update discovery; use one exact semantic version.
- Only `{version}` interpolation is available, so one declaration cannot select different artifacts by OS, architecture, or libc.
- Authenticated URLs, signed query URLs, custom headers, and cross-origin CDN redirects are not supported.
- Environment and system HTTP proxies are deliberately ignored; the destination must resolve directly to public addresses.
- There is one URL and no mirror/source failover for a declaration.
- Only bare files, `tar.gz`, `tar.xz`, and ZIP archives are supported; `tar.zst`, installers, and disk images are not.
- SHA-256 is the only integrity algorithm for this backend and cannot be omitted.
- Every installation uses osdk's isolated scope.
- A tool id is expanded one directory per segment, but only for its first five segments; a longer id keeps the leading four and folds the rest into a `~t1~` digest. This bounds the install tree for ids derived from a URL, whose segment count is chosen by the remote server, and keeps each receipt reachable by the inventory scanner.

For the parser, identity, redirect, cache, extraction, and publication details,
read [HTTP artifact backend internals](./implementation/http-artifacts).
