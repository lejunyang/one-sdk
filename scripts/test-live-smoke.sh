#!/usr/bin/env bash
set -Eeuo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)

test_root=$(mktemp -d)
cleanup() {
  rm -rf "$test_root"
}
trap cleanup EXIT

fake_osdk="$test_root/osdk"

cat > "$fake_osdk" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

state_dir=${OSDK_TEST_STATE:?}
args=("$@")
if [[ ${args[0]:-} == --quiet ]]; then
  args=("${args[@]:1}")
fi
if [[ ${args[0]:-} == --yes ]]; then
  args=("${args[@]:1}")
fi

case ${args[0]:-} in
  list-remote)
    printf '1.0.0\n'
    ;;
  install)
    spec=${args[1]}
    tool=${spec%@*}
    mkdir -p "$OSDK_INSTALL_DIR/node/20.0.0" "$OSDK_INSTALL_DIR/node/22.0.0" "$OSDK_INSTALL_DIR/$tool/1.0.0"
    : > "$OSDK_INSTALL_DIR/node/20.0.0/.osdk-complete"
    : > "$OSDK_INSTALL_DIR/node/22.0.0/.osdk-complete"
    : > "$OSDK_INSTALL_DIR/$tool/1.0.0/.osdk-complete"
    ;;
  lock)
    printf 'schema = 4\n' > osdk.lock
    ;;
  exec)
    printf '1.0.0\n'
    ;;
  use)
    if [[ ${args[1]:-} != --global || ${args[2]:-} != npm:prettier@3.6.2 ]]; then
      printf 'unexpected fake global use: %s\n' "${args[*]}" >&2
      exit 2
    fi
    mkdir -p \
      "$OSDK_INSTALL_DIR/node/26.10.0" \
      "$OSDK_INSTALL_DIR/npm/12.2.0" \
      "$OSDK_INSTALL_DIR/npm-global/prettier/3.6.2/fixture" \
      "$OSDK_DATA_DIR/shims"
    : > "$OSDK_INSTALL_DIR/node/26.10.0/.osdk-complete"
    : > "$OSDK_INSTALL_DIR/npm/12.2.0/.osdk-complete"
    : > "$OSDK_INSTALL_DIR/npm-global/prettier/3.6.2/fixture/.osdk-complete"
    cat > "$OSDK_DATA_DIR/shims/prettier" <<'SHIM'
#!/usr/bin/env bash
printf '3.6.2\n'
SHIM
    chmod +x "$OSDK_DATA_DIR/shims/prettier"
    ;;
  where)
    printf '%s\n' "$OSDK_INSTALL_DIR/npm-global/prettier/3.6.2/fixture"
    ;;
  list)
    tool=${args[1]}
    printf '%s:\n' "$tool"
    if [[ -d "$OSDK_INSTALL_DIR/$tool" ]]; then
      find "$OSDK_INSTALL_DIR/$tool" -mindepth 1 -maxdepth 1 -type d -printf '  %f\n' | sort
    fi
    ;;
  uninstall)
    if [[ ${args[1]:-} == --global ]]; then
      spec=${args[2]}
      printf '%s\n' "global:$spec" >> "$state_dir/uninstalled.log"
      rm -rf "$OSDK_INSTALL_DIR/npm-global/prettier/3.6.2"
      rm -f "$OSDK_DATA_DIR/shims/prettier"
    else
      spec=${args[1]}
      tool=${spec%@*}
      version=${spec##*@}
      printf '%s@%s\n' "$tool" "$version" >> "$state_dir/uninstalled.log"
      rm -rf "$OSDK_INSTALL_DIR/$tool/$version"
    fi
    ;;
  *)
    printf 'unexpected fake osdk invocation: %s\n' "${args[*]}" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$fake_osdk"
cp "$fake_osdk" "$test_root/osdk-shim"

export OSDK_TEST_STATE="$test_root"
export LIVE_SMOKE_ROOT="$test_root/smoke"
export LIVE_SMOKE_COMMAND_TIMEOUT=30s
bash "$repo_root/scripts/live-smoke/run.sh" yarn "$fake_osdk" >/dev/null

for expected in node@20.0.0 node@22.0.0 yarn@1.0.0; do
  grep -Fxq "$expected" "$test_root/uninstalled.log" || {
    printf 'live smoke did not clean %s\n' "$expected" >&2
    exit 1
  }
done

: > "$test_root/uninstalled.log"
rm -rf "$LIVE_SMOKE_ROOT"
bash "$repo_root/scripts/live-smoke/run.sh" pnpm "$fake_osdk" >/dev/null

for expected in node@20.0.0 node@22.0.0 pnpm@1.0.0; do
  grep -Fxq "$expected" "$test_root/uninstalled.log" || {
    printf 'pnpm live smoke did not clean %s\n' "$expected" >&2
    exit 1
  }
done

: > "$test_root/uninstalled.log"
rm -rf "$LIVE_SMOKE_ROOT"
bash "$repo_root/scripts/live-smoke/run.sh" npm-global "$fake_osdk" >/dev/null

for expected in 'global:npm:prettier@3.6.2' node@26.10.0 npm@12.2.0; do
  grep -Fxq "$expected" "$test_root/uninstalled.log" || {
    printf 'npm-global live smoke did not clean %s\n' "$expected" >&2
    exit 1
  }
done
