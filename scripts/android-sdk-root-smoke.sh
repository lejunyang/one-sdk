#!/usr/bin/env bash
# Verify the Android SDK root bridge on Linux / macOS: install a package, confirm
# the link, uninstall, and confirm neither a dangling link nor leftover
# scaffolding is left behind.
#
# The bridge links each versioned install directory into the single layout
# Google's tools insist on. Windows uses an NTFS junction, unix a symlink, and the
# removal call differs (`rmdir` fails on a unix symlink with ENOTDIR), so this
# path needs checking on a real unix host rather than only compiling there.
#
# Everything happens under a throwaway OSDK_* root; your own installation is never
# touched. Downloads a small package, so it needs network access.
set -Eeuo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

osdk=${OSDK_BIN:-}
if [[ -z $osdk ]]; then
  for candidate in "$repo_root/target/release/osdk" "$repo_root/target/debug/osdk"; do
    [[ -x $candidate ]] && osdk=$candidate && break
  done
fi
if [[ -z $osdk || ! -x $osdk ]]; then
  echo "no osdk binary found; run 'cargo build --release' or set OSDK_BIN" >&2
  exit 1
fi

test_root=$(mktemp -d)
cleanup() { rm -rf "$test_root"; }
trap cleanup EXIT

export OSDK_DATA_DIR="$test_root/data"
export OSDK_CACHE_DIR="$test_root/cache"
export OSDK_CONFIG_DIR="$test_root/config"
export OSDK_YES=true
sdk="$OSDK_DATA_DIR/installs/android-sdk"

fail=0
check() {
  local label=$1 want=$2 got=$3
  if [[ $want == "$got" ]]; then
    printf '  ok    %-52s %s\n' "$label" "$got"
  else
    printf '  FAIL  %-52s want=%s got=%s\n' "$label" "$want" "$got"
    fail=1
  fi
}
is_link() { [[ -L $1 ]] && echo yes || echo no; }
resolves() { [[ -e $1 ]] && echo yes || echo no; }
exists()   { [[ -e $1 || -L $1 ]] && echo yes || echo no; }

echo "osdk:     $osdk"
echo "test root: $test_root"
echo

echo "== install a versioned package (build-tools nests one level) =="
"$osdk" install android-build-tools@37.0.0 -o accept-licenses=true >/dev/null
link="$sdk/build-tools/37.0.0"
check "the bridge created a link"            yes "$(is_link "$link")"
check "the link resolves to the payload"     yes "$(resolves "$link")"
check "aapt2 reachable through the link"     yes "$([[ -e $link/aapt2 ]] && echo yes || echo no)"
check "package.xml written for Google tools" yes "$([[ -f $OSDK_DATA_DIR/installs/android-build-tools/37.0.0/package.xml ]] && echo yes || echo no)"
echo

echo "== a foreign directory at the target path is never clobbered =="
foreign="$sdk/platforms/android-99"
mkdir -p "$foreign" && echo precious > "$foreign/keep.txt"
"$osdk" android sdk-root repair >/dev/null 2>&1 || true
check "foreign directory kept"               yes "$([[ -f $foreign/keep.txt ]] && echo yes || echo no)"
check "foreign directory is not a link"      no  "$(is_link "$foreign")"
echo

echo "== uninstall removes the link and the scaffolding it empties =="
"$osdk" uninstall android-build-tools@37.0.0 --yes >/dev/null
check "link gone (not merely unresolvable)"  no  "$(exists "$link")"
check "empty 'build-tools' dir pruned"       no  "$(exists "$sdk/build-tools")"
check "the SDK root itself survives"         yes "$([[ -d $sdk ]] && echo yes || echo no)"
echo

echo "== a link left behind out-of-band is reported, then pruned =="
"$osdk" install android-build-tools@37.0.0 -o accept-licenses=true >/dev/null
# Delete the payload behind osdk's back: this is the state an older osdk, or a
# manual rm, would leave.
rm -rf "$OSDK_DATA_DIR/installs/android-build-tools"
check "link still present"                   yes "$(is_link "$link")"
check "but it no longer resolves"            no  "$(resolves "$link")"
show=$("$osdk" android sdk-root show 2>&1 || true)
check "show reports it"                      yes "$(grep -q 'dangling' <<<"$show" && echo yes || echo no)"
check "show did not mutate anything"         yes "$(is_link "$link")"
"$osdk" android sdk-root repair >/dev/null
check "repair removed it"                    no  "$(exists "$link")"
again=$("$osdk" android sdk-root repair 2>&1)
check "repair is idempotent"                 yes "$(grep -q 'no dangling links' <<<"$again" && echo yes || echo no)"
echo

if (( fail )); then
  echo "FAILED"
  exit 1
fi
echo "all checks passed"
