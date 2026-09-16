#!/bin/bash
# The remaining warning: as root, apt drops privileges to the `_apt` user to
# download, and _apt cannot read a directory under /tmp owned by root with
# default permissions. apt says it "performs the download unsandboxed" and
# continues.
#
# That is a real security property being silently given up, not cosmetic noise:
# the sandbox exists so a malicious archive cannot exploit the fetcher as root.
# If osdk is going to use this path, the directory must be readable by _apt so
# the sandbox stays on.
#
# Test: does making the lists directory accessible to _apt remove the warning?

set -uo pipefail
say() { printf '\n=== %s ===\n' "$1"; }

codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"

say "A. reproduce the warning: default permissions under a root-owned tmp dir"
work_a="$(mktemp -d)"
mkdir -p "$work_a/lists/partial" "$work_a/cache/archives/partial" "$work_a/empty-parts"
cat >"$work_a/mirror.list" <<EOF
deb https://mirrors.ustc.edu.cn/ubuntu/ $codename main
EOF
out_a="$(timeout 120 apt-get \
    -o "Dir::Etc::SourceList=$work_a/mirror.list" \
    -o "Dir::Etc::SourceParts=$work_a/empty-parts" \
    -o "Dir::State::Lists=$work_a/lists" \
    -o "Dir::Cache=$work_a/cache" \
    -o "Acquire::Languages=none" \
    update 2>&1)"
if grep -q 'unsandboxed' <<<"$out_a"; then
    echo "  warning present, as expected"
else
    echo "  no warning (unexpected -- the premise of this test is wrong)"
fi
rm -rf "$work_a"

say "B. grant _apt access, and see whether the sandbox stays on"
work_b="$(mktemp -d)"
mkdir -p "$work_b/lists/partial" "$work_b/cache/archives/partial" "$work_b/empty-parts"
cat >"$work_b/mirror.list" <<EOF
deb https://mirrors.ustc.edu.cn/ubuntu/ $codename main
EOF
# _apt needs to traverse the temp dir and write into lists/partial.
chown -R _apt:root "$work_b/lists" "$work_b/cache" 2>/dev/null
chmod 755 "$work_b"
out_b="$(timeout 120 apt-get \
    -o "Dir::Etc::SourceList=$work_b/mirror.list" \
    -o "Dir::Etc::SourceParts=$work_b/empty-parts" \
    -o "Dir::State::Lists=$work_b/lists" \
    -o "Dir::Cache=$work_b/cache" \
    -o "Acquire::Languages=none" \
    update 2>&1)"
echo "  update exit=$?"
if grep -q 'unsandboxed' <<<"$out_b"; then
    echo "  FAIL: still unsandboxed"
    grep 'unsandboxed' <<<"$out_b" | head -2
else
    echo "  OK: sandbox preserved -- no unsandboxed warning"
fi
echo "  fetched lists: $(ls -1 "$work_b/lists" 2>/dev/null | grep -c Packages || true)"
rm -rf "$work_b"

say "C. for comparison: how fast is each mirror right now?"
for m in mirrors.ustc.edu.cn mirrors.tuna.tsinghua.edu.cn mirrors.aliyun.com; do
    url="https://$m/ubuntu/dists/$codename/main/binary-amd64/Packages.gz"
    speed="$(timeout 30 curl -o /dev/null -s -w '%{speed_download}' "$url" 2>/dev/null || echo 0)"
    printf '  %-34s %8.0f KB/s\n' "$m" "$(echo "$speed/1024" | bc -l 2>/dev/null || echo 0)"
done
