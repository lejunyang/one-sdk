# Environment Variables

Every `OSDK_*` variable osdk reads is listed here, grouped by purpose. Overall
precedence is `command-line options > OSDK_* variables > project configuration >
user configuration > built-in defaults`; see
[Configuration Files](./files#exact-override-and-merge-semantics) for exactly how
files merge.

Boolean variables treat `1`, `true`, `yes`, and `on` (case-insensitive) as true
and any other value as false. An unrecognized value for an enumerated variable is
ignored and the configured value stays in effect, except for `OSDK_SELECTION`
as noted below.

## Directories

| Variable | Purpose | Linux default |
| --- | --- | --- |
| `OSDK_DATA_DIR` | Data root: installs, CAS, shims, models | `~/.local/share/osdk` |
| `OSDK_CACHE_DIR` | Cache root: downloads, metadata, native caches | `~/.cache/osdk` |
| `OSDK_CONFIG_DIR` | Configuration directory: `config.toml`, trust records | `~/.config/osdk` |
| `OSDK_STORE_DIR` | Content-addressed store | `<data>/store` |
| `OSDK_INSTALL_DIR` | SDK installations | `<data>/installs` |

The installer asks for the first three and writes them into your shell startup
files, so they rarely need to be set by hand. Derived directories and the CAS are
described in [Directories and Storage](./storage).

## Behavior settings

These override the fields of the same name in `[settings]`, `[sources]`, and
`[container]`.

| Variable | Configuration field | Values |
| --- | --- | --- |
| `OSDK_JOBS` | `settings.jobs` | Positive integer; also the variable for `--jobs` |
| `OSDK_MODEL_JOBS` | `sources.model_jobs` | Positive integer; the variable for `--model-jobs` |
| `OSDK_YES` | `settings.yes` | Boolean; answers yes to every confirmation |
| `OSDK_OFFLINE` | `settings.offline` | Boolean; forbids the network and uses caches only |
| `OSDK_LINK_MODE` | `settings.link_mode` | `auto`, `hardlink`, `reflink`, `copy`, `symlink` |
| `OSDK_LANG` | Output language | `en`, `zh`; takes precedence over configuration and the system locale |
| `OSDK_PRERELEASE` | `settings.prerelease` | `never`, `if-explicit`, `allow` |
| `OSDK_NPM_DEFAULT_INSTALLER` | `settings.npm.default-installer` | `npm`, `pnpm` |
| `OSDK_OVERRIDE_TOOLS_ENV` | `settings.override_tools_env` | Boolean; can only turn it on, not off when configuration enables it |
| `OSDK_PYTHON_CATALOG_URL` | `settings.python.catalog_url` | HTTP(S) URL or local path |
| `OSDK_PYTHON_CATALOG_SHA256` | `settings.python.catalog_sha256` | Hex SHA-256 |
| `OSDK_JAVA_CATALOG_URL` | `settings.java.catalog_url` | URL |
| `OSDK_CONTAINER_RUNTIME` | `container.runtime` | `auto`, `docker`, `containerd` |
| `OSDK_CONTAINER_BUILDER` | `container.builder` | `auto` or a validated Buildx builder name |
| `OSDK_CONTAINER_PLATFORM` | `container.platform` | `runtime` or `OS/ARCH[/VARIANT]` |

## Sources and security

| Variable | Configuration field | Values |
| --- | --- | --- |
| `OSDK_SELECTION` | `sources.selection` | `auto`, `pinned`, `ordered`; **an unknown value falls back to `auto`** |
| `OSDK_SOURCE_MODE` | `sources.mode` | `auto`, `env`; the variable for `--source-mode` |
| `OSDK_VERIFY_SIGNATURES` | `settings.verify_signatures` | Boolean |
| `OSDK_REQUIRE_CHECKSUMS` | `settings.require_checksums` | Boolean |
| `OSDK_ATTESTATIONS` | `settings.attestations` | `off`, `if-available`, `required` |
| `OSDK_TRUSTED_CONFIG_PATHS` | — | Reviewed files or directories joined with the OS path separator; project configuration under them is trusted for this process. See [Project Configuration Trust](./trust) |

`OSDK_SOURCE_MODE` decides how ecosystem-native mirror variables such as
`RUSTUP_DIST_SERVER`, `GOPROXY`, and `npm_config_registry` are used; see
[Download Sources and Mirrors](../sources/mirrors#mirrors-set-in-the-environment).

## Credentials

Variables on the left win. osdk sends a credential only to that service's
official endpoints and drops it on a cross-origin redirect.

| Service | Variables |
| --- | --- |
| GitHub API | `OSDK_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN` |
| Hugging Face | `OSDK_HF_TOKEN`, `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` |
| ModelScope | `OSDK_MODELSCOPE_TOKEN`, `MODELSCOPE_API_TOKEN` |
| Civitai | `OSDK_CIVITAI_TOKEN`, `CIVITAI_API_TOKEN`, `CIVITAI_TOKEN` |

Model endpoints can also be overridden with `HF_ENDPOINT`,
`MODELSCOPE_ENDPOINT` / `MODELSCOPE_DOMAIN`, and `CIVITAI_ENDPOINT`; see
[Model Snapshots](../ai/models#endpoints-and-credentials) for precedence.

## Logging

| Variable | Purpose |
| --- | --- |
| `OSDK_LOG` | A `tracing` filter that replaces the `-v` defaults; for example `OSDK_LOG=debug` enables debug logs for every crate |

## Installer scripts

`install.sh` / `install.ps1` read these only while installing osdk itself; osdk
does not read them at runtime:

| Variable | Purpose |
| --- | --- |
| `OSDK_VERSION` | Version to install; defaults to `latest` |
| `OSDK_BIN_DIR` | Directory for the executables; unrelated to the SDK state directory `OSDK_INSTALL_DIR` |
| `OSDK_REPOSITORY` | GitHub `owner/repo` |
| `OSDK_DOWNLOAD_BASE_URL` | Root URL of GitHub or a download mirror |
| `OSDK_TARGET` | Overrides the detected platform target |
| `OSDK_SKIP_VERIFY` | `1` skips SHA-256 verification; not recommended |
| `OSDK_SETUP_SHELLS` | Shells to configure: `all`, `none`, or a comma-separated list |
| `OSDK_ACCEPT_DEFAULTS` | `1` accepts every default without prompting |

See [Installation](../start/installation#customize-the-installation) for every option.

## Variables osdk sets itself

osdk sets these for child processes or the shell to track state. Do not set them
by hand:

| Variable | Meaning |
| --- | --- |
| `OSDK_TASK`, `OSDK_TASK_NAME` | The process is a task started by `osdk run`, and its name; shell hooks skip re-activation when they see `OSDK_TASK` |
| `OSDK_SHIM_ACTIVE` | The tool currently running through a shim, used to refuse a shim re-entering itself |
| `OSDK_MANAGED_ENV`, `OSDK_ORIG_<NAME>`, `OSDK_ORIGINAL_PATH` | Variables taken over during activation and their original values, restored by `deactivate` |
