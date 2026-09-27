#!/usr/bin/env bash
# Exercise osdk's native container adapters against the CLIs installed on the
# host. Scripted tests cannot catch argument or output drift in those CLIs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
smoke_image="${OSDK_CONTAINER_SMOKE_IMAGE:-mirror.gcr.io/library/alpine@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc}"
remove_smoke_image=false

cleanup() {
    if [[ "$remove_smoke_image" == true && -x "$temporary/native-bin/docker" ]]; then
        "$temporary/native-bin/docker" image rm "$smoke_image" >/dev/null 2>&1 || true
    fi
    rm -rf "$temporary"
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    echo "skip: docker is not installed"
    exit 0
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "fail: python3 is required to validate container JSON contracts" >&2
    exit 1
fi

mkdir -p "$temporary/native-bin" "$temporary/home" "$temporary/data" \
    "$temporary/cache" "$temporary/config"

native_docker="$(command -v docker)"
if "$native_docker" info >/dev/null 2>&1; then
    cat >"$temporary/native-bin/docker" <<EOF
#!/usr/bin/env bash
exec "$native_docker" "\$@"
EOF
elif command -v sudo >/dev/null 2>&1 && sudo -n "$native_docker" info >/dev/null 2>&1; then
    cat >"$temporary/native-bin/docker" <<EOF
#!/usr/bin/env bash
exec sudo -n "$native_docker" "\$@"
EOF
else
    echo "fail: docker is installed but its daemon is not usable by this test" >&2
    exit 1
fi
chmod +x "$temporary/native-bin/docker"

if [[ -n "${OSDK_CONTAINER_SMOKE_BIN:-}" ]]; then
    osdk_binary="$OSDK_CONTAINER_SMOKE_BIN"
else
    cargo build --locked -p osdk-cli --bin osdk
    target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
    if [[ "$target_dir" != /* ]]; then
        target_dir="$repo_root/$target_dir"
    fi
    osdk_binary="$target_dir/debug/osdk"
fi
if [[ ! -x "$osdk_binary" ]]; then
    echo "fail: expected an osdk binary at $osdk_binary" >&2
    exit 1
fi

export HOME="$temporary/home"
export OSDK_DATA_DIR="$temporary/data"
export OSDK_CACHE_DIR="$temporary/cache"
export OSDK_CONFIG_DIR="$temporary/config"
export PATH="$temporary/native-bin:$PATH"
cd "$temporary"

assert_json() {
    local expression="$1"
    python3 -c "import json, sys; value = json.load(sys.stdin); assert $expression, value"
}

echo "checking Docker and Buildx discovery"
docker_report="$($osdk_binary container doctor --runtime docker --json)"
assert_json 'value["runtime"]["status"] == "healthy"' <<<"$docker_report"
assert_json 'value["builder"]["status"] == "healthy"' <<<"$docker_report"

echo "checking native cache contracts"
docker_cache="$($osdk_binary container cache status --runtime docker --json)"
assert_json 'value["status"] == "available"' <<<"$docker_cache"
buildkit_cache="$($osdk_binary container cache status --runtime buildkit --json)"
assert_json 'value["status"] == "available"' <<<"$buildkit_cache"

echo "checking read-only plans and previews"
docker_plan="$($osdk_binary container mirrors plan docker.io --runtime docker \
    --native-config "$temporary/daemon.json" --json)"
assert_json 'value["applicability"] == "ready"' <<<"$docker_plan"
docker_prune="$($osdk_binary container prune --runtime docker --scope images --json)"
assert_json 'value["preview_id"].startswith("sha256:")' <<<"$docker_prune"
buildkit_prune="$($osdk_binary container prune --runtime buildkit --scope build-cache --json)"
assert_json 'value["preview_id"].startswith("sha256:")' <<<"$buildkit_prune"
test ! -e "$temporary/daemon.json"

echo "checking a pinned OCI registry and Docker pull"
registry="${smoke_image%%/*}"
registry_report="$($osdk_binary container registry test "$registry" \
    --image "$smoke_image" --platform linux/amd64 --json)"
assert_json 'value["status"] == "healthy"' <<<"$registry_report"
if "$temporary/native-bin/docker" image inspect "$smoke_image" >/dev/null 2>&1; then
    :
else
    remove_smoke_image=true
fi
$osdk_binary container pull "$smoke_image" --runtime docker --platform linux/amd64 >/dev/null
$temporary/native-bin/docker image inspect "$smoke_image" >/dev/null
container_output="$($temporary/native-bin/docker run --rm --network none "$smoke_image" \
    /bin/sh -c 'printf osdk-container-smoke-ok')"
test "$container_output" = "osdk-container-smoke-ok"

if command -v ctr >/dev/null 2>&1 && command -v containerd >/dev/null 2>&1; then
    native_ctr="$(command -v ctr)"
    native_containerd="$(command -v containerd)"
    if "$native_ctr" --address /run/containerd/containerd.sock --namespace default version \
        >/dev/null 2>&1; then
        ctr_prefix=()
    elif command -v sudo >/dev/null 2>&1 \
        && sudo -n "$native_ctr" --address /run/containerd/containerd.sock \
            --namespace default version >/dev/null 2>&1; then
        ctr_prefix=(sudo -n)
    else
        echo "fail: ctr and containerd are installed but the daemon is not usable" >&2
        exit 1
    fi
    cat >"$temporary/native-bin/ctr" <<EOF
#!/usr/bin/env bash
exec ${ctr_prefix[*]} "$native_ctr" "\$@"
EOF
    cat >"$temporary/native-bin/containerd" <<EOF
#!/usr/bin/env bash
exec ${ctr_prefix[*]} "$native_containerd" "\$@"
EOF
    chmod +x "$temporary/native-bin/ctr" "$temporary/native-bin/containerd"

    echo "checking containerd's native address contract"
    containerd_report="$($osdk_binary container doctor --runtime containerd --json)"
    assert_json 'value["runtime"]["status"] == "healthy"' <<<"$containerd_report"
else
    echo "skip: ctr/containerd pair is not installed"
fi

echo "container smoke passed"
