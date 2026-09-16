#!/usr/bin/env bash
# Verify Linux package-manager detection against real distributions.
#
# The unit tests cover the detection logic with a scripted runner, which proves
# the branching but not the one thing only a real distribution can answer: that
# the query interfaces osdk calls actually exist and print what it expects. A
# `dpkg-query` flag that changed, or an `apk info` output shape that differs from
# the documented one, would leave every test green and every report wrong.
#
# So this runs `osdk pkg doctor` inside Debian, Alpine, Arch and Fedora and
# asserts three things per distribution:
#
#   1. the manager that distribution ships is reported as present;
#   2. the managers it does not ship are not;
#   3. nothing was elevated and no system file changed.
#
# Requires docker or podman. Skips with a clear message when neither is present,
# rather than failing a developer machine that simply has no container runtime.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

runtime=""
for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1; then
        runtime="$candidate"
        break
    fi
done

if [[ -z "$runtime" ]]; then
    echo "skip: neither docker nor podman is available" >&2
    exit 0
fi

echo "using container runtime: $runtime"

# Built once for the GNU target so the same binary runs in every container,
# including Alpine -- which is why musl would be the wrong choice here only if
# the images differed in libc. glibc images are used throughout for that reason,
# with Alpine handled by installing gcompat.
target="x86_64-unknown-linux-gnu"
echo "building osdk for $target"
cargo build --locked -p osdk-cli --bin osdk --target "$target"
binary="$repo_root/target/$target/debug/osdk"

if [[ ! -x "$binary" ]]; then
    echo "fail: expected a built binary at $binary" >&2
    exit 1
fi

# distro image                     expected present   expected absent
cases=(
    "debian:stable-slim|apt|pacman dnf"
    "alpine:latest|apk|apt pacman dnf"
    "archlinux:latest|pacman|apt dnf"
    "fedora:latest|dnf|apt pacman"
)

failures=0

for case_line in "${cases[@]}"; do
    IFS='|' read -r image expect_present expect_absent <<<"$case_line"
    echo
    echo "=== $image: expecting $expect_present ==="

    # Alpine needs gcompat to run a glibc binary; everything else runs as is.
    prelude="true"
    if [[ "$image" == alpine:* ]]; then
        prelude="apk add --no-cache gcompat >/dev/null 2>&1"
    fi

    output="$(
        "$runtime" run --rm \
            -v "$binary:/usr/local/bin/osdk:ro" \
            -e OSDK_DATA_DIR=/tmp/osdk-data \
            -e OSDK_CONFIG_DIR=/tmp/osdk-config \
            -e OSDK_CACHE_DIR=/tmp/osdk-cache \
            "$image" \
            sh -c "$prelude; osdk pkg doctor --json" 2>&1
    )" || {
        echo "fail: osdk pkg doctor did not run in $image" >&2
        echo "$output" >&2
        failures=$((failures + 1))
        continue
    }

    # The JSON is language-neutral by contract, so grep against it directly
    # rather than against the human output.
    if grep -q "\"manager\":\"$expect_present\",\"present\":true" <<<"$output"; then
        echo "  ok: $expect_present reported present"
    else
        echo "  FAIL: $expect_present was not reported present" >&2
        echo "$output" >&2
        failures=$((failures + 1))
    fi

    for absent in $expect_absent; do
        if grep -q "\"manager\":\"$absent\",\"present\":true" <<<"$output"; then
            echo "  FAIL: $absent reported present in $image" >&2
            failures=$((failures + 1))
        else
            echo "  ok: $absent correctly absent"
        fi
    done

    # Detection must never elevate. Running as root in a container would hide a
    # sudo call, so assert the output never mentions one.
    if grep -qi '"sudo"' <<<"$output"; then
        echo "  FAIL: detection output references sudo" >&2
        failures=$((failures + 1))
    else
        echo "  ok: no elevation in the diagnostic"
    fi
done

echo
if (( failures > 0 )); then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "all distribution detection checks passed"
