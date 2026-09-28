#!/usr/bin/env bash
set -Eeuo pipefail

: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
: "${GITHUB_ENV:?GITHUB_ENV is required}"
: "${GITHUB_PATH:?GITHUB_PATH is required}"
: "${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
: "${GITHUB_ACTION_PATH:?GITHUB_ACTION_PATH is required}"

for input in \
  OSDK_ACTION_CACHE \
  OSDK_ACTION_INSTALL_TOOLS \
  OSDK_ACTION_INSTALL_DEPS \
  OSDK_ACTION_FROZEN \
  OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL \
  OSDK_ACTION_OFFLINE \
  OSDK_ACTION_REQUIRE_CHECKSUMS; do
  case ${!input:-false} in
    true|false) ;;
    *) printf '%s must be true or false, got %s\n' "$input" "${!input}" >&2; exit 2 ;;
  esac
done
case ${OSDK_ACTION_SOURCE_MODE:-auto} in
  auto|env) ;;
  *) printf 'source-mode must be auto or env, got %s\n' "$OSDK_ACTION_SOURCE_MODE" >&2; exit 2 ;;
esac
case ${OSDK_ACTION_ATTESTATIONS:-if-available} in
  off|if-available|required) ;;
  *) printf 'attestations must be off, if-available, or required, got %s\n' "$OSDK_ACTION_ATTESTATIONS" >&2; exit 2 ;;
esac

state_root="$RUNNER_TEMP/osdk"
bin_dir="$state_root/bin"
data_dir="$state_root/data"
cache_dir="$state_root/cache"
config_dir="$state_root/config"
mkdir -p "$bin_dir" "$data_dir" "$cache_dir" "$config_dir"

export OSDK_BIN_DIR="$bin_dir"
export OSDK_DATA_DIR="$data_dir"
export OSDK_CACHE_DIR="$cache_dir"
export OSDK_CONFIG_DIR="$config_dir"
export PATH="$bin_dir:$PATH"

for name in OSDK_BIN_DIR OSDK_DATA_DIR OSDK_CACHE_DIR OSDK_CONFIG_DIR; do
  printf '%s=%s\n' "$name" "${!name}" >> "$GITHUB_ENV"
done
printf '%s\n' "$bin_dir" >> "$GITHUB_PATH"

requested=${OSDK_ACTION_VERSION:-}
if [[ -z "$requested" ]]; then
  action_ref=${OSDK_ACTION_REF#refs/tags/}
  if [[ $action_ref =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+].*)?$ ]]; then
    requested=$action_ref
  else
    requested=latest
  fi
fi

installer=${OSDK_ACTION_INSTALLER:-"$GITHUB_ACTION_PATH/install.sh"}
install_args=(
  --version "$requested"
  --install-dir "$bin_dir"
  --repository "${OSDK_ACTION_REPOSITORY:-lejunyang/one-sdk}"
  --base-url "${OSDK_ACTION_DOWNLOAD_BASE_URL:-https://github.com}"
  --no-modify-shell
)
if [[ -n ${OSDK_ACTION_TARGET:-} ]]; then
  install_args+=(--target "$OSDK_ACTION_TARGET")
fi
bash "$installer" "${install_args[@]}"

osdk_bin=${OSDK_ACTION_OSDK:-"$bin_dir/osdk"}
version_output=$("$osdk_bin" --version)
if [[ $version_output != "osdk "* ]]; then
  printf 'unexpected osdk --version output: %s\n' "$version_output" >&2
  exit 1
fi
printf 'version=%s\n' "${version_output#osdk }" >> "$GITHUB_OUTPUT"
