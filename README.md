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

The installers download the latest release and verify it against
`SHA256SUMS`. To choose a version or destination, download the script first:

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

If Rust is already installed, `cargo install osdk-cli --locked` installs the
main `osdk` command and its private `osdk-aube` helper. Use the Release
installer for the complete three-program installation, including `osdk-shim`.

If GitHub downloads are slow, route both the installer and release downloads
through a trusted proxy:

```bash
curl --proto '=https' --tlsv1.2 -sSf \
  https://gh-proxy.com/https://raw.githubusercontent.com/lejunyang/one-sdk/main/install.sh |
  OSDK_DOWNLOAD_BASE_URL=https://gh-proxy.com/https://github.com sh
```

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

For immutable Rust reproduction, pin an explicit or dated toolchain. Floating
rustup channels such as `stable`, `beta`, and `nightly` remain floating when
written to the lock.

osdk can also follow existing `.tool-versions`, `.nvmrc`, `.node-version`,
`.python-version`, `.java-version`, `go.mod`, `rust-toolchain.toml`, and Node
version declarations in `package.json`.
Data-only declarative backends use the same locked artifact URL, checksum,
download cache, and offline reinstall path as built-in archive backends.
For osdk-owned dynamic `npm:<package>`, `cargo:<crate-or-https-url>`,
`go:<module-or-command-path>`, and `github:owner/repo` installs, options and managed-runtime dependencies that
change the selected or built output are part of the installation identity. osdk
records that identity in `.osdk-install.json`
schema 1 and places each `b3-v2:` identity under its own fingerprinted install
root, so multiple identities of the same backend and version can coexist.
Reuse, activation, shims, `where`, `uninstall`, and `reshim` all select the exact
configured identity. Older `.osdk-tool.json` manifests are detected only as
legacy state and are never reused or executed.

Guides: [Project toolchains](site/en/guide/projects.md) ·
[Lockfiles and repeatable environments](site/en/guide/lockfiles.md)

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
osdk use --global 'npm:@antfu/ni@0.21.12' -o installer=aube
osdk where --global 'npm:@antfu/ni'
osdk uninstall --global 'npm:@antfu/ni@0.21.12'
```

Project `use` respects the nearest `package.json` and exactly one compatible
existing native lock. Automatic selection prefers Aube when that lock is
compatible; `installer=aube`, `installer=npm`, and `installer=pnpm` select one
explicitly. Shell activation exposes only validated commands from packages
selected by the trusted project configuration, never the project's entire
`node_modules/.bin`. Without a `package.json`, local `use` retains the isolated
osdk-managed installation and shim behavior. The generated `.osdk/npm-bin/`
directory is local derived state and should normally be ignored by version
control. Commit the package manager's native lock alongside `osdk.lock`; the
latter does not replace the transitive dependency graph.

Global npm, pnpm, and Aube choices each run that manager's real global-add
operation inside an osdk-controlled prefix, leaving the ambient Node
installation untouched. Release installs include the `osdk-aube` companion
needed for Aube global mode. Aube 2.1 needs network access for a new or repaired
global install, although an already complete exact install with matching options
can be selected again offline without launching Aube; choose npm or pnpm when
the install itself must use a native offline mode.
`where --global` and `uninstall --global` explicitly target the user-wide npm
installation whose exact configured identity matches. Without that flag, the
existing project/isolated behavior is preserved; uninstall removes only the
selected identity root, while sibling identities of the same package version
remain installed. A global uninstall also removes its user config, lock entry,
and now-unowned shims. Project-managed npm dependencies and their curated
`.osdk/npm-bin` generation remain separate from these osdk-owned roots.

Guide: [npm developer tools](site/en/guide/npm-tools.md)

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
osdk install maven@3.9.16 gradle@9.7.0 kotlin@2.4.10
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

The Android packages contain no JDK, so `sdkmanager`, `avdmanager`, `d8` and
the other jar-backed tools run against an osdk-managed `java` when the
environment has no `JAVA_HOME` of its own. Install one with `osdk install java`.

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
osdk model list
```

Enable provider endpoint and cache variables for activated shells when model
tools should share the osdk environment:

```bash
osdk model env enable
osdk model env list
osdk model env disable huggingface
```

Guide: [Model snapshots](site/en/guide/models.md)

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

After a successful online download, require cache-only operation with
`--offline`. Tighten artifact policy when your environment requires it:

```bash
osdk --offline install node@20
osdk --require-checksums install github:sharkdp/fd
osdk --attestations required install github:cli/cli@latest
```

Review and explicitly trust project configuration that changes sources or
execution behavior:

```bash
osdk --yes trust ./osdk.toml
osdk trust list
osdk untrust ./osdk.toml
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

## Scenario: diagnose an environment or switch language

```bash
osdk doctor
osdk current
osdk where node
osdk config path
osdk config list
osdk --lang zh doctor
OSDK_LANG=en osdk --help
osdk completions bash > osdk.bash
```

osdk localizes commands, help, prompts, errors, and diagnostics in English and
Chinese. `--lang` overrides the locale for one command; `OSDK_LANG` sets the
session preference.

Guide: [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)

## Support matrix

| Area | Supported |
| --- | --- |
| Platforms | Windows, macOS, Linux |
| Runtimes | Node.js, Python, Java JDK/JRE, Go, Rust, Deno, Bun |
| Package and JVM tools | npm, pnpm, Yarn, Maven, Gradle, Kotlin |
| Other developer tools | npm packages through `npm:<package>`, registry crates or HTTPS Git repositories through `cargo:...`, Go command packages through `go:<module-or-command-path>`, public GitHub Releases through `github:owner/repo`, and exact checksum-pinned HTTPS artifacts through `http:https://...{version}...` |
| Model providers | Hugging Face, ModelScope |
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
- [Cargo developer tools](site/en/guide/cargo-tools.md)
- [Go developer tools](site/en/guide/go-tools.md)
- [Direct HTTPS artifacts](site/en/guide/http-artifacts.md)
- [Model snapshots](site/en/guide/models.md)
- [Sources, offline use, and security](site/en/guide/sources-security.md)
- [Container runtimes, registries, and native operations](site/en/guide/containers.md)
- [Storage, shell integration, diagnostics, and i18n](site/en/guide/storage-shell.md)
- [Implementation docs](site/en/guide/implementation/index.md)

Contributions are welcome through
[issues](https://github.com/lejunyang/one-sdk/issues) and pull requests.

## License

MIT
