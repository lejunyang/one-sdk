# Conda Developer Tools

osdk installs toolchains and libraries from conda channels through the `conda:`
namespace. It exists for the things that **are not published as a single
archive**: CUDA, LLVM/clang, and much of the C/C++ cross-compilation ecosystem.

::: tip What makes this backend different
`github:` and `http:` download one archive and are done. A conda package is not
self-contained: `conda:clang` on linux-64 is a 30 KB metapackage whose actual
compiler lives across a dozen dependencies. Installing one therefore means
**solving its dependency closure** first -- this is the only backend in osdk
that carries a SAT solver.
:::

## Installing

```bash
osdk install conda:ripgrep
osdk exec --tool conda:ripgrep -- rg --version
```

Or pin it in a project:

```toml
[tools]
"conda:ripgrep" = "15.2.0"
```

osdk resolves the full closure, verifies each package against its sha256,
unpacks everything into one prefix, and moves that prefix into place only once
every package succeeded. An interrupted install cannot leave behind a directory
that later looks complete.

## Channels

The default channel is `conda-forge`. Use the `channels` option for others:

```bash
osdk install "conda:cuda-nvcc[channels='nvidia,conda-forge']"
```

**Order is priority, not a set.** Earlier channels win, and osdk deliberately
does not sort them -- sorting would silently change which build you get. These
are therefore three distinct install identities:

| Request | Result |
| --- | --- |
| `conda:cuda-nvcc` | Resolved from `conda-forge` only |
| `conda:cuda-nvcc[channels='nvidia,conda-forge']` | Prefer `nvidia`, fall back to `conda-forge` |
| `conda:cuda-nvcc[channels='conda-forge,nvidia']` | Prefer `conda-forge`; may select a different build |

This is not a theoretical distinction. Measured on win-64, `cuda-nvcc` solves to
a 27-package closure under `nvidia,conda-forge` and a 26-package one under
`conda-forge` alone. The two coexist under different fingerprints rather than
overwriting each other.

::: warning cuda-toolkit on Windows requires the nvidia channel
`cuda-toolkit` has **no win-64 build on conda-forge**. Windows users must pass
`channels='nvidia,conda-forge'` explicitly or the solve fails outright. This is
why the backend supported multiple channels from its first version: a
single-channel design cannot express the request at all.
:::

Channel names must be plain names (`conda-forge`, `nvidia`, `bioconda`). URLs,
path traversal, and names containing whitespace are rejected, because the
channel name becomes part of a download URL.

## Mirrors

osdk ships anaconda.org upstream plus three mirrors (TUNA, BFSU, NJU), managed
with the usual `osdk source` commands:

```bash
osdk source list conda:clang
osdk source test conda:clang
osdk source pin conda:clang tuna
```

**Upstream is preferred by default, deliberately**, which is the opposite of
what a latency measurement alone suggests. Only upstream publishes CEP-16
sharded repodata; the mirrors serve whole-subdir `repodata.json` files. Measured
from Beijing running `osdk lsr conda:clang`:

| Source | Time | repodata downloaded |
| --- | --- | --- |
| Upstream (sharded) | 1.8 s | 1.6 MB |
| Mirror (full) | 27.6 s | 445.9 MB |

The mirrors are genuinely faster per byte (4.5 vs 3.4 MB/s), but 1.3x cannot pay
for 278x the bytes. They remain in the list as failover for when upstream is
unreachable, which is the case where they actually help.

If you explicitly `osdk source pin` a mirror, osdk respects that choice and
downloads the full repodata from it.

## Versions

```bash
osdk lsr conda:clang        # list remote versions
osdk list conda:clang       # list installed
```

Conda versions are not semver (`2024.06.1` and epochs such as `1!1.2` both
occur), so osdk orders and classifies them using conda's own version ordering.
That is why `9.0.1` sorts before `10.0.0` rather than lexicographically.

## Platform support

| Platform | conda subdir |
| --- | --- |
| Linux x64 | `linux-64` |
| Linux arm64 | `linux-aarch64` |
| macOS x64 | `osx-64` |
| macOS arm64 | `osx-arm64` |
| Windows x64 | `win-64` |
| Windows arm64 | `win-arm64` |

conda-forge builds no 32-bit targets, and requesting one produces a clear error
rather than an empty solve.

win-arm64 is a newer subdir with noticeably thinner coverage than the others:
`clang` has 16 builds there starting at 22.1.8, against 123 versions on
linux-64. When a package has no build for it the solve fails and says why,
rather than quietly installing a different architecture.

## Which commands get exported

A conda prefix holds the whole dependency closure, so its `bin` directories
contain much more than the package you asked for. `conda:clang` resolves to 16
packages, and its `bin` ends up with `xmllint`, `zstd` and the ICU tools next to
the compiler.

**By default only the requested package's own commands are exported**, based on
the file list conda records in `info/paths.json`. `conda:clang` therefore
publishes three:

```bash
osdk where --bins conda:clang
# ...\installs\conda\clang\23.1.1\b3-v2-c48200a0...
# published (3): clang, clang-cl, clang-cpp
# withheld (21): clang++-23, clang-23, derb, ..., xmllint, zstd
```

Withheld commands are still installed in the prefix; they simply get no shim and
stay off PATH. To bring one back, use `osdk config set`:

```bash
osdk config set shims.include "conda:clang:xmllint"
osdk reshim
```

Both `include` and `exclude` accept `*` and `?` globs, and `exclude` is applied
after `include` so a broad include can be trimmed. These rules are shared by
every backend, not specific to conda.

`config set` writes to the project config by default; `-g` targets the user
config:

```bash
osdk config set -g shims.include "conda:clang:xmllint"   # every project
osdk config get shims.include                            # effective value
osdk config unset shims.include                          # back to default
```

A setting in a project config makes that `osdk.toml` trust-required, so
`config set` offers to trust it on the spot, and `--yes` accepts. See
[Projects and configuration](./projects#project-configuration-trust).

::: tip When paths.json is missing
A few packages ship without that manifest. osdk then exports the whole prefix
rather than nothing: a handful of extra commands can be narrowed afterwards,
whereas publishing none would make the install useless.
:::

## Lifecycle commands

```bash
osdk current conda:ripgrep
osdk where conda:ripgrep@15.2.0
osdk --yes uninstall conda:ripgrep@15.2.0
osdk reshim
```

For the details of solving, mirror selection, identity fingerprinting, and
install publication, see
[Conda Developer Tool Implementation](./implementation/conda-tools).
