#!/usr/bin/env bash
set -Eeuo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/.." && pwd)
test_root=$(mktemp -d)
cleanup() {
  rm -rf "$test_root"
}
trap cleanup EXIT

export RUNNER_TEMP="$test_root/runner"
export GITHUB_ACTION_PATH="$repo_root"
export GITHUB_ENV="$test_root/github-env"
export GITHUB_PATH="$test_root/github-path"
export GITHUB_OUTPUT="$test_root/github-output"
export OSDK_ACTION_TEST_LOG="$test_root/osdk.log"
export OSDK_ACTION_TEST_INSTALLER_LOG="$test_root/installer.log"
mkdir -p "$RUNNER_TEMP"
: > "$GITHUB_ENV"
: > "$GITHUB_PATH"
: > "$GITHUB_OUTPUT"
: > "$OSDK_ACTION_TEST_LOG"

fake_osdk_source="$test_root/fake-osdk"
cat > "$fake_osdk_source" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

if [[ ${1:-} == --version ]]; then
  printf 'osdk 9.8.7\n'
  exit 0
fi
if [[ ${1:-} == hook-env && ${2:-} == --shell && ${3:-} == bash ]]; then
  cat <<'HOOK'
if [ -z "${OSDK_ORIGINAL_PATH_SET+x}" ]; then export OSDK_ORIGINAL_PATH="$PATH"; export OSDK_ORIGINAL_PATH_SET=1; fi
export PATH='/managed/one:/managed/two':"$OSDK_ORIGINAL_PATH"
export CARGO_HOME='/cache/cargo'
export OSDK_MANAGED_ENV='CARGO_HOME'
HOOK
  exit 0
fi
printf '%s\n' "$*" >> "${OSDK_ACTION_TEST_LOG:?}"
EOF
chmod +x "$fake_osdk_source"

fake_installer="$test_root/fake-installer.sh"
cat > "$fake_installer" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%s\n' "$*" > "${OSDK_ACTION_TEST_INSTALLER_LOG:?}"
mkdir -p "${OSDK_BIN_DIR:?}"
cp "${OSDK_ACTION_TEST_FAKE_OSDK:?}" "$OSDK_BIN_DIR/osdk"
chmod +x "$OSDK_BIN_DIR/osdk"
EOF
chmod +x "$fake_installer"

export OSDK_ACTION_TEST_FAKE_OSDK="$fake_osdk_source"
export OSDK_ACTION_INSTALLER="$fake_installer"
export OSDK_ACTION_VERSION=
export OSDK_ACTION_REF=v9.8.7
export OSDK_ACTION_REPOSITORY=example/osdk
export OSDK_ACTION_DOWNLOAD_BASE_URL=https://downloads.example.test
export OSDK_ACTION_TARGET=x86_64-unknown-linux-musl
if OSDK_ACTION_CACHE=invalid "$repo_root/action/setup.sh" >"$test_root/invalid.log" 2>&1; then
  printf 'setup accepted an invalid boolean input\n' >&2
  exit 1
fi
grep -Fq 'OSDK_ACTION_CACHE must be true or false' "$test_root/invalid.log"
export OSDK_ACTION_CACHE=true
"$repo_root/action/setup.sh"

grep -Fxq 'version=9.8.7' "$GITHUB_OUTPUT"
grep -Fq -- '--version v9.8.7' "$OSDK_ACTION_TEST_INSTALLER_LOG"
grep -Fq -- '--repository example/osdk' "$OSDK_ACTION_TEST_INSTALLER_LOG"
grep -Fq -- '--target x86_64-unknown-linux-musl' "$OSDK_ACTION_TEST_INSTALLER_LOG"

# A local `uses: ./` has no semver action ref. Omitting `version` must follow
# the latest Release rather than inheriting an unrelated branch or SHA string.
export OSDK_ACTION_REF=
"$repo_root/action/setup.sh"
grep -Fq -- '--version latest' "$OSDK_ACTION_TEST_INSTALLER_LOG"

export OSDK_BIN_DIR="$RUNNER_TEMP/osdk/bin"
export OSDK_DATA_DIR="$RUNNER_TEMP/osdk/data"
export OSDK_CACHE_DIR="$RUNNER_TEMP/osdk/cache"
export OSDK_CONFIG_DIR="$RUNNER_TEMP/osdk/config"
export PATH="$OSDK_BIN_DIR:$PATH"
export OSDK_ACTION_INSTALL_TOOLS=true
export OSDK_ACTION_INSTALL_DEPS=true
export OSDK_ACTION_FROZEN=true
export OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL=false
export OSDK_ACTION_JOBS=3
export OSDK_ACTION_SOURCE_MODE=auto
export OSDK_ACTION_OFFLINE=false
export OSDK_ACTION_REQUIRE_CHECKSUMS=true
export OSDK_ACTION_ATTESTATIONS=required
"$repo_root/action/materialize.sh"

grep -Fxq -- '--yes --jobs 3 --source-mode auto --require-checksums --attestations required install --no-deps' "$OSDK_ACTION_TEST_LOG"
grep -Fxq -- '--yes --jobs 3 --source-mode auto --require-checksums --attestations required deps --frozen --no-install-tools' "$OSDK_ACTION_TEST_LOG"

"$repo_root/action/export-env.sh"
grep -Fxq '/managed/one' "$GITHUB_PATH"
grep -Fxq '/managed/two' "$GITHUB_PATH"
grep -Fq 'CARGO_HOME<<OSDK_' "$GITHUB_ENV"
grep -Fxq '/cache/cargo' "$GITHUB_ENV"

printf 'GitHub Action Unix helper smoke passed\n'
