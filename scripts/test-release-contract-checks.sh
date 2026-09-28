#!/usr/bin/env bash
set -Eeuo pipefail

# Exercise the release ELF contract checkers (check-glibc-baseline.sh and
# check-musl-static.sh) in regular CI, so a broken checker is red before a
# release rather than failing only inside publish.
#
# Only the failure branches are testable here. The positive branches need the
# real release artifacts -- binaries built against glibc 2.31 or fully static
# musl binaries -- which only exist in the publish workflow. The failure
# branches still execute every line of the scripts apart from the final
# success report.

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

test_root=$(mktemp -d)
cleanup() {
  rm -rf "$test_root"
}
trap cleanup EXIT

command -v readelf >/dev/null 2>&1 || {
  printf 'readelf is required to test the release contract checker scripts.\n' >&2
  exit 1
}

glibc="$repo_root/scripts/check-glibc-baseline.sh"
musl="$repo_root/scripts/check-musl-static.sh"
bash -n "$glibc"
bash -n "$musl"

# Run a command that is expected to fail, and assert its output names the
# failure reason the publish workflow greps for.
expect_failure() {
  local log=$1 expected=$2
  shift 2
  if "$@" >"$log" 2>&1; then
    printf 'expected the checker to fail, but it passed: %s\n' "$*" >&2
    exit 1
  fi
  if ! grep -F -- "$expected" "$log" >/dev/null; then
    printf 'checker output did not contain: %s\nactual output:\n' "$expected" >&2
    cat "$log" >&2
    exit 1
  fi
}

# Fixture "release dir": copies of the runner's dynamically linked /bin/sh,
# a real ELF with imported GLIBC symbol versions.
dynamic="$test_root/dynamic-binaries"
mkdir -p "$dynamic"
cp /bin/sh "$dynamic/osdk"
cp /bin/sh "$dynamic/osdk-shim"
missing="$test_root/empty"
mkdir -p "$missing"

# An impossible baseline must be rejected against real binaries, with the
# exact message publish's negative control greps for.
expect_failure "$test_root/glibc-impossible-baseline.log" \
  'exceeding the supported GLIBC_0.0 baseline' \
  env OSDK_GLIBC_BASELINE=0.0 OSDK_RELEASE_BIN_DIR="$dynamic" bash "$glibc"

# A release dir without the binaries must be reported as missing.
expect_failure "$test_root/glibc-missing.log" 'Release binary is missing' \
  env OSDK_RELEASE_BIN_DIR="$missing" bash "$glibc"

# Dynamic binaries must be rejected as not-static-musl.
expect_failure "$test_root/musl-dynamic.log" 'is not a static musl binary' \
  env OSDK_RELEASE_BIN_DIR="$dynamic" bash "$musl"

expect_failure "$test_root/musl-missing.log" 'Release binary is missing' \
  env OSDK_RELEASE_BIN_DIR="$missing" bash "$musl"

printf 'release contract checker scripts: all failure branches behave as expected\n'
