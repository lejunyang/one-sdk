#!/usr/bin/env bash
set -Eeuo pipefail

wine_version=11.15
wine_archive="wine-${wine_version}-amd64-wow64.tar.xz"
wine_sha256=e9c307a28575ae01a33610677ddb4708551a9d19d0883af5a255f370df0f7e59
wine_url="https://github.com/Kron4ek/Wine-Builds/releases/download/${wine_version}/${wine_archive}"
target=x86_64-pc-windows-gnu

test_base=${OSDK_TEST_TMPDIR:-${TMPDIR:-/tmp}}
mkdir -p "$test_base"
test_root=$(mktemp -d "$test_base/one-sdk-wine-tests.XXXXXX")
cache_root=${OSDK_WINE_CACHE_DIR:-"${TMPDIR:-/tmp}/one-sdk-wine-cache"}
archive_path="$cache_root/$wine_archive"
wine_root="$cache_root/wine-${wine_version}-amd64-wow64"
toolchain_cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
toolchain_cargo=$(rustup which cargo)
toolchain_rustc=$(rustup which rustc)
toolchain_rustdoc=$(rustup which rustdoc)

cleanup() {
  if [[ -x "$wine_root/bin/wineserver" && -n ${WINEPREFIX:-} ]]; then
    "$wine_root/bin/wineserver" -k >/dev/null 2>&1 || true
  fi
  rm -rf "$test_root"
}
trap cleanup EXIT

for command in curl rustup sha256sum tar x86_64-w64-mingw32-gcc; do
  command -v "$command" >/dev/null || {
    printf 'Required command is unavailable: %s\n' "$command" >&2
    exit 1
  }
done

mkdir -p "$cache_root"
if [[ ! -f "$archive_path" ]] ||
  [[ $(sha256sum "$archive_path" | awk '{print $1}') != "$wine_sha256" ]]; then
  rm -f "$archive_path"
  curl --fail --location --retry 3 --silent --show-error \
    "$wine_url" --output "$archive_path"
fi
printf '%s  %s\n' "$wine_sha256" "$archive_path" | sha256sum --check --status

if [[ ! -x "$wine_root/bin/wine" ]]; then
  rm -rf "$wine_root"
  tar -xJf "$archive_path" -C "$cache_root"
fi

export HOME="$test_root/home"
export OSDK_DATA_DIR="$test_root/osdk/data"
export OSDK_CACHE_DIR="$test_root/osdk/cache"
export OSDK_CONFIG_DIR="$test_root/osdk/config"
export OSDK_STORE_DIR="$test_root/osdk/store"
export OSDK_INSTALL_DIR="$test_root/osdk/installs"
export CARGO_HOME="${OSDK_WINE_CARGO_HOME:-$test_root/cargo}"
export RUSTUP_HOME="$test_root/rustup"
export RUSTC="$toolchain_rustc"
export RUSTDOC="$toolchain_rustdoc"
# An explicit temporary build cache can avoid recompiling the entire dependency
# graph on a repair/retry; SDK and Wine state remain unique to this run.
export CARGO_TARGET_DIR="${OSDK_WINE_TARGET_DIR:-$test_root/target}"
export TMPDIR="$test_root/tmp"
export WINEARCH=win64
export WINEPREFIX="$test_root/wineprefix"
export WINEDEBUG=-all
export WINEDLLOVERRIDES='mscoree,mshtml='
# Wine translates Windows Unicode paths through the host locale. In a minimal
# container's C locale even CreateDirectoryW fails for Chinese directory names,
# before any osdk code runs. Use the UTF-8 locale shipped by Linux libc.
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER="$wine_root/bin/wine"
mkdir -p \
  "$HOME" \
  "$OSDK_DATA_DIR" \
  "$OSDK_CACHE_DIR" \
  "$OSDK_CONFIG_DIR" \
  "$OSDK_STORE_DIR" \
  "$OSDK_INSTALL_DIR" \
  "$CARGO_HOME" \
  "$RUSTUP_HOME" \
  "$CARGO_TARGET_DIR" \
  "$TMPDIR"
if [[ -f "$toolchain_cargo_home/config.toml" ]]; then
  cp "$toolchain_cargo_home/config.toml" "$CARGO_HOME/config.toml"
elif [[ -f "$toolchain_cargo_home/config" ]]; then
  cp "$toolchain_cargo_home/config" "$CARGO_HOME/config"
fi

cat > "$test_root/wine-ready.c" <<'EOF'
#include <windows.h>
int main(void) {
  const wchar_t *directory = L"osdk-unicode-\u7a0b\u5e8f";
  if (!CreateDirectoryW(directory, NULL)) return 1;
  return RemoveDirectoryW(directory) ? 0 : 2;
}
EOF
x86_64-w64-mingw32-gcc "$test_root/wine-ready.c" -o "$test_root/wine-ready.exe"
(cd "$test_root" && "$wine_root/bin/wine" "$test_root/wine-ready.exe")

# Wine itself can open an overlong DOS path. Check the actual write handle
# for the backup junction to catch a missing Win32 extended-path prefix.
junction_test=commands::npm_migration::rollback::tests::node_package_migration_backs_up_junction_past_max_path
junction_trace="$test_root/junction-file.log"
if ! WINEDEBUG=-all,+file "$toolchain_cargo" test --locked -p osdk-cli --bin osdk \
  --target "$target" -- --exact "$junction_test" 2>"$junction_trace"; then
  tail -n 80 "$junction_trace" >&2
  exit 1
fi
awk -F '"' '
  BEGIN {
    # Wine escapes each Windows backslash in the trace.
    slash = "\\\\"
    extended_prefix = slash slash "?" slash
    backup_link = slash "node_modules" slash "linked"
  }
  /:trace:file:CreateFileW L"/ && $3 ~ /^ GENERIC_WRITE / &&
    index($2, slash ".node-migration-") &&
    substr($2, length($2) - length(backup_link) + 1) == backup_link {
      print
      opens++
      if (index($2, extended_prefix) == 1) extended++
  }
  END {
    if (opens != 1 || extended != 1) {
      printf "Expected one backup junction write open with a Win32 extended path; found %d write opens, %d with the prefix\n", opens, extended > "/dev/stderr"
      exit 1
    }
  }
' "$junction_trace"

"$toolchain_cargo" test --locked --workspace --target "$target"
