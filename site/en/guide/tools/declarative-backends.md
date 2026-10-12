# Declarative Backends

At startup, osdk automatically loads direct `*.toml` children from these
directories; there is no separate installation command:

```text
<config>/plugins/*.toml
<data>/plugins/*.toml
```

The config directory loads first, followed by the data directory. Any invalid
definition or ID/alias collision with a built-in or another backend aborts
registry loading; definitions cannot shadow each other. Loading is non-recursive,
regular files only, with a 1 MiB limit per file.

## Schema 1 example

```toml
schema = 1
id = "acme"
bin_paths = ["bin"]
bin_names = ["acme", "acmectl"]
idiomatic_files = [".acme-version"]

[versions]
values = ["1.0.0", "1.2.3"]
# Or: url = "https://example.test/{os}/{arch}/versions.txt"

[archive]
url = "https://example.test/{version}/{file}"
file = "acme-{version}-{os}-{arch}.tar.gz"
kind = "tar.gz"          # tar.gz|tar.xz|tar.zst|zip|7z
strip_root = true

[archive.checksum]
algorithm = "sha256"     # sha256|sha512|blake3
url = "{archive_url}.sha256"
# Or: value = "<fixed hexadecimal digest>"
# Or, when the upstream publishes no digest file at all:
#   attestation = { repo = "owner/repo" }

# Optional. A compiler toolchain is not usable from PATH alone, so a build
# system is pointed at it through variables like these.
[env]
CC = "{install_path}/bin/acme-gcc"
SYSROOT = "{install_path}/sysroot"
```

## Describing a toolchain environment

`PATH` and shims make a tool's commands runnable, which is all an interpreter or
a CLI needs. A C/C++ toolchain is different: CMake, Autoconf, and Make locate a
cross compiler through environment variables, so a definition that only adds
`bin/` to `PATH` installs a compiler the build system still cannot find.

The optional `[env]` table closes that gap. Every variable it declares is
exported whenever that version is active — during `osdk exec`, through shell
activation, and for shims.

```toml
[env]
CC = "{install_path}/bin/aarch64-none-elf-gcc"
CXX = "{install_path}/bin/aarch64-none-elf-g++"
AR = "{install_path}/bin/aarch64-none-elf-ar"
SYSROOT = "{install_path}/aarch64-none-elf"
ACME_RELEASE = "{version}"
```

`{install_path}` expands to that version's own installation root, so a value
never has to name an absolute host path. `{version}` and `{id}` are also
available.

Because these variables reach child processes, they are the one place a
data-only definition could otherwise aim a build at arbitrary host state. Values
are therefore restricted, and every rule below is enforced when the definition is
parsed rather than when it is activated:

- Names use ASCII letters, digits, and `_`, and cannot start with a digit.
- `PATH` is reserved — declare directories through `bin_paths` so shim generation
  and activation stay consistent. `LD_PRELOAD`, `LD_LIBRARY_PATH`,
  `DYLD_INSERT_LIBRARIES`, and `DYLD_LIBRARY_PATH` are reserved as well, because
  they redirect the process or the dynamic loader outside the installation root.
  Reserved names are rejected regardless of letter case.
- Values cannot be absolute, cannot contain `..`, and cannot contain control
  characters, so they stay inside the installation root that `{install_path}`
  anchors.
- Only `{install_path}`, `{version}`, and `{id}` are accepted. Any other
  placeholder fails, and a value that would still contain an unresolved
  placeholder after rendering is an error rather than a strange export.

A definition that declares `[env]` still cannot execute code: it describes
variables, and osdk exports them.

## When an upstream renames its assets

A single template assumes an upstream names its files consistently forever. Real
projects do not. LLVM is the worked example: on Linux x86-64 it published
`clang+llvm-18.1.8-x86_64-linux-gnu-ubuntu-18.04.tar.xz` and then switched to
`LLVM-19.1.0-Linux-X64.tar.xz` in 19.1.0 — a different word order, different
capitalisation, and an embedded distro version that no platform fact can produce.
Windows never switched at all, so within one release both spellings are live.

`[[archive.overrides]]` expresses that. Each entry states the conditions it
applies to and the fields it replaces:

```toml
[archive]
url = "https://github.com/llvm/llvm-project/releases/download/llvmorg-{version}/{file}"
file = "LLVM-{version}-Linux-X64.tar.xz"
kind = "tar.xz"
strip_root = true

# Linux used the older spelling before 19.1.0. The `ubuntu-18.04` fragment is
# not derivable from anything, so it is written out.
[[archive.overrides]]
versions = "<19.1.0"
os = "linux"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-linux-gnu-ubuntu-18.04.tar.xz"

# Windows kept the older spelling on both sides of that boundary.
[[archive.overrides]]
os = "windows"
arch = "x86_64"
file = "clang+llvm-{version}-x86_64-pc-windows-msvc.tar.xz"
```

Conditions are `versions` (a semver requirement such as `<19.1.0` or
`>=17, <18`), plus `os`, `arch`, and `libc`. They combine: an entry applies only
where every condition it sets is true, which is what the LLVM case needs, because
the rename happened per platform rather than for a whole release.

An override may replace `url`, `file`, `kind`, `strip_root`, and `checksum`.
Anything it leaves unset falls back to `[archive]`, so an entry that only renames
a file does not restate the rest.

Two rules keep the outcome predictable:

- **The most specific match wins**, counted by how many conditions an entry sets
  — not by declaration order. Reordering a definition cannot change which archive
  is installed.
- **Two equally specific matches are an error**, reported when that version and
  platform are resolved. Choosing by order would make the result depend on
  something easy to reshuffle by accident, so osdk asks you to narrow one instead.

Conditions accept both arch spellings (`x64` and `x86_64` both match x86-64), so
a definition does not need a second vocabulary alongside `{arch_llvm}`. A version
that is not semver never satisfies a `versions` requirement; it falls back to the
defaults rather than failing the install.

## Taking the digest from an attestation

Some upstreams publish no digest file. LLVM is the example: its releases carry
`.sig` (GPG) and, from 19.1.0, `.jsonl` sigstore bundles, but no `.sha256`. A
sigstore bundle's in-toto subject already contains the artifact's SHA-256, so the
attestation is both the signature and the digest — there is nothing else to fetch.

```toml
[archive.checksum]
algorithm = "sha256"
attestation = { repo = "llvm/llvm-project" }
```

This is the same check `gh attestation verify --repo llvm/llvm-project` performs,
and it does not need the `gh` CLI: osdk verifies the bundle against its embedded
trusted root, requiring the certificate to come from GitHub Actions' OIDC issuer
and from a workflow in the repository you named.

Because the digest is only known once the bytes are on disk, verification happens
after the download rather than before it. The archive is still never extracted
unverified — the attested digest becomes the checksum the pipeline enforces.

Two consequences are worth knowing before relying on it:

- **Verification is mandatory here.** When the attestation *is* the digest source,
  osdk requires it regardless of the global `attestations` setting (whose default
  is `off`). Otherwise a definition that named an attestation would install an
  archive with no integrity evidence at all.
- **Coverage is not universal.** Attestations only exist for artifacts built by a
  GitHub Actions workflow after the upstream adopted them. For LLVM that means
  19.1.0 and later, and even then only some assets per release — its Windows
  archives are not attested at all. An `[[archive.overrides]]` entry can switch
  digest source per version and platform for exactly this reason.

## Validation and security boundaries

- Exactly one of `versions.values` and `versions.url` is required; at most 10,000 versions are accepted.
- An ID starts with a lowercase ASCII letter or digit and then uses only lowercase letters, digits, `-`, and `_`; the `github` namespace is reserved.
- `bin_paths` and `bin_names` cannot be empty, and every path/name must stay safely inside the installation root.
- Either the archive URL or filename must vary with `{version}`. The same applies to each `[[archive.overrides]]` entry, counting the fields it inherits.
- Every `[[archive.overrides]]` entry must set at least one condition (`versions`, `os`, `arch`, `libc`) and replace at least one field, so an entry can neither shadow the defaults for everything nor claim a match while changing nothing. A `versions` value must be a valid semver requirement. The most specific match wins; two equally specific matches are rejected instead of resolved by order.
- Archive kinds are limited to `tar.gz|tar.xz|tar.zst|zip|7z`. `.7z` exists because Windows GCC toolchains are commonly published in that format only; entry paths are checked so an archive cannot write outside the extraction directory.
- Exactly one checksum `value`, `url`, or `attestation` is required, with digest length matching the algorithm. `attestation` requires `algorithm = "sha256"`, because a GitHub attestation subject carries a SHA-256 digest, and its `repo` must be a plain `owner/repo` — the value becomes a sigstore certificate-identity policy, so it is validated rather than passed through. An attested digest is always verified, even when the global `attestations` setting is `off`.
- Versions/archive URLs accept only HTTP(S); a checksum URL may additionally derive from `{archive_url}`.
- Allowed template variables depend on location and come from `{id}`, `{version}`, `{os}`, `{arch}`, `{arch_llvm}`, `{libc}`, `{file}`, and `{archive_url}`; unsupported variables fail. `[env]` values accept only `{install_path}`, `{version}`, and `{id}`.
- `{arch}` renders osdk's short token (`x64`, `arm64`, `x86`, `arm`), while `{arch_llvm}` renders the CPU part of an LLVM target triple (`x86_64`, `aarch64`, `i686`, `armv7`). Compiler and toolchain archives are usually published with the triple spelling, so use `{arch_llvm}` for those and `{arch}` for runtimes that follow Node-style naming.
- `[env]` names cannot be `PATH` or a dynamic-loader variable, and `[env]` values must be relative, free of `..`, and anchored inside the installation root.
- The schema rejects unknown fields, so it cannot contain hooks or install scripts.

Declarative backends describe data and cannot execute custom code. Installation
still uses the shared download, checksum, safe-extraction, and CAS-materialization
pipeline. After `osdk lock` records an artifact receipt, a later no-argument
`osdk install` uses that exact URL, filename, checksum, and optional subdirectory
before rendering the current plugin templates. A cached artifact can therefore
be reinstalled offline even if the version/checksum endpoint is unavailable or
the local plugin definition has since changed.

Fish activation and deactivation treat absent optional saved variables as a
successful no-op, including repeated activation or deactivation from scripts.

Fresh native Go/Cargo, npm, HTTP, GitHub and Conda installs also record integrity manifests before publication. Older installs without a baseline must be reinstalled; verification never invents a baseline from potentially modified files.

`install --force` invalidates the completion marker of the exact resolved dynamic install identity under its lock, so native Go/Cargo installs rebuild instead of silently reusing the previous payload.
