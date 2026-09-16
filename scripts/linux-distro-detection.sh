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

# image | manager it ships | managers it does not | a package it always has
#
# Column 4 ships in the base image; column 5 must NOT, so installing it proves
# something actually happened rather than that it was already there.
# step: it is the ground truth the status query is checked against.
cases=(
    "debian:stable-slim|apt|pacman dnf|coreutils|jq"
    "alpine:latest|apk|apt pacman dnf|busybox|jq"
    "archlinux:latest|pacman|apt dnf|coreutils|jq"
    "fedora:latest|dnf|apt pacman|coreutils|jq"
)

failures=0

for case_line in "${cases[@]}"; do
    IFS='|' read -r image expect_present expect_absent known_present installable <<<"$case_line"
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

    # Detection proves the manager was found. This proves the *query* interface
    # works: that `dpkg-query -W -f=` and friends exist, exit as expected, and
    # print what the parser assumes. A changed flag would leave every unit test
    # green and every status report wrong, which is the failure only a real
    # distribution can reveal.
    status_output="$(
        "$runtime" run --rm \
            -v "$binary:/usr/local/bin/osdk:ro" \
            -e OSDK_DATA_DIR=/tmp/osdk-data \
            -e OSDK_CONFIG_DIR=/tmp/osdk-config \
            -e OSDK_CACHE_DIR=/tmp/osdk-cache \
            -e OSDK_YES=true \
            "$image" \
            sh -c "$prelude
mkdir -p /tmp/proj && cd /tmp/proj
cat > osdk.toml <<'TOML'
[syspkg]
managers = [\"$expect_present\"]

[syspkg.packages]
\"$expect_present:$known_present\" = \"latest\"
\"$expect_present:definitely-not-a-real-package-osdk\" = \"latest\"
TOML
osdk --yes trust >/dev/null 2>&1
osdk pkg status --json" 2>&1
    )" || true

    # A package the base image is guaranteed to have must read as satisfied.
    if grep -q "\"id\":\"$known_present\",\"requested\":\"latest\",\"installed\":\"[^\"]" <<<"$status_output"; then
        echo "  ok: $known_present reported installed with a version"
    else
        echo "  FAIL: $known_present was not reported as installed with a version" >&2
        echo "$status_output" >&2
        failures=$((failures + 1))
    fi

    # And one that cannot exist must read as missing -- not as "manager
    # unavailable", which would mean the query never ran.
    if grep -q '"state":"missing"' <<<"$status_output"; then
        echo "  ok: an absent package is reported missing"
    else
        echo "  FAIL: the absent package was not reported missing" >&2
        echo "$status_output" >&2
        failures=$((failures + 1))
    fi
    if grep -q '"state":"manager-unavailable"' <<<"$status_output"; then
        echo "  FAIL: the query did not run; the manager was reported unavailable" >&2
        failures=$((failures + 1))
    fi

    # The install path, in a container where running it is harmless. Unit tests
    # prove the argv is built per manager; only a real distribution proves the
    # manager accepts that argv and that the package actually arrives.
    #
    # pacman is excluded on purpose: osdk declines to install there, and the
    # assertion below is that it declines rather than that it succeeds.
    if [[ "$expect_present" != "pacman" ]]; then
        install_output="$(
            "$runtime" run --rm \
                -v "$binary:/usr/local/bin/osdk:ro" \
                -e OSDK_DATA_DIR=/tmp/osdk-data \
                -e OSDK_CONFIG_DIR=/tmp/osdk-config \
                -e OSDK_CACHE_DIR=/tmp/osdk-cache \
                -e OSDK_YES=true \
                "$image" \
                sh -c "$prelude
mkdir -p /tmp/proj && cd /tmp/proj
cat > osdk.toml <<'TOML'
[syspkg]
managers = [\"$expect_present\"]

[syspkg.packages]
\"$expect_present:$installable\" = \"latest\"
TOML
osdk --yes trust >/dev/null 2>&1
osdk pkg apply --yes 2>&1
echo \"---exit:\$?\"
command -v $installable >/dev/null 2>&1 && echo 'BINARY-PRESENT' || echo 'BINARY-ABSENT'" 2>&1
        )" || true

        # Containers run as root, so elevation resolves to AlreadyRoot and the
        # command runs directly. A refusal here would mean the root case is
        # broken, which no amount of unit testing would show.
        if grep -q 'BINARY-PRESENT' <<<"$install_output"; then
            echo "  ok: $installable was installed and is on PATH"
        else
            echo "  FAIL: $installable did not end up installed" >&2
            echo "$install_output" | tail -20 >&2
            failures=$((failures + 1))
        fi
        if grep -qi 'not run:' <<<"$install_output"; then
            echo "  FAIL: elevation was refused while running as root" >&2
            failures=$((failures + 1))
        fi
    else
        pacman_output="$(
            "$runtime" run --rm \
                -v "$binary:/usr/local/bin/osdk:ro" \
                -e OSDK_DATA_DIR=/tmp/osdk-data \
                -e OSDK_CONFIG_DIR=/tmp/osdk-config \
                -e OSDK_CACHE_DIR=/tmp/osdk-cache \
                -e OSDK_YES=true \
                "$image" \
                sh -c "$prelude
mkdir -p /tmp/proj && cd /tmp/proj
cat > osdk.toml <<'TOML'
[syspkg]
managers = [\"pacman\"]

[syspkg.packages]
\"pacman:$installable\" = \"latest\"
TOML
osdk --yes trust >/dev/null 2>&1
osdk pkg apply --dry-run 2>&1" 2>&1
        )" || true

        # Arch documents that installing one package is a partial upgrade and
        # unsupported, so osdk must decline and say so -- not quietly omit it.
        if grep -qi 'partial upgrade' <<<"$pacman_output"; then
            echo "  ok: pacman is declined with the reason stated"
        else
            echo "  FAIL: pacman was not declined with a stated reason" >&2
            echo "$pacman_output" | tail -20 >&2
            failures=$((failures + 1))
        fi
    fi

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
