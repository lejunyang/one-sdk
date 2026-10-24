# Zig

Configuration reference: [applicable tool_order sources](../config/tool-selection#zig), [project source diagnostics](../config/tool-selection#inspect-sources), and [this tool’s selectors, managed variables and cache overrides](../config/tool-environment#zig).

```bash
osdk install zig@latest
osdk use zig@0.16
osdk exec --tool zig -- zig version
```

Versions come from `ziglang.org/download/index.json`, which lists every release
with its archive URL and SHA-256 together, so each install is checksum-verified
and can be locked. Zig's GitHub releases carry only source and bootstrap
archives, so the generic `github:` backend cannot install it. Although the index
contains absolute tarball URLs, osdk rebases their relative release path across
the ranked source download roots. A mirror whose index works but whose target
archive is missing, corrupt, or not extractable therefore falls through.

`master` is a rolling nightly, not a release. It is treated as a prerelease, so
`zig@latest` always selects a tagged version; request it explicitly with
`osdk install zig@master` under a permissive
[prerelease policy](../tools/github-releases).

## Zig as a C and C++ cross compiler

`zig cc` and `zig c++` are Clang drivers that ship their own libc: musl,
several glibc versions, mingw-w64 and wasi-libc are all bundled. One install
therefore cross-compiles to many targets with no per-target sysroot, which is
exactly what the [Android NDK cannot do](./android#the-ndk-is-not-a-general-cross-compiler)
outside Android.

```bash
# Same source, no extra downloads, no sysroot to point at.
osdk exec --tool zig -- zig cc -target aarch64-linux-musl -o hello hello.c
osdk exec --tool zig -- zig cc -target x86_64-windows-gnu -o hello.exe hello.c
```

Verified on a Windows x86-64 host with a source that includes `<stdio.h>`; every
target below produced a binary for the right machine:

| `-target` | Output |
| --- | --- |
| `x86_64-linux-gnu` | ELF, x86-64 |
| `aarch64-linux-gnu` | ELF, aarch64 |
| `x86_64-linux-musl` | ELF, x86-64, static |
| `aarch64-linux-musl` | ELF, aarch64, static |
| `riscv64-linux-musl` | ELF, riscv |
| `x86_64-windows-gnu` | PE |
| `wasm32-wasi` | wasm |

Zig can also stand in for a C compiler in other build systems by pointing `CC`
at it, which is useful for Rust crates with C dependencies:

```bash
osdk exec --tool zig -- cargo build   # with CC="zig cc" in the environment
```

`ZIG_GLOBAL_CACHE_DIR` is set to a directory inside osdk's cache so build
artifacts do not accumulate in your home directory. An existing value is left
untouched.
