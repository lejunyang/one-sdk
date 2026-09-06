# Storage, Shell, and Extensions

This page explains osdk's directories, CAS and cache boundaries, shims, shell
hooks, temporary execution, completions, diagnostics, and data-only declarative
backends.

## Directory layout and overrides

| Purpose | Linux default | Environment variable |
| --- | --- | --- |
| Data root | `~/.local/share/osdk` | `OSDK_DATA_DIR` |
| Content store | `<data>/store` | `OSDK_STORE_DIR` |
| SDK installations | `<data>/installs` | `OSDK_INSTALL_DIR` |
| Cache root | `~/.cache/osdk` | `OSDK_CACHE_DIR` |
| Configuration directory | `~/.config/osdk` | `OSDK_CONFIG_DIR` |

Derived directories are:

```text
<data>/models                model snapshots
<data>/shims                 command shims
<data>/rustup                isolated RUSTUP_HOME
<data>/cargo                 isolated Rust CARGO_HOME; controlled cargo-binstall location
<data>/plugins               declarative backends
<cache>/downloads            SDK/model downloads
<cache>/tmp                  extraction scratch space
<cache>/remote               metadata and proof cache
<cache>/sources              source probe rankings
<cache>/pkg                  downstream tools' native caches
```

`OSDK_BIN_DIR` belongs to the osdk executable installer; it is not an SDK state
directory and must not be confused with `OSDK_INSTALL_DIR`.

## CAS and materialization

After verification and extraction, SDK and model files enter the BLAKE3 CAS at
paths such as `store/<first-2>/<next-2>/<full-hash>`. Installation/snapshot
manifests record relative paths, hashes, Unix modes, and symlink targets before
materializing objects into a version directory.

`settings.link_mode` and `OSDK_LINK_MODE` accept:

| Mode | Behavior |
| --- | --- |
| `auto` | On one filesystem, try hardlink, reflink, then copy; across filesystems, try reflink then copy |
| `hardlink` / `hard` | Prefer hardlink and fall back to copy |
| `reflink` / `clone` / `cow` | Prefer copy-on-write cloning and fall back to copy |
| `copy` | Regular byte copy |
| `symlink` / `sym` | Explicit symbolic links; failure does not fall back, and `auto` never selects this |

The SDK/model CAS does not contain package managers' native caches; those formats
are incompatible. See [Native cache semantics](./package-managers#native-cache-semantics).

## Cache layers and commands

```text
osdk cache dir
osdk cache env
osdk cache clean
osdk prune [--dry-run]
```

| Command | Current behavior |
| --- | --- |
| `cache dir` | Print cache root, downloads, CAS, and downstream-cache root |
| `cache env` | Print all supported downstream native-cache environment mappings |
| `cache clean` | Remove and recreate only `<cache>/downloads` |
| `prune --dry-run` | Currently print a dry-run notice without computing or listing candidate objects |
| `prune` | Use SDK-install and model-snapshot manifests as roots and remove unreferenced CAS objects |

`cache clean` does not remove the CAS, installations, models, remote/source
metadata, or `<cache>/pkg`. GC refuses to continue when it encounters a corrupt
manifest, preventing deletion of objects that may still be referenced.

`osdk container cache status` is separate: it queries storage owned by Docker
Engine or a Buildx builder through supported native aggregate interfaces. See
[Container Runtimes, Registries, and Native Operations](./containers).

When the user has not set them, the general shell hook also maps
`npm_config_cache`, `PIP_CACHE_DIR`, `GOMODCACHE`, `GOCACHE`, `CARGO_HOME`, and
`GRADLE_USER_HOME`. There is no Maven `M2_HOME`/`maven.repo.local` redirection.
Direct Rust shims and `exec` override the general `<cache>/pkg/cargo` mapping
with `<data>/cargo`.
Cargo developer-tool providers do not use either location as their build cache:
each install gets a stage-private `HOME`, `CARGO_HOME`, target, and install root.
Only an eligible controlled `cargo-binstall` executable is discovered at
`<data>/cargo/bin`; temporary source/build workspace is removed before publication.
Go developer-tool providers force their module and build caches to
`<cache>/pkg/go-mod` and `<cache>/pkg/go-build`. Their `HOME`, `GOPATH`, `GOBIN`,
and temp directories are stage-private and removed before publication.

## Deletion and confirmations

```text
osdk uninstall|rm TOOL@VERSION
osdk cache clean
osdk prune [--dry-run]
osdk model remove NAME
```

`uninstall`, `cache clean`, and non-dry-run `prune` require confirmation. An
interactive terminal displays a prompt; non-interactive use must pass `--yes`,
set `OSDK_YES=true`, or configure `settings.yes=true`, otherwise it fails.
`--quiet` hides progress but never grants consent. `model remove` currently asks
for no confirmation.
For an osdk-owned dynamic tool, uninstall derives the complete configured
identity and removes only its fingerprinted root; another identity with the same
backend and version remains installed.

## Shims and shell activation

Installation and `use` generate shims. Regenerate them after moving installation
directories with:

```text
osdk reshim
```

For dynamic tools, `reshim` publishes launchers only for the exact configured
`.osdk-install.json` schema-1 identity. It does not select another same-version
root, and legacy `.osdk-tool.json` state is never executable.

Each shim resolves the version for the current directory at execution time, so
project pins also work in IDEs, CI, and processes without a prompt hook. Shims
avoid recursively invoking themselves. On Windows, `.cmd`/`.bat` targets run via
`%ComSpec% /D /S /C call` to preserve arguments, standard I/O, and status. The
generated `.cmd` wrapper writes the install path in the system OEM code page so a
console-less cmd that falls back to the OEM code page can still reach a non-ASCII
install path; characters the OEM code page cannot express fall back to a
`chcp 65001` line before the quoted path and UTF-8 bytes.

Shell activation syntax is:

```text
osdk activate bash|zsh|fish|powershell|pwsh
osdk deactivate bash|zsh|fish|powershell|pwsh
```

Common setup forms are:

```bash
eval "$(osdk activate bash)"
eval "$(osdk activate zsh)"
osdk activate fish | source
osdk activate powershell | Invoke-Expression
```

The hook recomputes the environment at prompts/directory changes. `PATH` order is:

```text
generated shims > independent npm/pnpm/yarn > Node > other backends > original PATH
```

It also exports backend environment, downstream caches, and enabled model
adapters. osdk stores the original value of every managed variable; `deactivate`
removes the hook and restores that environment. The PowerShell hook prevents
re-entrant command lookup.

### Choosing which shims to generate

Every executable gets a shim by default. `[settings.shims]` narrows that:

```toml
[settings.shims]
include = []                      # non-empty shims only matching names
exclude = ["android-ndk:*"]       # applied last, so it always wins
```

Patterns accept `*` and `?` and ignore case; a `backend:name` pattern applies to
that backend only. Excluding a name only withholds the shim -- the tool stays
installed, stays on PATH once activated, and `osdk exec` still reaches it. Run
`osdk reshim` to apply a change.

Generation and routing read the same decision, so an excluded backend cannot
win a shared command name at run time either.

## Temporary execution

```text
osdk exec (-t|--tool TOOL[@VERSION])... -- COMMAND [ARG ...]
```

At least one `-t/--tool` is required, and the option is repeatable. osdk installs
the requested versions, then exposes their bins and environment to one child
process. It neither changes project pins nor reads the lock.

```bash
osdk exec --tool node@20 -- node --version
osdk exec --tool node@20 --tool pnpm@10 -- pnpm test
osdk exec -t bun@latest -- bunx vite
```

`pnpx` routes to managed `pnpm dlx` and `bunx` to managed `bun x`; omitting the
corresponding backend is an error. A package-manager invocation may run registry
preflight before startup. Child failure makes osdk return an error, but exact
numeric exit-code pass-through is not currently guaranteed.

## Completions

```text
osdk completions bash|elvish|fish|powershell|zsh
```

The command writes completion code to stdout; save or source it according to the
shell's conventions. For example:

```bash
osdk completions bash > ~/.local/share/bash-completion/completions/osdk
osdk completions zsh > ~/.zfunc/_osdk
osdk completions fish > ~/.config/fish/completions/osdk.fish
```

## Diagnostics

```text
osdk doctor
osdk doctor --verify
osdk doctor --verify --tool TOOL
osdk config path
osdk config list
osdk source list TOOL
osdk registry test [MANAGER]
```

| Command | Output |
| --- | --- |
| `doctor` | Platform, data/store/install directories, whether store and installs share a filesystem, shim path and PATH presence, and backend IDs |
| `doctor --verify` | Everything above, then re-hashes every installed file and names what no longer matches |
| `doctor --verify --tool` | The same check limited to one tool; a full pass reads every byte and takes minutes |
| `config path` | Config directory, user file, and current project configuration |
| `config list` | Selected effective settings/directories, registry, model environment, tools, and aliases |
| `source list` | Sources and pin for one backend/provider; `doctor` does not list mirrors |
| `registry test` | Anonymous npm-compatible registry probe and selection plan |
| `container doctor` | Read-only Docker/containerd selection plus a separate Buildx report |

Top-level `doctor` currently does not print `link_mode`; use `config list` to
inspect it. It is distinct from `container doctor`, which diagnoses native
container control planes.

## Verifying installed files

osdk verifies bytes as they are downloaded, but until they are checked again
nothing notices when an install changes afterwards. Three things do that: a tool
that updates itself in place, a manual edit, and a half-restored backup or bit
rot. In each case osdk keeps reporting the version it installed while a
different binary actually runs.

`osdk doctor --verify` re-hashes every file recorded in each install's
`.osdk-manifest.json` and reports what no longer matches:

```text
osdk doctor --verify
```

```text
  verifying installed files
  node@20.11.1 : no longer matches what osdk installed
    E:\osdk-data\data\installs\node\20.11.1
      node.exe: contents changed
  checked 9 install(s), 1 changed
  reinstall to restore (a plain install skips what is already there):
    osdk install --force node@20.11.1
```

It reports four kinds of drift: contents changed, missing, file type changed
(a file replaced by a link, or the reverse), and a link that now points
somewhere else. Installs made before the manifest existed are reported as
unverifiable rather than silently passing.

This reads every file, so it is opt-in. Plain `osdk doctor` stays a fast
environment check, and nothing on the execution path hashes anything — running a
tool is not slowed down.

The cost is disk read speed, not hashing. A full pass over 11.6 GB across 32,463
files measured about 4.5 minutes, dominated by two NDKs and a system image.
Name the tool you actually suspect to keep it usable:

```text
osdk doctor --verify --tool node
```

That same check took under a second. `--tool` requires `--verify`, and an
unknown name is rejected rather than silently checking nothing.

### Repairing what drifted

A plain `osdk install` treats an existing install as done and returns
immediately, so it will not repair one whose files changed. `--force` reinstalls
over it:

```text
osdk install --force node@20.11.1
```

Only the versions you name are affected. Dependencies that osdk installs on your
behalf are not forced along with them: bringing a dependency into place is not a
reason to rebuild it.

Because installs are hardlinked into the CAS, editing an installed file also
edits the store object behind it. osdk therefore confirms an object still hashes
to the name it is filed under before reusing it, and discards it if not, so a
reinstall genuinely repairs instead of linking the damaged bytes back. Intact
objects are still reused, so deduplication is unaffected.

### What this does not do

Verification is a snapshot, not a policy. It tells you an install no longer
matches what osdk put there; it cannot tell you whether the change was a
legitimate self-update or something hostile, because on disk the two are
identical. Tools that update themselves will keep reporting drift after every
update — for those, `--force` reinstalls the version osdk has pinned, which is
the point of pinning it.

## Declarative backends

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

### Schema 1 example

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

# Optional. A compiler toolchain is not usable from PATH alone, so a build
# system is pointed at it through variables like these.
[env]
CC = "{install_path}/bin/acme-gcc"
SYSROOT = "{install_path}/sysroot"
```

### Describing a toolchain environment

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

### Validation and security boundaries

- Exactly one of `versions.values` and `versions.url` is required; at most 10,000 versions are accepted.
- An ID starts with a lowercase ASCII letter or digit and then uses only lowercase letters, digits, `-`, and `_`; the `github` namespace is reserved.
- `bin_paths` and `bin_names` cannot be empty, and every path/name must stay safely inside the installation root.
- Either the archive URL or filename must vary with `{version}`.
- Archive kinds are limited to `tar.gz|tar.xz|tar.zst|zip|7z`. `.7z` exists because Windows GCC toolchains are commonly published in that format only; entry paths are checked so an archive cannot write outside the extraction directory.
- Exactly one checksum `value` or `url` is required, with digest length matching the algorithm.
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
