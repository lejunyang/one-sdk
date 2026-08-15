#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  printf 'usage: %s <backend> <osdk-binary>\n' "${0##*/}" >&2
}

if [[ $# -ne 2 ]]; then
  usage
  exit 2
fi

backend=$1
osdk_binary=$2
smoke_root=${LIVE_SMOKE_ROOT:-}
command_timeout=${LIVE_SMOKE_COMMAND_TIMEOUT:-20m}

if [[ -z "$smoke_root" ]]; then
  printf 'LIVE_SMOKE_ROOT must point to an isolated temporary directory\n' >&2
  exit 2
fi

if [[ ! -x "$osdk_binary" ]]; then
  printf 'osdk binary is not executable: %s\n' "$osdk_binary" >&2
  exit 2
fi

if [[ "$smoke_root" == "/" || "$smoke_root" == "$HOME" ]]; then
  printf 'LIVE_SMOKE_ROOT must be separate from the invoking HOME: %s\n' "$smoke_root" >&2
  exit 2
fi

mkdir -p \
  "$smoke_root/home" \
  "$smoke_root/xdg-cache" \
  "$smoke_root/xdg-config" \
  "$smoke_root/xdg-data" \
  "$smoke_root/osdk/data" \
  "$smoke_root/osdk/cache" \
  "$smoke_root/osdk/config" \
  "$smoke_root/osdk/store" \
  "$smoke_root/osdk/installs" \
  "$smoke_root/cargo" \
  "$smoke_root/rustup" \
  "$smoke_root/build" \
  "$smoke_root/tmp" \
  "$smoke_root/project" \
  "$smoke_root/logs"

export HOME="$smoke_root/home"
export XDG_CACHE_HOME="$smoke_root/xdg-cache"
export XDG_CONFIG_HOME="$smoke_root/xdg-config"
export XDG_DATA_HOME="$smoke_root/xdg-data"
export OSDK_DATA_DIR="$smoke_root/osdk/data"
export OSDK_CACHE_DIR="$smoke_root/osdk/cache"
export OSDK_CONFIG_DIR="$smoke_root/osdk/config"
export OSDK_STORE_DIR="$smoke_root/osdk/store"
export OSDK_INSTALL_DIR="$smoke_root/osdk/installs"
export CARGO_HOME="$smoke_root/cargo"
export RUSTUP_HOME="$smoke_root/rustup"
export CARGO_TARGET_DIR="$smoke_root/build"
export TMPDIR="$smoke_root/tmp"
export OSDK_JOBS=1
export OSDK_LANG=en
export RUST_BACKTRACE=1

log_file="$smoke_root/logs/$backend.log"
exec > >(tee "$log_file") 2>&1

run() {
  printf '\n+'
  printf ' %q' "$@"
  printf '\n'
  timeout --foreground "$command_timeout" "$@"
}

request=
list_tool=
version_command=()
install_options=()
exec_tools=()
cleanup_tools=()

case "$backend" in
  node)
    list_tool=node
    request=node@lts
    version_command=(node --version)
    ;;
  go)
    list_tool=go
    request=go@stable
    version_command=(go version)
    ;;
  python)
    list_tool=python
    request=python@3.12
    version_command=(python3 --version)
    ;;
  java)
    list_tool=java
    request=java@temurin-21
    version_command=(java -version)
    ;;
  rust)
    list_tool=rust
    request=rust@stable
    install_options=(-o profile=minimal)
    version_command=(rustc --version)
    ;;
  pnpm)
    list_tool=pnpm
    request=pnpm@latest
    version_command=(pnpm --version)
    ;;
  yarn)
    list_tool=yarn
    request=yarn@latest
    exec_tools=(--tool node@lts)
    cleanup_tools=(node)
    version_command=(yarn --version)
    ;;
  deno)
    list_tool=deno
    request=deno@latest
    version_command=(deno --version)
    ;;
  bun)
    list_tool=bun
    request=bun@latest
    version_command=(bun --version)
    ;;
  github)
    export OSDK_ATTESTATIONS=required
    list_tool=github:cli/cli
    request=github:cli/cli@latest
    version_command=(gh --version)
    ;;
  *)
    printf 'unsupported live-smoke backend: %s\n' "$backend" >&2
    exit 2
    ;;
esac

exec_tools+=(--tool "$request")
cleanup_tools+=("$list_tool")

printf 'backend=%s\n' "$backend"
printf 'request=%s\n' "$request"
printf 'smoke_root=%s\n' "$smoke_root"
printf 'command_timeout=%s\n' "$command_timeout"

cd "$smoke_root/project"
run "$osdk_binary" --quiet list-remote "$list_tool"
run "$osdk_binary" --quiet --yes install "$request" "${install_options[@]}"
run "$osdk_binary" --quiet lock "$request" "${install_options[@]}"

lock_artifact="$smoke_root/logs/$backend.osdk.lock"
if [[ ! -s osdk.lock ]]; then
  printf 'lock command did not write osdk.lock\n' >&2
  exit 1
fi
cp osdk.lock "$lock_artifact"
printf '\nlock_artifact=%s\n' "$lock_artifact"
cat "$lock_artifact"

run "$osdk_binary" list "$list_tool"
run "$osdk_binary" exec "${exec_tools[@]}" -- "${version_command[@]}"

for cleanup_tool in "${cleanup_tools[@]}"; do
  printf '\n+ %q list %q\n' "$osdk_binary" "$cleanup_tool"
  list_output=$(timeout --foreground "$command_timeout" \
    "$osdk_binary" list "$cleanup_tool")
  printf '%s\n' "$list_output"

  installed_version=
  while IFS= read -r line; do
    if [[ "$line" == "  "* ]]; then
      installed_version=${line#"  "}
    fi
  done <<< "$list_output"

  if [[ -z "$installed_version" ]]; then
    printf 'could not determine installed version for %s\n' "$cleanup_tool" >&2
    exit 1
  fi
  run "$osdk_binary" uninstall "$cleanup_tool@$installed_version"
done

remaining_marker=$(find "$OSDK_INSTALL_DIR" -type f -name .osdk-complete -print -quit)
if [[ -n "$remaining_marker" ]]; then
  printf 'complete marker remains after uninstall: %s\n' "$remaining_marker" >&2
  exit 1
fi
printf '\nno complete markers remain under %s\n' "$OSDK_INSTALL_DIR"
