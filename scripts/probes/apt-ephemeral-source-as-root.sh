#!/bin/bash
# Round 2: close the gap the first probe exposed.
#
# The first run left two warnings -- apt tried to unlink
# /var/cache/apt/pkgcache.bin and was denied. Non-root made that harmless, but as
# root it would have written a system path, so the "touches nothing" claim was
# only true by accident of privilege. Add Dir::Cache and re-verify AS ROOT, which
# is the case that matters: containers and CI run as root, and that is exactly
# where osdk would use this.

set -uo pipefail

say() { printf '\n=== %s ===\n' "$1"; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"
echo "running as uid=$(id -u), codename=$codename"

cat >"$work/mirror.list" <<EOF
deb https://mirrors.ustc.edu.cn/ubuntu/ $codename main restricted universe multiverse
EOF

mkdir -p "$work/lists/partial" "$work/cache/archives/partial" "$work/state" "$work/empty-parts"

# Dir::Cache was the missing one. Everything apt writes now lands in $work.
apt_opts=(
    -o "Dir::Etc::SourceList=$work/mirror.list"
    -o "Dir::Etc::SourceParts=$work/empty-parts"
    -o "Dir::State::Lists=$work/lists"
    -o "Dir::State=$work/state"
    -o "Dir::Cache=$work/cache"
    -o "Dir::Cache::Archives=$work/cache/archives"
    -o "Acquire::Languages=none"
)

say "record system state before"
sys_cache_before="$(md5sum /var/cache/apt/pkgcache.bin 2>/dev/null | cut -d' ' -f1)"
sys_lists_before="$(ls -1 /var/lib/apt/lists/ 2>/dev/null | wc -l)"
echo "pkgcache.bin md5: ${sys_cache_before:-absent}"
echo "system lists entries: $sys_lists_before"

say "update through the temp source (warnings should be gone now)"
timeout 180 apt-get "${apt_opts[@]}" update 2>&1 | tail -6
echo "update exit=$?"

say "system state after: nothing may have changed"
sys_cache_after="$(md5sum /var/cache/apt/pkgcache.bin 2>/dev/null | cut -d' ' -f1)"
sys_lists_after="$(ls -1 /var/lib/apt/lists/ 2>/dev/null | wc -l)"
echo "pkgcache.bin md5: ${sys_cache_after:-absent}"
echo "system lists entries: $sys_lists_after"

if [ "${sys_cache_before:-absent}" = "${sys_cache_after:-absent}" ]; then
    echo "OK: system package cache unchanged"
else
    echo "FAIL: system package cache was modified"
fi
if [ "$sys_lists_before" = "$sys_lists_after" ]; then
    echo "OK: system lists unchanged"
else
    echo "FAIL: system lists changed ($sys_lists_before -> $sys_lists_after)"
fi

say "can an install be simulated through the temp source?"
timeout 60 apt-get "${apt_opts[@]}" install -s jq 2>&1 | tail -5

say "does /etc/apt remain untouched?"
find /etc/apt -newermt '-5 minutes' 2>/dev/null | head -5
echo "(no output above means untouched)"
