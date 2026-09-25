#!/usr/bin/env bash
# Retry only the observed GitHub-hosted runner failure: bash starts the script,
# it exits 2 in under a second, and neither stdout nor stderr contains one byte.
# Any output means the installer smoke actually started, so preserve its first
# result verbatim rather than hiding a deterministic failure behind a retry. If
# the exact silent failure repeats, xtrace identifies the startup command that
# returned 2 instead of leaving another empty CI log.
set -u

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer_smoke="${OSDK_INSTALLER_SMOKE_SCRIPT:-$repo_root/scripts/test-installers.sh}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT

set +e
bash "$installer_smoke" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

if [[ $status -eq 2 && ! -s "$log" ]]; then
    echo "installer smoke exited 2 without output; retrying once with xtrace" >&2
    exec 3>&2
    BASH_XTRACEFD=3 PS4='+ ${BASH_SOURCE}:${LINENO}: ' bash -x "$installer_smoke"
    status=$?
    exec 3>&-
    exit "$status"
fi

exit "$status"
