#!/usr/bin/env bash
set -Eeuo pipefail

wine_version=11.15
wine_archive="wine-${wine_version}-amd64-wow64.tar.xz"
wine_sha256=e9c307a28575ae01a33610677ddb4708551a9d19d0883af5a255f370df0f7e59
wine_url="https://github.com/Kron4ek/Wine-Builds/releases/download/${wine_version}/${wine_archive}"
target=x86_64-pc-windows-gnu

test_root=$(mktemp -d)
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
export CARGO_HOME="$test_root/cargo"
export RUSTUP_HOME="$test_root/rustup"
export RUSTC="$toolchain_rustc"
export RUSTDOC="$toolchain_rustdoc"
export CARGO_TARGET_DIR="$test_root/target"
export TMPDIR="$test_root/tmp"
export WINEARCH=win64
export WINEPREFIX="$test_root/wineprefix"
export WINEDEBUG=-all
export WINEDLLOVERRIDES='mscoree,mshtml='
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
int main(void) { return 0; }
EOF
x86_64-w64-mingw32-gcc "$test_root/wine-ready.c" -o "$test_root/wine-ready.exe"
"$wine_root/bin/wine" "$test_root/wine-ready.exe"

"$toolchain_cargo" test --locked --workspace --target "$target"
