# Conda Developer Tool Implementation

`conda:` is the only backend in osdk that carries a dependency solver. This page
records why that is unavoidable, and the trade-offs where it deliberately
diverges from the other backends.

## Why solving cannot be avoided

Every other archive backend models one request as one archive. Conda does not
fit:

- `clang` on linux-64 is a 0.03 MB metapackage. One level of expansion already
  reaches 12 packages and ~40 MB, and is nowhere near closed (`libclang-cpp`
  24 MB, `libstdcxx` 6 MB, `llvm-openmp` 6 MB, and so on).
- Constraints look like `clang-23 ==23.1.1 default_h7037f76_0`, `libstdcxx >=15`
  and `__glibc >=2.17,<3.0.a0`.

That last one matters most: `__glibc` is a **virtual package** describing host
capability, not something downloadable. Solving therefore runs
`VirtualPackage::detect` first; without it the solver cannot evaluate such
constraints and rejects packages that would in fact run here.

The implementation uses rattler with resolvo, under `ChannelPriority::Strict` so
that the order in `channels` really is priority rather than a set of equivalent
sources the solver may merge.

## Downloads reuse osdk's own pipeline

rattler has its own HTTP entry point, but it needs an extra
`reqwest_middleware` dependency while osdk already has resumable transfers,
retries and progress reporting. Downloads therefore go through
`pipeline::download` plus `verify_file`, and rattler is used only for
`fs::extract`.

Digests can only come from `repodata.json`: the anaconda.org file metadata API
returns an empty `sha256` field. A package without a digest is **refused rather
than installed unverified** -- there is no "install now, verify never" branch.

Extraction runs inside `spawn_blocking`, and each archive is deleted as soon as
it is unpacked.

## Why metadata prefers upstream over the nearest mirror

The generic source probe measures round-trip time for one small file, which
selects the lowest-latency domestic mirror. But latency is not what dominates
this backend. Only anaconda.org publishes the CEP-16 shard index
(`repodata_shards.msgpack.zst`, 0.41 MB). Mirrors return 404 for the shard
index, so rattler falls back to the whole-subdir `repodata.json` -- 268 MB for
win-64, 35 MB zstd-compressed.

The measured gap is in the user guide: 1.8 s / 1.6 MB versus 27.6 s / 445.9 MB.
The code therefore keeps a `serves_sharded_repodata()` allowlist and a separate
`metadata_base()`, defaulting to a sharded source and honouring the user's
choice only when they pinned one explicitly or set selection to something other
than `Auto`.

SJTU is deliberately excluded: `mirror.sjtu.edu.cn` refuses connections and
`mirrors.sjtug.sjtu.edu.cn` 404s for anaconda paths, so listing it would only
spend a probe timeout before failing over.

## Install identity when a prefix is N packages

The dynamic install contract is written around one downloaded artifact and wants
an `artifact-file` / `artifact-checksum` pair. A conda prefix has no single
artifact, so identity binds to a digest of the **whole closure**: every package
URL plus its sha256, sorted and hashed, with `artifact-file` recorded as
`conda-closure-<N>.json`.

Sorting keeps solver iteration order from changing the digest. Including both
URL and digest keeps a channel from serving different bytes under a name that
already hashed.

This gives the fingerprinted directory its actual purpose: different builds of
the same version -- a different channel order, or an upstream rebuild -- land in
different install roots instead of overwriting each other.

Because the digest is knowable only after solving, every path that needs the
prefix *without* solving (`bin_paths`, `uninstall`, the shim) recovers it from
the inventory instead. An ambiguous match is an error rather than a guess:
guessing wrong would run a different build than the one requested.

## Why finalize_artifact_install is not reused

The shared `dynamic::finalize_artifact_install` rejects any symlink under the
install root. In conda packages symlinks are the norm rather than the exception:
conda-forge's linux-64 `zlib` ships `lib/libz.so -> libz.so.1.2.13`, and
versioned shared libraries generally do the same. Reusing the helper would make
this backend essentially unusable on Linux.

Conda therefore has its own finalize step that keeps the protections that
actually matter:

- Archive names are validated **before download**, and percent-decoded before
  the check, so a channel cannot use `%2E%2E%2Fevil` to write outside the
  download directory.
- Every published command must still resolve, **after following symlinks**,
  to a real file inside the prefix -- a package cannot export a command
  pointing elsewhere on the filesystem.

`validate_dynamic_install` follows the same principle: it checks the
fingerprinted root, the completion marker, the inventory manifest and that the
receipt matches the identity, but performs no whole-tree symlink scan.

## Bin directories on Windows

There is no single conda bin directory. On Windows a prefix has up to seven, and
which one a package uses is a property of how that package was built, not
something the prefix declares. The list and its order are conda's own
(`_get_path_dirs` in `conda/activate.py`):

1. the prefix root
2. `Library\<msys2 env>\bin`
3. `Library\mingw-w64\bin`
4. `Library\usr\bin`
5. `Library\bin`
6. `Scripts\`
7. `bin\`

The order matters as much as the set: it is what decides the winner when the
same command name exists in two of these directories, so a list with the right
members in the wrong order still resolves incorrectly.

Getting the set wrong fails silently -- the package downloads, verifies and
installs correctly, and then exports nothing. Two cases proved this in practice:

- `bin\` -- packages cross-built from a unix layout use it (ripgrep installs
  `bin\rg.exe`). Verified with `conda:ripgrep` on win-64.
- `Library\usr\bin` -- where every `m2-*` package lands. Without it
  `conda:m2-make`, `conda:m2-bash` and `conda:m2-pkg-config` each installed
  correctly and reported `published (0)` while their executables sat on disk.
  `Library\mingw-w64\bin` is the same story for the legacy `m2w64-*` packages:
  `conda:m2w64-toolchain` keeps its whole GCC cross toolchain there.

The MSYS2 environments (`ucrt64`, `clang64`, `mingw64`, `clangarm64`) are
mutually exclusive: only the first one present is exposed. Two of them in one
prefix are two incompatible runtimes, and putting both on PATH would decide per
command which of them wins.

Only directories that exist are returned, so a prefix cannot contribute a dead
PATH entry.

## Command ownership: who installed this executable

For other backends "what was installed" and "what should be exported" are the
same question. Not for conda: the prefix is shared by the whole closure, and
`conda:clang` ends up with 24 commands of which only three belong to clang. The
rest come from libxml2, zstd and ICU. Exporting all of them pollutes PATH and
lets a few installs start fighting over names.

Conda records every file a package installs in its `info/paths.json`, which
makes ownership answerable. There is a timing trap, though: every package
unpacks into the same prefix, so **each package's `info/` overwrites the last**.
The manifest has to be read inside the extraction loop, the moment the requested
package lands, and persisted to `.osdk-conda-paths.json`; asking afterwards is
too late.

A path counts as a command when it sits *directly* in one of the bin
directories. The directory comparison is exact rather than a prefix match: on
Windows the prefix root is itself a command directory (empty relative path), so
a prefix match would accept `lib/libclang.so` on the grounds that it starts with
the empty string. A unit test caught that; reasoning had not.

Being in a bin directory is necessary but not sufficient. A conda bin directory
is a plain directory, so packages ship their libraries next to the executables
that load them: `zstd` records `Library/bin/zstd.dll` and
`Library/bin/libzstd.dll` beside `Library/bin/zstd.exe`, and the MSYS2 packages
put `msys-2.0.dll` next to every tool in `Library\usr\bin`. `exe_stem` cannot
reject these -- it strips a known executable suffix and otherwise returns the
name unchanged -- so `zstd.dll` survived as the command name `zstd.dll` and the
shim layer would generate a shim for a library.

On Windows, ownership therefore applies the same extension test
(`has_executable_extension`) that the directory scan in `bin_names_in_dirs`
already used. The two are halves of one decision -- `bin_names` returns the
ownership list when a record exists and the scan otherwise -- so letting them
disagree makes the published set depend on which path answered. Unix is left
alone deliberately: the mode bit is the real test there, extensionless commands
are normal, and this record describes files that need not be present to stat.

**Ownership is a label, not a deletion.** The manifest records every command in
the prefix and tags each with `owned`; the filtering itself happens during shim
generation. The first implementation dropped the closure's commands from the
manifest at install time, which was wrong: the manifest is the only inventory
the shim layer reads, so a dropped command could never be recovered and the
`include` escape hatch promised in these docs silently did nothing. `owned` only
supplies the default -- an explicit `include` still reaches a dependency's
command, and `exclude` still wins. `DynamicToolBin.owned` defaults to true on
deserialization, so manifests written by older versions keep working.

Reconciliation has to apply the identical filter. It previously compared against
the unfiltered set, so it deleted shims generation had just written while
keeping ones the user had excluded. `where --bins` likewise reports the routed
decision rather than the raw backend list, or the preview would contradict the
shims produced by the next reshim.

When `paths.json` is missing or empty, the fallback is to export the whole
prefix. The asymmetry is deliberate: extra commands can be narrowed later,
whereas exporting none makes the install useless.

To recover a dependency's command, the existing `shims.include` setting is
reused -- it already matches `backend:name` globs, so conda needs no
configuration surface of its own.

## Binary size

This is the most expensive backend in osdk. With the solver and repodata stack
linked in:

| Binary | Change |
| --- | --- |
| `osdk` | 9.118 -> 11.849 MB (+29.9%) |
| `osdk-shim` | 3.487 -> 3.489 MB (+0.06%) |

That exceeds the repository's 10% guideline. The cost *is* the feature:
dependency resolution is what conda packages require and what no other backend
here can do.

Worth recording: an intermediate measurement showing **+0.66% was an illusion**.
At that point nothing called the backend yet and the linker discarded the code
entirely. Scanning the binary for symbols distinguishes the two cases -- zero
occurrences means stripped; once genuinely linked, resolvo appears 83 times and
rattler 248.

The shim-side red line holds throughout: all six rattler crates are
`optional = true` and enter only through the `install` feature, the shim's
dependency graph stays at 427 lines, and rattler, resolvo and bzip2 each appear
zero times in it.

## The gate belongs on methods, not on the module

The module and `CondaBackendFactory` were initially placed behind the `install`
feature on reasoning that sounds right: solving and unpacking really do happen
only during installation. But the boundary was drawn in the wrong place -- the
shim has to *route* `conda:clang` to this backend before it can dispatch
`clang`. With the factory unregistered, a shim build could not construct the
backend at all, and every conda command failed with `no backend provides`.

Solving is expensive; routing is not. The factory is now registered
unconditionally, and the module along with two read-only inventory helpers
(`conda_installed_locator`, `conda_prefix_root`) is no longer gated, while
solving and unpacking stay behind `install`. The red line is unaffected because
none of those functions touch a rattler type.

The gap survived as long as it did because earlier rounds only counted shim
files and never executed one. A regression test now covers it:
`every_dynamic_namespace_resolves_in_a_shim_build_too` runs in the shim's own
feature configuration and fails if any namespace regresses the same way.
