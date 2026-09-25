#!/usr/bin/env bash
# Retry only the observed GitHub-hosted runner failure: bash starts the script,
# it exits 2 in under a second, and neither stdout nor stderr contains one byte.
# Any output means the installer smoke actually started, so preserve its first
# result verbatim rather than hiding a deterministic failure behind a retry.
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer_smoke="${OSDK_INSTALLER_SMOKE_SCRIPT:-$repo_root/scripts/test-installers.sh}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

set +e
"$installer_smoke" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

if [[ $status -eq 2 && ! -s "$log" ]]; then
    echo "installer smoke exited 2 without output; retrying once" >&2
    "$installer_smoke"
    exit $?
fi

exit "$status"
