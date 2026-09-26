#!/usr/bin/env bash
set -Eeuo pipefail

release_dir=${OSDK_RELEASE_BIN_DIR:-}
if [[ -z "$release_dir" ]]; then
  host_arch=$(uname -m)
  case "$host_arch" in
    x86_64|amd64) target=x86_64-unknown-linux-musl ;;
    aarch64|arm64) target=aarch64-unknown-linux-musl ;;
    *)
      printf 'Unsupported musl release architecture: %s\n' "$host_arch" >&2
      exit 1
      ;;
  esac
  release_dir="${CARGO_TARGET_DIR:-target}/$target/release"
fi

command -v readelf >/dev/null 2>&1 || {
  printf 'readelf is required to check static musl release binaries.\n' >&2
  exit 1
}

for name in osdk osdk-shim; do
  binary="$release_dir/$name"
  [[ -f "$binary" ]] || {
    printf 'Release binary is missing: %s\n' "$binary" >&2
    exit 1
  }
  if readelf --program-headers "$binary" | grep -q 'INTERP'; then
    printf '%s has an ELF interpreter and is not a static musl binary.\n' \
      "$binary" >&2
    exit 1
  fi
  if readelf --dynamic "$binary" 2>/dev/null | grep -q '(NEEDED)'; then
    printf '%s has dynamic library dependencies and is not fully static.\n' \
      "$binary" >&2
    exit 1
  fi
  printf '%s: static ELF with no interpreter or NEEDED entries\n' "$binary"
done
