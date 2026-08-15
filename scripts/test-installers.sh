#!/usr/bin/env bash
set -Eeuo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_root=$(mktemp -d)
server_pid=

cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

export HOME="$test_root/home"
export XDG_CACHE_HOME="$test_root/xdg-cache"
export XDG_CONFIG_HOME="$test_root/xdg-config"
export XDG_DATA_HOME="$test_root/xdg-data"
export OSDK_DATA_DIR="$test_root/osdk/data"
export OSDK_CACHE_DIR="$test_root/osdk/cache"
export OSDK_CONFIG_DIR="$test_root/osdk/config"
export OSDK_STORE_DIR="$test_root/osdk/store"
export OSDK_INSTALL_DIR="$test_root/osdk/installs"
export CARGO_HOME="$test_root/cargo"
export RUSTUP_HOME="$test_root/rustup"
export CARGO_TARGET_DIR="$test_root/target"
export TMPDIR="$test_root/tmp"
mkdir -p \
  "$HOME" \
  "$XDG_CACHE_HOME" \
  "$XDG_CONFIG_HOME" \
  "$XDG_DATA_HOME" \
  "$OSDK_DATA_DIR" \
  "$OSDK_CACHE_DIR" \
  "$OSDK_CONFIG_DIR" \
  "$OSDK_STORE_DIR" \
  "$OSDK_INSTALL_DIR" \
  "$CARGO_HOME" \
  "$RUSTUP_HOME" \
  "$CARGO_TARGET_DIR" \
  "$TMPDIR"

target=x86_64-unknown-linux-gnu
release_root="$test_root/http/example/one-sdk/releases"
asset_dir="$release_root/download/v9.8.7"
latest_dir="$release_root/latest/download"
fixture_dir="$test_root/fixtures"
mkdir -p "$asset_dir" "$latest_dir" "$fixture_dir"
printf '#!/bin/sh\nprintf "osdk fixture\\n"\n' > "$fixture_dir/osdk"
printf '#!/bin/sh\nprintf "osdk-shim fixture\\n"\n' > "$fixture_dir/osdk-shim"
chmod +x "$fixture_dir/osdk" "$fixture_dir/osdk-shim"
tar -C "$fixture_dir" -czf "$asset_dir/osdk-$target.tar.gz" osdk osdk-shim
(
  cd "$asset_dir"
  sha256sum "osdk-$target.tar.gz" > SHA256SUMS
)
cp "$asset_dir/osdk-$target.tar.gz" "$asset_dir/SHA256SUMS" "$latest_dir/"

port=$((20000 + RANDOM % 20000))
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$test_root/http" \
  >"$test_root/http.log" 2>&1 &
server_pid=$!
for _ in {1..50}; do
  if curl --silent --fail "http://127.0.0.1:$port/" >/dev/null; then
    break
  fi
  sleep 0.1
done
curl --silent --fail "http://127.0.0.1:$port/" >/dev/null

install_dir="$test_root/custom bin"
OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$install_dir"

[[ $("$install_dir/osdk") == "osdk fixture" ]]
[[ $("$install_dir/osdk-shim") == "osdk-shim fixture" ]]

latest_install_dir="$test_root/latest-bin"
OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  OSDK_TARGET="$target" \
  OSDK_BIN_DIR="$latest_install_dir" \
  sh "$repo_root/install.sh"

[[ $("$latest_install_dir/osdk") == "osdk fixture" ]]
[[ $("$latest_install_dir/osdk-shim") == "osdk-shim fixture" ]]

printf '%064d  %s\n' 0 "osdk-$target.tar.gz" > "$asset_dir/SHA256SUMS"
if OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$test_root/invalid-checksum"; then
  printf 'Installer accepted an invalid checksum.\n' >&2
  exit 1
fi

help_output=$(sh "$repo_root/install.sh" --help)
grep -F -- "--version <version>" <<<"$help_output" >/dev/null
grep -F -- "--install-dir <path>" <<<"$help_output" >/dev/null

printf 'Unix installer smoke tests passed.\n'
