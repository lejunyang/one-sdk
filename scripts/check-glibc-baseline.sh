#!/usr/bin/env bash
set -Eeuo pipefail

# GNU/Linux release archives promise to run on glibc 2.31 or newer. Keep this
# check next to the build rather than trusting the builder image name: a runner,
# container, compiler, or linker change can otherwise raise the ABI floor while
# every build and unit test stays green.
baseline=${OSDK_GLIBC_BASELINE:-2.31}
release_dir=${OSDK_RELEASE_BIN_DIR:-}

if [[ -z "$release_dir" ]]; then
  host_target=$(rustc -vV | sed -n 's/^host: //p')
  [[ -n "$host_target" ]] || {
    printf 'Could not determine the Rust host target.\n' >&2
    exit 1
  }
  release_dir="${CARGO_TARGET_DIR:-target}/$host_target/release"
fi

command -v readelf >/dev/null 2>&1 || {
  printf 'readelf is required to check the glibc baseline.\n' >&2
  exit 1
}

version_exceeds() {
  local actual=$1
  local allowed=$2
  [[ $(printf '%s\n' "$actual" "$allowed" | LC_ALL=C sort -V | tail -n 1) != "$allowed" ]]
}

for name in osdk osdk-shim; do
  binary="$release_dir/$name"
  [[ -f "$binary" ]] || {
    printf 'Release binary is missing: %s\n' "$binary" >&2
    exit 1
  }

  versions=$(
    readelf --version-info "$binary" |
      sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\).*/\1/p' |
      LC_ALL=C sort -Vu
  )
  [[ -n "$versions" ]] || {
    printf '%s has no readable GLIBC symbol versions.\n' "$binary" >&2
    exit 1
  }
  maximum=$(printf '%s\n' "$versions" | tail -n 1)
  if version_exceeds "$maximum" "$baseline"; then
    printf '%s requires GLIBC_%s, exceeding the supported GLIBC_%s baseline.\n' \
      "$binary" "$maximum" "$baseline" >&2
    exit 1
  fi
  printf '%s: maximum required symbol version GLIBC_%s (baseline GLIBC_%s)\n' \
    "$binary" "$maximum" "$baseline"
done
