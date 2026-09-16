#!/bin/bash
# Does apt accept an alternative source list without touching /etc?
#
# This decides the whole shape of mirror acceleration on Linux. If apt can be
# pointed at a temporary source file per invocation, osdk can accelerate its own
# calls with no system change and no confirmation -- the same L2/L3 split that
# winget forced, but with a working L2 this time. If it cannot, the only route is
# rewriting /etc/apt/sources.list, which is destructive and format-dependent.
#
# Read-only with respect to system state: everything is written under a temp dir,
# and `apt-get` is only ever asked to *print* what it would do (-s / download-only
# into a temp dir), never to install.

set -uo pipefail

say() { printf '\n=== %s ===\n' "$1"; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

say "baseline: what does the system currently use?"
if [ -f /etc/apt/sources.list ]; then
    grep -v '^\s*#' /etc/apt/sources.list | grep -v '^\s*$' | head -3
fi
ls /etc/apt/sources.list.d/ 2>/dev/null | head -5
echo "ubuntu codename: $(. /etc/os-release && echo "$VERSION_CODENAME")"

codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"

say "1. can Dir::Etc::SourceList point at a file outside /etc?"
cat >"$work/mirror.list" <<EOF
deb https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ $codename main restricted universe multiverse
EOF
cat "$work/mirror.list"

# Everything apt writes must land in the temp dir, or this "read-only" probe is a
# lie: without these overrides apt would update /var/lib/apt/lists in place.
mkdir -p "$work/lists/partial" "$work/cache/archives/partial" "$work/state"
apt_opts=(
    -o "Dir::Etc::SourceList=$work/mirror.list"
    -o "Dir::Etc::SourceParts=$work/empty-parts"
    -o "Dir::State::Lists=$work/lists"
    -o "Dir::Cache::Archives=$work/cache/archives"
    -o "Dir::State=$work/state"
    -o "Acquire::Languages=none"
)
mkdir -p "$work/empty-parts"

say "2. does apt-get update read ONLY that file?"
timeout 120 apt-get "${apt_opts[@]}" update 2>&1 | tail -8
echo "update exit=$?"

say "3. did anything land in the system lists directory?"
before_count=$(ls -1 /var/lib/apt/lists/ 2>/dev/null | wc -l)
echo "system /var/lib/apt/lists entries: $before_count"
echo "temp lists entries: $(ls -1 "$work/lists" 2>/dev/null | wc -l)"

say "4. can a package be resolved through the temp source? (simulate only)"
timeout 60 apt-get "${apt_opts[@]}" install -s curl 2>&1 | tail -6
echo "simulate exit=$?"

say "5. confirm /etc/apt was never written"
find /etc/apt -newermt '-5 minutes' 2>/dev/null | head -5
echo "(no output above means /etc/apt is untouched)"
