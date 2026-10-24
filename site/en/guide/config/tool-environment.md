# Tool Environments

Use [Tool Selection](./tool-selection#tool-order-reference) to choose **where a tool comes from**. This page explains **which environment variables select an existing tool, which managed execution replaces, and which caches are configurable**. The separate [osdk environment reference](./environment) lists implemented `OSDK_*` settings.

## Three separate mechanisms

1. **System selectors:** only opt-in `tool_order=["system", ...]` enables discovery through the supported tool's ambient `PATH` and the home selectors below. A selector chooses an existing executable/toolchain; it does not request an osdk installation.
2. **Managed execution variables:** selecting an osdk-managed installation prepends its binary directories and sets its required location variables for the child process/activated shell. These are not controlled by `override_env`.
3. **Caches:** existing user cache values are preserved by default. `settings.override_tools_env` or `settings.<tool>.override_env` allows osdk to replace the corresponding cache values. Neither setting changes `tool_order` or turns a cache/home variable into a selector.

## Per-tool reference {#per-tool-environment}

All source lists and the fixed idiomatic file order are in the [per-tool source matrix](./tool-selection#tool-order-reference); the source order is shared, not per-tool configuration. “PATH” below means **absolute ambient PATH entries**, excluding osdk's managed roots and shim identities; empty/relative entries are ignored. On Windows only `.exe` targets qualify, including case-insensitive extensions; `.cmd`, `.bat` and PowerShell wrappers do not.

| Tool | System selection, if enabled | Variables set for managed execution | Cache variables / override owner |
| --- | --- | --- | --- |
| <span id="node"></span>Node (`node`) | PATH → `node` | `npm_config_prefix` | `npm_config_cache`; shared npm cache uses `settings.npm`, Node's own backend also honors `settings.node` |
| <span id="python"></span>Python (`python`) | `VIRTUAL_ENV/bin` (`Scripts` on Windows), otherwise PATH → `python3`, `python` | No forced Python locator | `PIP_CACHE_DIR`, `UV_CACHE_DIR` / `python` |
| <span id="go"></span>Go (`go`) | `GOROOT/bin`, otherwise PATH → `go` | `GOROOT`, `GOBIN`; `GOPROXY` when a proxy is selected | `GOMODCACHE`, `GOCACHE` / `go` |
| <span id="java"></span>Java (`java`) | `JAVA_HOME/bin`, otherwise PATH → `java`; requires a JDK with `javac` | `JAVA_HOME` | `GRADLE_USER_HOME` / `java` |
| <span id="npm"></span>npm (`npm`) | PATH → `npm`; selected Node supplies its runtime | `npm_config_prefix` | `npm_config_cache` / `npm` |
| <span id="pnpm"></span>pnpm (`pnpm`) | PATH → `pnpm`; selected Node supplies its runtime | `PNPM_HOME` | pnpm 11+: `pnpm_config_store_dir`; earlier: `npm_config_store_dir` / `pnpm` |
| <span id="yarn"></span>Yarn (`yarn`) | PATH → `yarn`; selected Node supplies its runtime | `YARN_GLOBAL_FOLDER` | Classic only: `YARN_CACHE_FOLDER` / `yarn`; Berry uses its native project cache configuration |
| <span id="bun"></span>Bun (`bun`) | PATH → `bun` | `BUN_INSTALL` | `BUN_INSTALL_CACHE_DIR` / `bun` |
| <span id="deno"></span>Deno (`deno`) | PATH → `deno` | `DENO_INSTALL_ROOT` | `DENO_DIR` / `deno` |
| <span id="zig"></span>Zig (`zig`) | PATH → `zig` | None | `ZIG_GLOBAL_CACHE_DIR` / `zig` |
| <span id="rust"></span>Rust (`rust`) | Unsupported; `RUSTUP_HOME`/`CARGO_HOME` are not system selectors | `RUSTUP_HOME`, `CARGO_HOME` | Shared cache layer can emit `CARGO_HOME` / `rust`, but the selected managed Rust backend subsequently sets its own home |
| Maven, Gradle, Kotlin | No system adapter for these tools; a supported external Java selection can supply their JDK | JDK environment follows the selected Java runtime | Shared `GRADLE_USER_HOME` uses `java`; no generic `MAVEN_HOME` or `GRADLE_HOME` selector |
| Android families | No system adapter | SDK families: `ANDROID_HOME`, `ANDROID_SDK_ROOT`; NDK: `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT` | `ANDROID_AVD_HOME` is a separate explicit-user-value policy, not a cache override selector; see [Android](../runtimes/android) |
| Dynamic/package tools and declarative plugins | No system adapter | Backend/package environment; plugin declarations may define their own variables | No arbitrary-variable override API; consult the backend guide |

The shared cache layer participates in activation, exec and shims even when a particular tool is not selected. Tool-specific cache values are emitted by the corresponding **managed** backend. An external system target does not acquire that backend's managed cache/location setup; ambient cache values and the shared cache policy still apply. For example, selecting external pnpm does not force osdk's pnpm store, whereas shared pip/Go cache policy may still be emitted.

`PNPM_HOME`, `NVM_DIR`, `BUN_INSTALL`, `DENO_DIR`, `PIP_CACHE_DIR`, `UV_CACHE_DIR`, `PYTHONHOME`, `MAVEN_HOME` and `GRADLE_HOME` are **not executable selectors**. They may have meaning to the native tool, but osdk does not discover an executable from them. No `OSDK_NODE_VERSION`, `OSDK_PYTHON_VERSION`, universal `OSDK_<TOOL>_VERSION`, or `env` source kind is implemented. `"system"` belongs in `tool_order`, not in `[tools]` as a version string.

## Home selection and validation

An explicitly set supported home is searched before PATH. It must be an absolute directory containing an eligible executable; an invalid explicit home errors instead of silently falling back to PATH. osdk first restores its own activation-injected values to their saved ambient values so it cannot mistake its managed tool for a system candidate. Selected Java companions come from the same canonical JDK; an explicit Python venv must match the interpreter's `sys.prefix` when probed.

Version verification uses `java -version`, `go version`, `zig version`, and `--version` for the other adapters. Node requires a `vMAJOR.MINOR.PATCH` banner, Python a stable `Python MAJOR.MINOR.PATCH`, Go a stable native Go banner, and Java a numeric supported version. JS managers/Bun/Zig require stable semver; Deno's native banner is recognized. Probes are bounded to three seconds and 8 KiB per output stream. Unsupported prerelease/custom banners and Java 8 underscore versions fail validation. No replacement is installed after a failed probe.

See [source diagnostics](./tool-selection#inspect-sources) for a default **non-executing** inspection and an explicit `--probe` option. Ordinary `current` follows the execution policy: `prefer` does not probe; `error` may probe for compatibility. Diagnostics deliberately defer that strict system verification unless requested.

## Restoration when using system tools

Before executing a system target, osdk restores that tool's activation-injected forced variables (the managed column above) to the saved user value **or absence**, then applies the selected system home where needed. For example, external pnpm gets the original `PNPM_HOME`, not osdk's managed pnpm home; external Node/npm restores `npm_config_prefix`; external Go restores `GOBIN`/`GOPROXY` and applies `GOROOT` when that home selector was used. This does not promote those restored variables to selection sources.

Unrelated environment variables and existing cache policies remain intact. `deactivate` restores variables owned by shell activation. osdk does not rewrite shell startup files or native manager configuration for this selection. The older behavior that preserves `JAVA_HOME` for auxiliary JVM tools when no managed JDK is available is a narrower compatibility path, not a general system-first policy.

## Configure cache overrides {#cache-overrides}

```toml
[settings]
override_tools_env = false  # default: preserve existing explicit cache choices

[settings.python]
override_env = true         # replace PIP_CACHE_DIR and UV_CACHE_DIR

[settings.pnpm]
override_env = true         # managed pnpm's version-specific store variable
```

The global and per-tool switches are ORed: `override_env=false` cannot undo a global `true`. `OSDK_OVERRIDE_TOOLS_ENV=true` can enable the global switch; a false-like value cannot turn off a configured `true`. These switches do not stop managed `JAVA_HOME`/`PNPM_HOME` replacement and do not enable system selection. `osdk cache env` describes managed cache mappings, not a source inventory or a promise that every mapping overrides a user value.

For a venv-first Python or JDK-first Java project, set the ordinary `VIRTUAL_ENV` or `JAVA_HOME` in your shell and use:

```toml
[settings.precedence]
tool_order = ["system", "osdk"]
```

This enables system selection for all supported tools. Use [the full fallback example](./tool-selection#system-tools) if system should come after every declaration source. Explicit version requests and managed lock/frozen replay keep their authority; external paths are never portable lock entries.
