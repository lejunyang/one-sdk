#!/usr/bin/env bash
set -Eeuo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
test_root=$(mktemp -d)
server_pid=
lock_holder_pid=
binaries=(osdk osdk-shim osdk-aube)

cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n "$lock_holder_pid" ]]; then
    kill "$lock_holder_pid" 2>/dev/null || true
    wait "$lock_holder_pid" 2>/dev/null || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

write_executable() {
  local path=$1
  local output=$2
  printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$output" > "$path"
  chmod +x "$path"
}

write_install_set() {
  local directory=$1
  local label=$2
  local binary
  mkdir -p "$directory"
  for binary in "${binaries[@]}"; do
    write_executable "$directory/$binary" "$label $binary"
  done
}

assert_install_set() {
  local directory=$1
  local label=$2
  local binary
  for binary in "${binaries[@]}"; do
    [[ $("$directory/$binary") == "$label $binary" ]]
  done
}

assert_no_transaction_dirs() {
  local directory=$1
  local candidate
  for candidate in "$directory"/.osdk-install.*; do
    [[ -d "$candidate" ]] || continue
    [[ ${candidate##*/} == .osdk-install.lock ]] && continue
    printf 'Installer left a transaction directory under %s\n' "$directory" >&2
    exit 1
  done
}

write_transaction_journal() {
  local transaction=$1
  shift
  mkdir -p "$transaction"
  printf '%s\n' 'version=1' 'phase=initializing' > "$transaction/journal"
  local phase
  for phase in "$@"; do
    printf 'phase=%s\n' "$phase" >> "$transaction/journal"
  done
}

assert_recovery_before_staging_failure() {
  local install_directory=$1
  local expected_label=$2
  local stale_transaction=$3
  local failure_marker="$test_root/recovery-probe-${RANDOM}"
  local error_log="$failure_marker.error"

  if PATH="$copy_wrapper_dir:$PATH" \
    OSDK_TEST_REAL_CP="$real_cp" \
    OSDK_TEST_CP_FAILURE_MARKER="$failure_marker" \
    OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
    OSDK_REPOSITORY=example/one-sdk \
    sh "$repo_root/install.sh" \
      --version 9.8.7 \
      --target "$target" \
      --install-dir "$install_directory" 2>"$error_log"; then
    printf 'Recovery probe unexpectedly completed installation.\n' >&2
    exit 1
  fi
  [[ -f "$failure_marker" ]]
  assert_install_set "$install_directory" "$expected_label"
  [[ ! -e "$stale_transaction" ]]
  assert_no_transaction_dirs "$install_directory"
}

write_portable_lock_record() {
  local record=$1
  local pid=$2
  local identity=$3
  local token=${record##*/}
  printf '%s\n' \
    'version=1' \
    "machine=$(uname -n)" \
    "pid=$pid" \
    "identity=$identity" \
    "token=$token" > "$record"
}

portable_process_identity() (
  identity_pid=$1
  case $identity_pid in
    ''|*[!0-9]*) exit 1 ;;
  esac

  if [ -r "/proc/$identity_pid/stat" ] &&
     [ -r /proc/sys/kernel/random/boot_id ]; then
    identity_stat=$(cat "/proc/$identity_pid/stat") || exit 1
    identity_stat_separator=') '
    identity_rest=${identity_stat##*"$identity_stat_separator"}
    set -- $identity_rest
    [ "$#" -ge 20 ] || exit 1
    shift 19
    identity_boot=$(cat /proc/sys/kernel/random/boot_id) || exit 1
    printf 'linux:%s:%s\n' "$identity_boot" "$1"
    exit 0
  fi

  identity_start=$(
    LC_ALL=C ps -o lstart= -p "$identity_pid" 2>/dev/null |
      awk '{$1=$1; print}'
  )
  [ -n "$identity_start" ] || exit 1
  if command -v sysctl >/dev/null 2>&1; then
    identity_boot=$(LC_ALL=C sysctl -n kern.boottime 2>/dev/null || :)
  else
    identity_boot=
  fi
  [ -n "$identity_boot" ] || identity_boot=unknown-boot
  printf 'posix:%s:%s:%s\n' "$(uname -n)" "$identity_boot" "$identity_start"
)

write_checksum() {
  local archive_path=$1
  local checksum_path=$2
  local digest
  if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum "$archive_path" | awk '{ print $1 }')
  else
    digest=$(shasum -a 256 "$archive_path" | awk '{ print $1 }')
  fi
  printf '%s  %s\n' "$digest" "${archive_path##*/}" > "$checksum_path"
}

export HOME="$test_root/home"
export XDG_CACHE_HOME="$test_root/xdg-cache"
export XDG_CONFIG_HOME="$test_root/xdg-config"
export XDG_DATA_HOME="$test_root/xdg-data"
export OSDK_DATA_DIR="$test_root/osdk/data"
export OSDK_CACHE_DIR="$test_root/osdk/cache"
export OSDK_CONFIG_DIR="$test_root/osdk/config"
export OSDK_STORE_DIR="$test_root/osdk/store"
export OSDK_INSTALL_DIR="$test_root/osdk/installs"
export OSDK_SKIP_VERIFY=0
export CARGO_HOME="$test_root/cargo"
export RUSTUP_HOME="$test_root/rustup"
export CARGO_TARGET_DIR="$test_root/target"
export TMPDIR="$test_root/tmp"
unset OSDK_TARGET
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

case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-gnu ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin:x86_64|Darwin:amd64) target=x86_64-apple-darwin ;;
  Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
  *)
    printf 'Unsupported installer-test platform: %s %s\n' \
      "$(uname -s)" "$(uname -m)" >&2
    exit 1
    ;;
esac
release_root="$test_root/http/example/one-sdk/releases"
asset_dir="$release_root/download/v9.8.7"
latest_dir="$release_root/latest/download"
fixture_dir="$test_root/fixtures"
mkdir -p "$asset_dir" "$latest_dir" "$fixture_dir"
write_install_set "$fixture_dir" fixture
tar -C "$fixture_dir" -czf "$asset_dir/osdk-$target.tar.gz" osdk osdk-shim osdk-aube
write_checksum "$asset_dir/osdk-$target.tar.gz" "$asset_dir/SHA256SUMS"
cp "$asset_dir/osdk-$target.tar.gz" "$asset_dir/SHA256SUMS" "$latest_dir/"

missing_asset_dir="$release_root/download/v9.8.6"
mkdir -p "$missing_asset_dir"
tar -C "$fixture_dir" -czf "$missing_asset_dir/osdk-$target.tar.gz" osdk osdk-shim
write_checksum \
  "$missing_asset_dir/osdk-$target.tar.gz" \
  "$missing_asset_dir/SHA256SUMS"

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

copy_wrapper_dir="$test_root/copy-wrappers"
mkdir -p "$copy_wrapper_dir"
real_cp=$(command -v cp)
printf '%s\n' \
  '#!/bin/sh' \
  'case "$2" in' \
  '  */.osdk-install-v2.*/new/*)' \
  '    if [ ! -e "$OSDK_TEST_CP_FAILURE_MARKER" ]; then' \
  '      : > "$OSDK_TEST_CP_FAILURE_MARKER"' \
  '      exit 74' \
  '    fi' \
  '    ;;' \
  'esac' \
  'exec "$OSDK_TEST_REAL_CP" "$@"' \
  > "$copy_wrapper_dir/cp"
chmod +x "$copy_wrapper_dir/cp"

install_dir="$test_root/custom bin"
write_install_set "$install_dir" old
OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$install_dir"

assert_install_set "$install_dir" fixture
assert_no_transaction_dirs "$install_dir"

# Probe recovery independently from a successful reinstall: the wrapper aborts
# the next transaction's first stage copy, before it can touch a destination.
init_recovery_dir="$test_root/init-recovery-bin"
write_install_set "$init_recovery_dir" old
init_transaction="$init_recovery_dir/.osdk-install-v2.initializing"
mkdir -p "$init_transaction"
assert_recovery_before_staging_failure \
  "$init_recovery_dir" old "$init_transaction"

staging_recovery_dir="$test_root/staging-recovery-bin"
write_install_set "$staging_recovery_dir" old
staging_transaction="$staging_recovery_dir/.osdk-install-v2.staging"
write_transaction_journal "$staging_transaction" staging
mkdir -p "$staging_transaction/new" "$staging_transaction/old"
cp "$fixture_dir/osdk" "$staging_transaction/new/osdk"
assert_recovery_before_staging_failure \
  "$staging_recovery_dir" old "$staging_transaction"

prepared_recovery_dir="$test_root/prepared-recovery-bin"
write_install_set "$prepared_recovery_dir" old
prepared_transaction="$prepared_recovery_dir/.osdk-install-v2.prepared"
write_transaction_journal "$prepared_transaction" staging prepared
mkdir -p "$prepared_transaction/new" "$prepared_transaction/old"
for binary in "${binaries[@]}"; do
  cp "$fixture_dir/$binary" "$prepared_transaction/new/$binary"
done
assert_recovery_before_staging_failure \
  "$prepared_recovery_dir" old "$prepared_transaction"

promoting_recovery_dir="$test_root/promoting-recovery-bin"
write_install_set "$promoting_recovery_dir" old
promoting_transaction="$promoting_recovery_dir/.osdk-install-v2.promoting"
write_transaction_journal "$promoting_transaction" staging prepared promoting
mkdir -p "$promoting_transaction/new" "$promoting_transaction/old"
for binary in "${binaries[@]}"; do
  mv "$promoting_recovery_dir/$binary" "$promoting_transaction/old/$binary"
  cp "$fixture_dir/$binary" "$promoting_transaction/new/$binary"
done
mv "$promoting_transaction/new/osdk" "$promoting_recovery_dir/osdk"
assert_recovery_before_staging_failure \
  "$promoting_recovery_dir" old "$promoting_transaction"

# Simulate a hard stop after the old trio was moved aside and only the first
# new binary was promoted. The next installer must restore the complete old
# set before beginning its own transaction.
crash_recovery_dir="$test_root/crash-recovery-bin"
write_install_set "$crash_recovery_dir" old
stale_transaction="$crash_recovery_dir/.osdk-install.crashed"
mkdir -p "$stale_transaction/new" "$stale_transaction/old"
for binary in "${binaries[@]}"; do
  mv "$crash_recovery_dir/$binary" "$stale_transaction/old/$binary"
  cp "$fixture_dir/$binary" "$stale_transaction/new/$binary"
done
mv "$stale_transaction/new/osdk" "$crash_recovery_dir/osdk"
OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$crash_recovery_dir"
assert_install_set "$crash_recovery_dir" fixture
assert_no_transaction_dirs "$crash_recovery_dir"

latest_install_dir="$test_root/latest-bin"
OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  OSDK_BIN_DIR="$latest_install_dir" \
  sh "$repo_root/install.sh"

assert_install_set "$latest_install_dir" fixture
assert_no_transaction_dirs "$latest_install_dir"

incomplete_install_dir="$test_root/incomplete-bin"
write_install_set "$incomplete_install_dir" old
if OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.6 \
    --target "$target" \
    --install-dir "$incomplete_install_dir"; then
  printf 'Installer accepted an archive without osdk-aube.\n' >&2
  exit 1
fi
assert_install_set "$incomplete_install_dir" old
assert_no_transaction_dirs "$incomplete_install_dir"

locked_install_dir="$test_root/locked-bin"
write_install_set "$locked_install_dir" old
active_lock_owner="$locked_install_dir/.osdk-install-lock.owner.active"
live_identity=$(portable_process_identity "$$")
write_portable_lock_record "$active_lock_owner" "$$" "$live_identity"
ln "$active_lock_owner" "$locked_install_dir/.osdk-install.lock"
lock_error="$test_root/lock-error.log"
if OSDK_TEST_FORCE_PORTABLE_LOCK=1 \
  OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$locked_install_dir" 2>"$lock_error"; then
  printf 'Installer ignored an active portable installation lock.\n' >&2
  exit 1
fi
grep -F -- "another installer is updating $locked_install_dir" \
  "$lock_error" >/dev/null
assert_install_set "$locked_install_dir" old
rm -f "$locked_install_dir/.osdk-install.lock" "$active_lock_owner"

stale_lock_dir="$test_root/stale-lock-bin"
write_install_set "$stale_lock_dir" old
stale_lock_owner="$stale_lock_dir/.osdk-install-lock.owner.stale"
write_portable_lock_record "$stale_lock_owner" 2147483647 \
  'linux:stale-boot-id:1'
ln "$stale_lock_owner" "$stale_lock_dir/.osdk-install.lock"
OSDK_TEST_FORCE_PORTABLE_LOCK=1 \
  OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$stale_lock_dir"
assert_install_set "$stale_lock_dir" fixture
[[ ! -e "$stale_lock_dir/.osdk-install.lock" ]]
[[ ! -e "$stale_lock_owner" ]]

rollback_install_dir="$test_root/rollback-bin"
write_install_set "$rollback_install_dir" old
wrapper_dir="$test_root/wrappers"
mkdir -p "$wrapper_dir"
real_mv=$(command -v mv)
printf '%s\n' \
  '#!/bin/sh' \
  'case "$1:$2" in' \
  '  */.osdk-install-v2.*/new/osdk-shim:*/osdk-shim)' \
  '    if [ ! -e "$OSDK_TEST_MV_FAILURE_MARKER" ]; then' \
  '      : > "$OSDK_TEST_MV_FAILURE_MARKER"' \
  '      exit 73' \
  '    fi' \
  '    ;;' \
  'esac' \
  'exec "$OSDK_TEST_REAL_MV" "$@"' \
  > "$wrapper_dir/mv"
chmod +x "$wrapper_dir/mv"
failure_marker="$test_root/promotion-failed"
if PATH="$wrapper_dir:$PATH" \
  OSDK_TEST_REAL_MV="$real_mv" \
  OSDK_TEST_MV_FAILURE_MARKER="$failure_marker" \
  OSDK_DOWNLOAD_BASE_URL="http://127.0.0.1:$port" \
  OSDK_REPOSITORY=example/one-sdk \
  sh "$repo_root/install.sh" \
    --version 9.8.7 \
    --target "$target" \
    --install-dir "$rollback_install_dir"; then
  printf 'Installer ignored an injected promotion failure.\n' >&2
  exit 1
fi
[[ -f "$failure_marker" ]]
assert_install_set "$rollback_install_dir" old
assert_no_transaction_dirs "$rollback_install_dir"

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
[[ ! -e "$test_root/invalid-checksum/osdk" ]]
[[ ! -e "$test_root/invalid-checksum/osdk-shim" ]]
[[ ! -e "$test_root/invalid-checksum/osdk-aube" ]]

help_output=$(sh "$repo_root/install.sh" --help)
grep -F -- "--version <version>" <<<"$help_output" >/dev/null
grep -F -- "--install-dir <path>" <<<"$help_output" >/dev/null

printf 'Unix installer smoke tests passed.\n'
