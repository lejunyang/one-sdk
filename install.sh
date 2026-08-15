#!/bin/sh
set -eu

REPOSITORY=${OSDK_REPOSITORY:-lejunyang/one-sdk}
VERSION=${OSDK_VERSION:-latest}
INSTALL_DIR=${OSDK_BIN_DIR:-"$HOME/.local/bin"}
BASE_URL=${OSDK_DOWNLOAD_BASE_URL:-https://github.com}
TARGET=${OSDK_TARGET:-}
SKIP_VERIFY=${OSDK_SKIP_VERIFY:-0}

usage() {
  cat <<'EOF'
Install osdk from GitHub Releases.

Usage:
  install.sh [options]

Options:
  --version <version>       Release version, with or without "v" (default: latest)
  --install-dir <path>      Binary directory (default: $HOME/.local/bin)
  --repository <owner/repo> GitHub repository (default: lejunyang/one-sdk)
  --base-url <url>          Download base or mirror URL (default: https://github.com)
  --target <target>         Override the detected Rust target triple
  --skip-verify             Skip SHA-256 verification
  -h, --help                Show this help

Environment equivalents:
  OSDK_VERSION, OSDK_BIN_DIR, OSDK_REPOSITORY,
  OSDK_DOWNLOAD_BASE_URL, OSDK_TARGET, OSDK_SKIP_VERIFY
EOF
}

fail() {
  printf 'osdk installer: %s\n' "$*" >&2
  exit 1
}

need_value() {
  [ "$#" -ge 2 ] || fail "$1 requires a value"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      need_value "$@"
      VERSION=$2
      shift 2
      ;;
    --install-dir)
      need_value "$@"
      INSTALL_DIR=$2
      shift 2
      ;;
    --repository)
      need_value "$@"
      REPOSITORY=$2
      shift 2
      ;;
    --base-url)
      need_value "$@"
      BASE_URL=$2
      shift 2
      ;;
    --target)
      need_value "$@"
      TARGET=$2
      shift 2
      ;;
    --skip-verify)
      SKIP_VERIFY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

if [ -z "$TARGET" ]; then
  kernel=$(uname -s)
  machine=$(uname -m)
  case "$kernel:$machine" in
    Linux:x86_64|Linux:amd64)
      TARGET=x86_64-unknown-linux-gnu
      ;;
    Linux:aarch64|Linux:arm64)
      TARGET=aarch64-unknown-linux-gnu
      ;;
    Darwin:x86_64|Darwin:amd64)
      TARGET=x86_64-apple-darwin
      ;;
    Darwin:arm64|Darwin:aarch64)
      TARGET=aarch64-apple-darwin
      ;;
    *)
      fail "unsupported platform: $kernel $machine (use --target to override)"
      ;;
  esac
fi

case "$VERSION" in
  latest)
    release_path=latest/download
    ;;
  v*)
    release_path="download/$VERSION"
    ;;
  *)
    release_path="download/v$VERSION"
    ;;
esac

BASE_URL=${BASE_URL%/}
archive="osdk-$TARGET.tar.gz"
release_url="$BASE_URL/$REPOSITORY/releases/$release_path"
work_dir=$(mktemp -d 2>/dev/null || mktemp -d -t osdk-install)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

printf 'Downloading %s\n' "$release_url/$archive"
download() {
  output=$1
  url=$2
  case "$url" in
    https://*)
      curl --fail --location --proto '=https' --tlsv1.2 --output "$output" "$url"
      ;;
    http://*)
      curl --fail --location --proto '=http' --output "$output" "$url"
      ;;
    *)
      fail "download URL must use http or https: $url"
      ;;
  esac
}
download "$work_dir/$archive" "$release_url/$archive"

if [ "$SKIP_VERIFY" != "1" ]; then
  download "$work_dir/SHA256SUMS" "$release_url/SHA256SUMS"
  expected=$(
    awk -v archive="$archive" '
      $2 == archive || $2 == "*" archive { print $1; exit }
    ' "$work_dir/SHA256SUMS"
  )
  [ -n "$expected" ] || fail "checksum for $archive is missing from SHA256SUMS"

  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work_dir/$archive" | awk '{ print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$work_dir/$archive" | awk '{ print $1 }')
  else
    fail "sha256sum or shasum is required (or pass --skip-verify)"
  fi
  [ "$actual" = "$expected" ] || fail "checksum verification failed for $archive"
fi

mkdir -p "$work_dir/unpack" "$INSTALL_DIR"
tar -xzf "$work_dir/$archive" -C "$work_dir/unpack"
for binary in osdk osdk-shim; do
  [ -f "$work_dir/unpack/$binary" ] || fail "$archive does not contain $binary"
  chmod 755 "$work_dir/unpack/$binary"
  mv "$work_dir/unpack/$binary" "$INSTALL_DIR/$binary"
done

printf 'Installed osdk and osdk-shim to %s\n' "$INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf 'Add %s to PATH to run osdk.\n' "$INSTALL_DIR"
    ;;
esac
