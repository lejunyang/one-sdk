# osdk — one SDK manager

**English** · [简体中文](README.zh-CN.md) ·
[Documentation](https://lejunyang.github.io/one-sdk/en/) ·
[Releases](https://github.com/lejunyang/one-sdk/releases)

osdk gives Windows, macOS, and Linux projects one CLI for language runtimes,
package managers, developer tools, and model snapshots. Use it to:

- install and switch complete project toolchains with one command style;
- keep platform-aware project locks that teammates and CI can reuse;
- choose responsive SDK mirrors and dependency registries automatically;
- work from downloaded metadata and artifacts when the network is unavailable;
- manage Hugging Face and ModelScope snapshots alongside development tools;
- inspect Docker, containerd, Buildx, OCI registries, mirror benchmarks/plans,
  and native caches, then deliberately apply mirror config, pull images, or
  approve narrowly scoped native cleanup;
- inspect storage, caches, active versions, and environment health in English or
  Chinese.

Start with the [getting-started guide](site/en/guide/getting-started.md), or see
the [complete feature overview](site/en/guide/features.md).

## Install

Linux and macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 | iex
```

The installers download the latest release and verify it against `SHA256SUMS`.
They then detect the shells on your system, ask which to configure, and let you
confirm or change where osdk keeps its config, data, and cache. Each selected
shell gets the environment variables plus `osdk activate`, so a new shell is
ready to use. On Windows the current session is activated too; on Unix, add
`--print-activation` and eval the output to activate the shell you are in:

```bash
eval "$(sh install.sh --print-activation)"
```

To choose a version or destination, download the script first:

```bash
curl -sSfLO https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh
sh install.sh --version 0.0.1 --install-dir "$HOME/bin"
```

```powershell
Invoke-WebRequest `
  https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.ps1 `
  -OutFile install.ps1
.\install.ps1 -Version 0.0.1 -InstallDir "$HOME\bin"
```

Every prompt has a flag, so unattended installs never block:

```bash
sh install.sh --shells bash,zsh --config-dir ~/.config/osdk --accept-defaults
sh install.sh --no-modify-shell   # install the binaries only
```

```powershell
.\install.ps1 -Shells pwsh -AcceptDefaults
.\install.ps1 -NoModifyShell
```

If Rust is already installed, `cargo install osdk-cli --locked` installs the
main `osdk` command. Use the Release installer for the complete two-program
installation, including `osdk-shim`.

If GitHub downloads are slow, route both the installer and release downloads
through a trusted proxy:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

Once installed, osdk updates itself — no need to rerun the installer:

```bash
osdk self upgrade --dry-run   # what is available
osdk self upgrade             # download it and replace this installation
```

Both programs are replaced together and the download is checksum-verified.
Update sources are speed-probed just like tool downloads, so a GitHub mirror is
used automatically when it is faster; `osdk source test self` shows the
measurement and `osdk source pin self <id>` fixes the choice.

See [Installation](site/en/guide/installation.md) for PATH setup, installer
options, source builds, and verification.

## Quick start

```bash
# Install several runtimes; downloads can run concurrently.
osdk --jobs 4 install node@20 python@3.12 go@1.22

# Choose a user-wide default.
osdk use -g node@20

# Pin a version for the current project.
osdk use python@3.12

# See what this directory will use.
osdk current

# Enable automatic per-directory switching.
eval "$(osdk activate bash)"

node --version
python --version
```

Activation also supports zsh, fish, and PowerShell. Run `osdk --help` or
`osdk <command> --help` whenever you need the full command reference.

## Scenario: make a project toolchain reproducible

Pin tools in the repository, resolve them, and install the matching lock for
the current platform:

```bash
osdk use node@20
osdk use python@3.12
osdk use go@1.22
osdk lock
osdk install
```

Check for newer matching versions or run a command without changing project
pins:

```bash
osdk outdated
osdk upgrade
osdk exec --tool node@20 -- node --version
```

`osdk lock` writes only the tools the project itself declares into `osdk.lock`;
pins that live in your user-global configuration stay out, so the lock is safe to
commit and reproduces on someone else's machine. To lock a global tool too,
declare it in the project configuration or name it with `osdk lock <tool>`.

For immutable Rust reproduction, pin an explicit or dated toolchain. Floating
rustup channels such as `stable`, `beta`, and `nightly` remain floating when
written to the lock.

osdk can also follow existing `.tool-versions`, `.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`, and Node
version declarations in `package.json`.
Data-only declarative backends use the same locked artifact URL, checksum,
download cache, and offline reinstall path as built-in archive backends. They can
also declare the environment a toolchain needs — a C/C++ cross compiler is found
by build systems through `CC`, `SYSROOT`, and similar variables rather than
through `PATH` alone — and osdk exports those variables whenever the version is
active.
For osdk-owned dynamic `npm:<package>`, `cargo:<crate-or-https-url>`,
`go:<module-or-command-path>`, `conda:<package>`, `pypi:<project>`, and `github:owner/repo` installs, options and managed-runtime dependencies that
change the selected or built output are part of the installation identity. osdk
records that identity in `.osdk-install.json`
schema 1 and places each `b3-v2:` identity under its own fingerprinted install
root, so multiple identities of the same backend and version can coexist.
Reuse, activation, shims, `where`, `uninstall`, and `reshim` all select the exact
configured identity. Older `.osdk-tool.json` manifests are detected only as
legacy state and are never reused or executed.

Guides: [Project toolchains](site/en/guide/projects.md) ·
[Lockfiles and repeatable environments](site/en/guide/lockfiles.md)

## Scenario: replace a Makefile with project tasks

Declare commands in `osdk.toml` and run them with `osdk run`. Tasks are phony by
default, prerequisites run in topological order, and osdk injects the tool
versions the project declared -- no shell activation required first.

```toml
[tasks]
build = "cargo build --release"

[tasks.ci]
run = [
  "cargo fmt --check",
  { cmd = "cargo clippy -- -D warnings", ignore_error = true },
  { tasks = ["test", "doc"] },
]
depends = ["build"]
```

```bash
osdk run ci
osdk task list
osdk run ci --dry-run
```

Array entries run in order and stop at the first failure; `ignore_error`
tolerates one and continues (printing a warning); `{ tasks = [...] }` runs them
together and waits for all. Do not reach for the shell's `&` -- it means
something different in cmd, PowerShell 7, and PowerShell 5.1. See
[Project tasks](site/en/guide/tasks.md).

## Scenario: install a tool from a direct HTTPS artifact

For a tool without a dedicated backend, bind one exact semantic version to an
HTTPS `{version}` URL template and the publisher's SHA-256. A bare executable
can be installed directly:

```bash
# Replace the example digest with the SHA-256 of the exact 1.2.3 artifact.
osdk install \
  'http:https://downloads.example.com/acme-{version}[sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef,kind=file,rename=acme]@1.2.3'
```

The same backend supports `tar.gz`, `tar.xz`, and ZIP archives with explicit
`bin`/`bins`, `subdir`, `strip-components`, and single-binary `rename` layout
options. Plain HTTP, credentials, query strings, cross-origin redirects,
floating versions, and missing checksums fail closed. Downloads bypass proxies,
pin DNS results only after every address passes a conservative public-address
check, and have a 512 MiB transfer cap and 10-minute HTTP request timeout.
Archives are limited to 16,384 entries and 2 GiB of cumulative declared expanded
size; publication also requires at least one executable, and Windows publishes
only `.exe`-named outputs. After an online install and `osdk lock`,
`osdk --offline install` can replay the locked URL, filename, and checksum from
the exact identity-scoped cache; the lock does not embed the artifact bytes.

Guide: [Direct HTTPS artifacts](site/en/guide/http-artifacts.md)

## Scenario: use package managers with an available registry

Install npm, pnpm, or Yarn independently, or let an exact
`package.json#packageManager` selection join the project toolchain:

```bash
osdk install npm@11.5.2
osdk install pnpm@9.15.0
osdk install yarn@4.9.1
```

Before a package-manager process starts, osdk can select a healthy configured
registry for npm, pnpm, Yarn, Bun, and Deno. Inspect the current choice with:

```bash
osdk registry test
osdk registry test pnpm
```

Explicit registry flags, environment variables, private registries, and native
package-manager configuration remain under your control.

Guide: [Package managers and registry selection](site/en/guide/package-managers.md)

## Scenario: add an npm-published developer tool

Prefix a package with `npm:` to distinguish it from the npm package manager. In
a Node project, `use` adds the package to the nearest `package.json`, keeps an
existing dependency section (or defaults to `devDependencies`), and makes its
local command available after shell activation. Activation exposes only an
osdk-curated generation of the configured packages' validated commands, never
the project's entire `node_modules/.bin`:

```bash
osdk use npm:prettier@3
eval "$(osdk activate bash)"
prettier --check .

# Force a particular installer when the automatic choice is not wanted.
osdk use npm:eslint@9 -o installer=pnpm

# Install a user-wide tool without changing the current project.
osdk use --global 'npm:@antfu/ni@0.21.12' -o installer=npm
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

Project `use` respects the nearest `package.json` and exactly one compatible
existing native lock. Automatic selection follows the project's declared
`packageManager`, then the installer that owns an existing native lock, and
otherwise the configured default; `installer=npm` and `installer=pnpm` select
one explicitly. Set the fallback with `settings.npm.default-installer` in
`config.toml` or `OSDK_NPM_DEFAULT_INSTALLER`; it applies only when a project
states no preference of its own. Shell activation exposes only validated
commands from packages selected by the trusted project configuration, never the
project's entire `node_modules/.bin`. Without a `package.json`, local `use`
retains the isolated osdk-managed installation and shim behavior. The generated
`.osdk/npm-bin/`
directory is local derived state and should normally be ignored by version
control. Commit the package manager's native lock alongside `osdk.lock`; the
latter does not replace the transitive dependency graph.

Global npm and pnpm choices each run that manager's real global-add operation
inside an osdk-controlled prefix, leaving the ambient Node installation
untouched. `where --global` and `uninstall --global` explicitly target the
user-wide npm installation whose exact configured identity matches. Without
that flag, the existing project/isolated behavior is preserved; uninstall
removes only the
selected identity root, while sibling identities of the same package version
remain installed. A global uninstall also removes its user config, lock entry,
and now-unowned shims. Project-managed npm dependencies and their curated
`.osdk/npm-bin` generation remain separate from these osdk-owned roots.

Guide: [npm developer tools](site/en/guide/npm-tools.md)

## Scenario: install a project's own application dependencies

`install` brings tools; `deps` brings the project's own dependency manifest.
`[tools]` provides the package manager, then `deps` drives it over
`package.json` so the whole closure lands inside the project. Adding or removing
a single dependency stays with `osdk install npm:<package>`.

Declare a provider in `osdk.toml`, then run it:

```toml
[deps.pnpm]          # Node: npm / pnpm / yarn / bun
# [deps.go]          # Go / Rust / Deno: go, cargo, deno
# roots = ["apps/*"]  # monorepo: declare sub-projects; never scanned for
# [deps.uv]          # Python: pyproject.toml + uv.lock
# [deps.pip-requirements]   # Python: requirements.txt (resolves transitive deps; not frozen)
```

```bash
osdk deps --list            # detected providers and their freshness
osdk deps --dry-run         # print what would run, without running it
osdk deps                   # materialize the manifest
osdk deps --verify          # check the installed tree against its own receipts
osdk deps npm               # nearest npm project to the working directory
osdk deps //:npm            # config root only
osdk deps //apps/api:npm    # one declared sub-project
```

You don't have to call it every time. A bare `osdk install`, `osdk run <task>` or
`osdk exec` compares manifest hashes first and materializes only when stale. On a
hit that costs a fraction of a millisecond, starts no package manager, and does not
run the deep `--verify` scan. Naming a tool (`osdk install node@22`) never triggers
it; skip once with `--no-deps`, or turn it off per provider with `auto = false`:

```bash
osdk run dev                # materialize if stale, then run the task
osdk run dev --no-deps      # not this time
```

For `pip-requirements`, osdk resolves the full transitive closure with
`uv pip install -r`; a top-level `requirements.txt` is not treated as an exact
lock or synchronized set. For providers with native locks, osdk decides
frozen-vs-not by looking for the lockfile itself rather than
delegating it: with a lockfile it installs frozen, without one it falls back to
a plain install **and says so**. That matters because `yarn@1` and `bun` install
happily with no lockfile at all, so passing a flag and trusting it would be
silently wrong on half the matrix. Use `--frozen` to turn that fallback into an
error:

```bash
osdk deps --frozen
```

Build and lifecycle scripts are off by default. Turn them on per provider, which
requires approving the config:

```toml
[deps.pnpm]
allow_build_from_source = true
```

When the package manager is not installed, `deps` installs it through the same
tool install path `osdk install` uses, into osdk's isolated directories rather
than your project. In CI, where tools should come only from an explicit
`osdk install`, turn the acquisition off:

```bash
osdk deps --no-install-tools
```

With no `[deps]` section, `osdk deps` only reports what it found and which
providers could manage it. It installs nothing.

Guide: [application dependencies](site/en/guide/deps.md)

## Scenario: install a Rust CLI from Cargo

Choose one exact managed Rust version, then install a crate by exact version,
latest stable release, or numeric prefix:

```bash
osdk use rust@1.91.1
osdk use cargo:ripgrep@14.1 -o features=pcre2 -o locked=true
eval "$(osdk activate bash)"
rg --version
```

Cargo tools reject floating or linked Rust toolchains. Registry requests use
`cargo:<crate>`; HTTPS Git requests use `latest`, `tag:<ref>`, `branch:<ref>`, or
an immutable `rev:<40 lowercase hex>` selector:

```bash
osdk use \
  'cargo:https://github.com/BurntSushi/ripgrep.git@rev:0123456789abcdef0123456789abcdef01234567'
```

Supported build options are `features`, `default-features`, `bin`, `locked`
(default `false`), and Git-only `crate`. A complete exact installation can be
reused offline, but Cargo locks do not contain the complete source graph needed
for a cold offline build. Registry versions are recorded as `version-only`, full
Git revisions as `immutable-revision`, and Git HEAD/tags/branches as
`floating-ref`.

Guide: [Cargo developer tools](site/en/guide/cargo-tools.md)

## Scenario: install a Go command package

Keep the Go runtime and Go command namespaces separate, then install a command
with an exact managed Go toolchain:

```bash
osdk use go@1.24
osdk use go:golang.org/x/tools/gopls@0.20.0
eval "$(osdk activate bash)"
gopls version
```

`go:` accepts module or nested command paths, `latest`, numeric prefixes, exact
semantic versions, and canonical pseudo-versions. `tags` and a restricted
`env` option are identity-bearing; `CGO_ENABLED=0` is supported, while enabling
cgo is rejected until a C toolchain can be bound to the install identity. osdk
chooses and records one Go proxy, invokes the exact managed `go` once with a
staged `GOBIN`, and keeps module/build caches under its cache root. The compact
schema-4 lock records the proxy, module root, and exact Go runtime—not the
transitive module graph—so an exact completed install can be reused offline,
but a cold offline build cannot.

Guide: [Go developer tools](site/en/guide/go-tools.md)

## Scenario: work in each language ecosystem

The same command style applies across ecosystems, while backend-specific
capability notes live in their guides. Runtime-specific commands cover the
workflows that need them.

### Node.js

```bash
osdk install node@20 -o corepack=true
osdk lock node@20 -o arch=arm64
osdk node migrate-packages --from 20.19.0 --to 22.17.0
osdk node migrate-packages --from 20.19.0 --to 22.17.0 --apply
```

### Python

```bash
osdk install python@3.14
osdk install python@cpython-3.14+freethreaded
osdk install python@pypy-3.11
osdk python find
osdk python find pypy-3.11
```

### Java and JVM tools

```bash
osdk install java@21
osdk install java@21 -o package-type=jre
osdk install java@21 -o distribution=zulu -o package-type=jdk
osdk install maven@3.9.16 "gradle@=9.3.1" kotlin@2.4.10
```

### Go

```bash
osdk install go@1.22
osdk use go@1.22
osdk exec --tool go@1.22 -- go version
```

### Rust

```bash
osdk install rust@stable -o profile=minimal -o components=clippy,rustfmt
osdk rust component add rustfmt --toolchain stable
osdk rust target add x86_64-pc-windows-gnu --toolchain stable
osdk rust check --repair
```

### Zig

```bash
osdk install zig@latest
osdk exec --tool zig -- zig version
```

Zig bundles its own libc (musl, several glibc versions, mingw-w64, wasi-libc), so
a single install cross-compiles C and C++ to many targets without a sysroot per
target:

```bash
osdk exec --tool zig -- zig cc -target aarch64-linux-musl -o hello hello.c
osdk exec --tool zig -- zig cc -target x86_64-windows-gnu -o hello.exe hello.c
```

Guide: [Runtime and ecosystem workflows](site/en/guide/runtimes.md)

## Scenario: manage the Android SDK

Install Android SDK packages — the NDK, `adb`/`fastboot`, build-tools — straight
from Google's repository, accepting the required agreements by flag so it works
unattended in CI:

```bash
# Review the agreement before agreeing to anything
osdk android licenses show android-ndk@29.0.14206865

# Install with explicit consent
osdk install android-ndk@29.0.14206865 -o accept-licenses=true
osdk install android-platform-tools@37.0.1 -o accept-license=android-sdk-license

# Use the tools
osdk exec -t android-platform-tools@37.0.1 -- adb devices

# Hand the recorded acceptance to Gradle
osdk android licenses export --sdk-root /path/to/sdk
```

osdk never accepts a license on your behalf: without one of the accept options
the install stops before downloading anything. Available families are
`android-ndk`, `android-platform-tools`, `android-build-tools`,
`android-cmdline-tools`, `android-cmake`, `android-platforms`,
`android-emulator`, `android-sources` and `android-system-images`.

`android-platforms` and `android-sources` spell their revisions `android-37.2`,
and `latest` picks the newest stable API level. Previews have to be asked for by
name: Google publishes builds such as `android-37.2-beta3` and `android-CANARY`
**on the stable channel** (`channelRef` is `channel-0`), so osdk identifies them
from the manifest's `<codename>` and `<beta-api-level>` instead, and the default
`prerelease = if-explicit` policy still keeps them out of `latest`.

Within one family `android-36` and `android-36.1` are two different API levels,
which the names do not reveal, so `osdk list-remote` annotates each candidate
with the API level it actually declares:

```bash
osdk list-remote android-platforms
# android-36-ext19 (API 36x)   <- side-by-side extension, ExtensionLevel 19
# android-36 (API 36)
# android-36.1 (API 36.1)
```

When you mean "this one, with no fallback", pin it verbatim with `=`:

```bash
# exactly API 36; cannot drift to 36.1
osdk install "android-platforms@=android-36"
```

Emulator system images work the same way, and whatever a package declares as a
dependency is installed with it — asking for an image brings the matching
`android-emulator` along:

```bash
# Browse the images (every vendor and form factor in one list)
osdk list-remote android-system-images

# Installing this also installs the emulator the image requires
osdk install "android-system-images@android-35;google_apis;x86_64" \
  -o accept-licenses=true
```

The emulator only accepts an SDK directory that contains `platform-tools`, so
install that as well before creating an AVD. osdk arranges every Android package
into the single directory layout Google's tools expect, without storing any
package twice.

Create and run a virtual device with the image:

```bash
osdk android avd create pixel-35 --image "android-35;google_apis;x86_64"
osdk android avd list
emulator -avd pixel-35
```

Devices live under osdk''s data directory, and the emulator and `avdmanager` are
pointed at it automatically, so `emulator -avd pixel-35` works from an activated
shell without exporting anything by hand.

osdk writes the device definition itself rather than calling `avdmanager`, which
cannot work against this layout: it locates the SDK by inspecting its own path,
so it looks one directory too high, and `create avd` accepts no flag to correct
that. Devices it did manage to create also record a relative image path that
resolves against the wrong directory here.

To see what Google's own tools can see, and to rebuild the index they read:

```bash
osdk android sdk-root show
osdk android sdk-root repair
```

`repair` is worth running once after upgrading osdk: packages installed by an
earlier version have no index file, so `sdkmanager` lists them while
`avdmanager` reports `Package path is not valid`.

osdk links every Android package into the directory layout Google's tools expect
(a junction on Windows), storing the payload once. Uninstalling removes the link
before the payload: the other order leaves a dangling link, which still answers
*yes* to the existence checks the emulator and Gradle make. `sdk-root show`
reports dangling links and `repair` removes them.

The Android packages contain no JDK, so `sdkmanager`, `avdmanager`, `d8` and
the other jar-backed tools run against an osdk-managed `java` when the
environment has no `JAVA_HOME` of your own (a stale value osdk's activation
exported is recomputed for the current directory, so one project's JDK cannot
drive another's build). Install one with `osdk install java`.

Guide: [Android SDK tools](site/en/guide/android.md)

## Scenario: pin a model snapshot

Pull selected files from Hugging Face or ModelScope, verify the local snapshot,
and obtain its path:

```bash
export HF_TOKEN=... # optional for private or gated repositories

osdk model pull qwen25 \
  hf:Qwen/Qwen2.5-7B-Instruct@main \
  --include '*.json' --include '*.safetensors'
osdk model pull qwen25-ms \
  ms:Qwen/Qwen2.5-7B-Instruct@master \
  --include '*.json' --include '*.safetensors'
osdk model verify qwen25
osdk model path qwen25

# A path that survives the next pull -- snapshot directories are content-hashed,
# so feed this one to ComfyUI, llama.cpp or a script instead
osdk model path qwen25 --stable

# Restore the same snapshots elsewhere from osdk.lock (pull writes, sync replays)
osdk model sync
osdk model list
```

Render a snapshot into a ComfyUI / Hugging Face cache-shaped consumer view
(links back to the snapshot, no copied weights, read-only) and emit the wiring
config:

```bash
osdk model view add comfyui qwen25 --map unet/=diffusion_models
osdk model view path comfyui                 # stable path for consumer config
osdk model view export comfyui --to extra_model_paths.yaml   # source ComfyUI
osdk model view list
osdk model view doctor comfyui
osdk model view remove comfyui --model qwen25
```
You can also declare models in `osdk.toml`; `osdk model pull <name>` then picks up
the declaration, records the views in the lock, and renders them. Declaring what to
fetch needs no trust; only an `endpoint`/custom-source key does, and model
declarations never block ordinary tool commands:

```toml
[models.flux]
source = "hf:black-forest-labs/FLUX.1-dev@main"
[models.flux.views.comfyui.map]
"unet/" = "diffusion_models"
"vae/"  = "vae"
```

`osdk model sync` restores every locked model **and** rebuilds its views.


Enable provider endpoint and cache variables for activated shells when model
tools should share the osdk environment:

```bash
osdk model env enable
osdk model env list
osdk model env disable huggingface
```

Guide: [Model snapshots](site/en/guide/models.md)

## Scenario: install a skill for an AI coding agent

A skill is a `SKILL.md` instruction package that AI coding agents such as Claude
Code, Codex, and Cursor read. osdk installs skills from GitHub or a local path,
stages them in its content-addressed store, links them into each agent's skills
directory, and records an immutable identity in `osdk.lock` so a team can
reproduce them with `osdk skills sync`. osdk only stages and links; it never runs
a skill's scripts.

```bash
osdk skills agents                       # which agents osdk knows, and their skills dirs
osdk skills find agent skills             # search GitHub for installable skills (anonymous, no skills.sh)
osdk skills add github:vercel-labs/agent-skills --list        # list a repo's skills only
osdk skills add github:vercel-labs/agent-skills/skills/web-design-guidelines -a claude-code
osdk skills add ./my-skills -s my-skill -a codex              # a local source, one skill
osdk skills list                         # installed skills and the agents they link into
osdk skills sync                         # reproduce from osdk.lock (team / CI)
osdk skills remove web-design-guidelines
```

`add` pins the resolved commit and a content hash into `osdk.lock`; `sync`
re-downloads a missing local copy at that exact commit and re-checks the hash, so
a moved tag or a substituted mirror is refused. Installs use a directory link by
default (a junction on Windows, a symlink on Unix) and fall back to a copy where
links are unavailable or with `--copy`, never replacing a real directory osdk did
not place. You can also declare skills in `osdk.toml` under `[skills]` and let
`osdk skills sync` reproduce them.

Guide: [Agent skills](site/en/guide/skills.md)

## Scenario: control sources, offline use, and trust

Let osdk rank available sources, pin a preferred mirror, add a trusted internal
source, or override the source for one command:

```bash
osdk source list node
osdk source test node
osdk source pin node tuna
osdk source add node --id mycorp \
  --download-url https://mirror.corp/node/ \
  --index-url https://mirror.corp/node/index.json
osdk --source official install go@1.22
```

A mirror already set in your environment (`RUSTUP_DIST_SERVER`, `GOPROXY`,
`npm_config_registry`, and the like) is validated and then raced against osdk's
built-in mirrors, so a stale or unreachable value cannot win by default; an
unusable one is reported instead of being ignored. To obey it as-is, or to fail
when it is missing, ask for it explicitly:

```bash
osdk --source-mode env install rust@1.98.0
```

The Go module proxy is a separate channel: `[sources.go]` only decides where the
Go toolchain archive is downloaded from, while `go build` / `go test` fetch
dependencies through `GOPROXY`. When running a managed go, osdk injects a ranked
`GOPROXY` joined with `|` (upstream first, then mirrors, `direct` last), so an
unreachable `proxy.golang.org` falls through to a mirror instead of failing the
build. The `|` separator is required: a comma only advances on 404/410 and
treats a connection timeout as terminal.

This mirror set lives under its own `go-modules` name, independent of the
toolchain archive sources in `[sources.go]`, and the usual `osdk source`
subcommands apply to it:

```bash
osdk source list go-modules
osdk source test go-modules
osdk source add go-modules --id corp --download-url https://goproxy.corp/
osdk source pin go-modules goproxy.cn
osdk source unpin go-modules
```

A pin means "try this first", not "use only this": the pinned source moves to
the front and the rest stay on as fallbacks, so one unreachable host does not
fail the build. Use `[sources.go-modules].disable` when you want exactly one
endpoint.

A `GOPROXY` you set yourself is never overridden -- including the policy values
`off` and `direct`.

After a successful online download, require cache-only operation with
`--offline`. Tighten artifact policy when your environment requires it:

```bash
osdk --offline install node@20
osdk --require-checksums install github:sharkdp/fd
osdk --attestations required install github:cli/cli@latest
```

Review and explicitly trust project configuration that can run code on this
machine, or that weakens artifact verification or redirects where downloads come
from. Declaring which tools or packages to install is not in that category -- when
a config is refused, osdk lists exactly which keys need review and why:

```bash
osdk --yes trust ./osdk.toml
osdk trust list
osdk untrust ./osdk.toml
osdk trust prune                 # drop records whose config file is gone
```

Guide: [Sources, offline use, and security](site/en/guide/sources-security.md)

## Scenario: inspect and operate native container runtimes

Docker Hub works out of the box with two operator-documented public
pull-through caches, `mirror.gcr.io` and `docker.m.daocloud.io`. osdk benchmarks
them anonymously against `library/alpine:latest`, verifies manifest equivalence
and a bounded layer sample, and recommends the passing mirrors by measured
latency. An explicit policy in trusted user or project configuration fully
replaces those built-ins:

```toml
[containers.registries."docker.io"]
mirrors = ["https://mirror.example/"]
anonymous_only = true
resolve = "mirror"
```

```bash
osdk container doctor
osdk container doctor --runtime docker --builder my-builder
osdk container doctor --json
osdk container registry test docker.io
osdk container registry test docker.io \
  --image ubuntu:24.04 --platform linux/amd64 --json
osdk container mirrors plan docker.io --runtime docker
osdk container mirrors plan docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --json
osdk container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json
# Automation is two-step and binds the exact fresh plan:
plan_id=$(osdk container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --dry-run --json | jq -r .plan_id)
osdk --yes container mirrors apply docker.io --runtime docker \
  --native-config /etc/docker/daemon.json --accept-plan "$plan_id" --json
osdk container cache status
osdk container cache status --runtime buildkit --builder my-builder
osdk container pull ubuntu:24.04
osdk container pull ghcr.io/example/tool:1.0 \
  --runtime containerd --platform linux/amd64 \
  --address unix:///run/containerd/containerd.sock --namespace default
osdk container prune --runtime docker --scope images
osdk container prune --runtime buildkit --scope build-cache --builder my-builder
```

`container doctor` reports the selected runtime first, then the typed facts
already obtained by that probe: Docker versions/platform/rootless/Desktop and
mirror origins; containerd versions and registry-config state; and Buildx
driver, node state, BuildKit versions, endpoints, and platforms. Its
schema-version-2 JSON omits context, builder and node names, namespaces, native
config paths, and secret-bearing endpoint paths or queries.

Registry tests use anonymous HTTPS only, validate image digests, platform
selection, bounded Range support, and rank verified mirrors. A mirror plan
always targets one configured or Docker Hub built-in registry policy and one
explicit Docker, containerd, or BuildKit control plane. It reports a
deterministic `plan_id`;
without an explicit native config path a locally actionable plan is
`manual-only`. Planning never writes native configuration, starts builders, or
restarts daemons. Plan JSON can expose operational absolute paths, builder names,
and mirror origins plus whether a path prefix exists; exact mirror prefixes,
existing configuration contents, and generated candidate bytes remain hidden.
`mirrors apply` performs that benchmark and plan in one invocation, prompts
interactively without asking you to copy the ID, then rechecks the input under
a lock and atomically replaces the file. It never elevates privileges or
restarts/recreates the native service. Unattended `--yes` requires the exact
fresh `--accept-plan`; use `--dry-run --json` to obtain it.

`container pull` uses the effective runtime and platform unless you override
them. In `auto` mode it performs one bounded read-only Docker/containerd
resolution, then starts exactly one native foreground pull. Explicit containerd
selection requires paired `--address` and `--namespace` values; `auto` requires
them only if containerd wins, so Docker can proceed without them. The child
inherits stdio and osdk waits for it, returning its direct exit code or, on Unix,
normalized `128 + signal`; it does not fall back to another runtime or copy the
image into an osdk store.

`container prune` is preview-only by default. It can preview one discovered
Docker context or Buildx builder and binds a secret-safe fingerprint of the
Docker endpoint or Buildx driver/node endpoint topology. Execution is available
only for Docker contexts backed by a directly addressable local Unix socket or
Windows named pipe without context-held TLS material; it uses `docker --host`
and removes only dangling images. To execute, repeat the command with both
`--execute` and the exact reported `--accept-preview sha256:...`, then
confirm the execution prompt (or use global `--yes`). BuildKit remains preview-only
because its mutable builder name cannot be pinned atomically. Although `--scope` remains
required, containerd has no accepted scope pairing: a selector-free,
non-executing request reports typed unsupported, while selectors and execution
flags are rejected. The command never expands into system-wide cleanup of
containers, volumes, networks, or implementation-private stores.

Guide: [Container runtimes, registries, and native operations](site/en/guide/containers.md)

## Scenario: see what package managers the host has

```bash
osdk pkg doctor
osdk pkg doctor --json
osdk pkg mirrors test
osdk pkg mirrors apply --dry-run
```

Reports which system package managers the host has (winget today), their
version, and the sources they have configured along with each source's trust
level. `mirrors test` measures each mirror and ranks them, with the official
source measured alongside. Read-only: nothing is installed, no configuration
changes, no elevation. The `--json` output carries a schema version and does
not vary with the display language.

Note that a winget mirror speeds up search and list, not installer downloads:
the URLs inside a manifest point at each vendor's own servers. osdk says so in
its output.

`mirrors apply` is the only one of these that changes machine state: it needs
administrator rights and an explicit `--accept-plan` fingerprint. A mirror that
cannot be installed (older than what is present) is refused up front, and a
failure part-way through rolls itself back.

Guide: [System Package Managers](site/en/guide/system-packages.md)

## Scenario: inspect caches and reclaim storage

```bash
osdk cache dir
osdk cache env
osdk --yes cache clean
osdk prune --dry-run
osdk --yes prune
```

`cache clean` removes downloaded archives. `prune` reclaims unreferenced shared
content; `prune --dry-run` does not delete anything.

Guide: [Storage, caches, and shell integration](site/en/guide/storage-shell.md)

## Scenario: a tool updated itself and no longer matches

```bash
osdk doctor --verify --tool node
osdk install --force node@20.11.1
```

`doctor --verify` re-hashes every installed file and names the ones that changed
after installation, which is what a tool's own self-update, a manual edit or a
half-restored backup leaves behind. A plain `install` treats an existing install
as done and will not repair it; `--force` reinstalls the pinned version over it.
Verification reads every file, so it is opt-in — plain `doctor` stays fast and
nothing on the execution path hashes anything.

Guide: [Storage, caches, and shell integration](site/en/guide/storage-shell.md)

## Scenario: diagnose an environment or switch language

```bash
osdk doctor
osdk doctor --verify
osdk current
osdk where node
osdk config path
osdk config list
osdk config get jobs
osdk config set shims.exclude "apkanalyzer"
osdk --lang zh doctor
OSDK_LANG=en osdk --help
osdk completions bash > osdk.bash
```

osdk localizes commands, help, prompts, errors, and diagnostics in English and
Chinese. `--lang` overrides the locale for one command; `OSDK_LANG` sets the
session preference.

`osdk doctor` also reports proxy state. osdk reads a proxy only from
`HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`, never from the Windows system-proxy
toggle; when those disagree it says so and names the variable to set, instead of
letting a browser-working, osdk-timing-out setup go unexplained.

Guide: [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)

## Support matrix

| Area | Supported |
| --- | --- |
| Platforms | Windows, macOS, Linux |
| Runtimes | Node.js, Python, Java JDK/JRE, Go, Rust, Deno, Bun, Zig |
| Package and JVM tools | npm, pnpm, Yarn, Maven, Gradle, Kotlin |
| Other developer tools | npm packages through `npm:<package>`, registry crates or HTTPS Git repositories through `cargo:...`, Go command packages through `go:<module-or-command-path>`, conda packages and toolchains such as CUDA through `conda:<package>`, Python CLIs through `pypi:<project>` (one virtual environment per tool, with dependencies shared between them), public GitHub Releases through `github:owner/repo`, and exact checksum-pinned HTTPS artifacts through `http:https://...{version}...` |
| Model providers | Hugging Face, ModelScope |
| Agent skills | Install `SKILL.md` packages from GitHub (`github:owner/repo` with an optional subdir) or a local path, linked into Claude Code / Codex / Cursor / OpenCode / Gemini CLI / GitHub Copilot and more |
| Native container operations | Docker Engine, containerd, Docker Buildx, anonymous OCI registry tests, built-in Docker Hub mirror benchmarking, safe native mirror apply, direct native image pulls, native cache status, Docker local-endpoint pruning, and BuildKit prune previews |
| Project inputs | `osdk.toml`, `.tool-versions`, common ecosystem version files |
| Shells | Bash, zsh, fish, PowerShell |
| CLI languages | English, Chinese |

## Documentation

- [Feature overview](site/en/guide/features.md)
- [Getting started](site/en/guide/getting-started.md)
- [Project toolchains](site/en/guide/projects.md)
- [Lockfiles and repeatable environments](site/en/guide/lockfiles.md)
- [Runtime and ecosystem workflows](site/en/guide/runtimes.md)
- [Package managers and registry selection](site/en/guide/package-managers.md)
- [npm developer tools](site/en/guide/npm-tools.md)
- [application dependencies](site/en/guide/deps.md)
- [Cargo developer tools](site/en/guide/cargo-tools.md)
- [Go developer tools](site/en/guide/go-tools.md)
- [Direct HTTPS artifacts](site/en/guide/http-artifacts.md)
- [Model snapshots](site/en/guide/models.md)
- [Agent skills](site/en/guide/skills.md)
- [Sources, offline use, and security](site/en/guide/sources-security.md)
- [Container runtimes, registries, and native operations](site/en/guide/containers.md)
- [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)
- [Implementation docs](site/en/guide/implementation/index.md)

Contributions are welcome through
[issues](https://github.com/lejunyang/one-sdk/issues) and pull requests.

## License

MIT
