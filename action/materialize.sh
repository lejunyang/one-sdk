#!/usr/bin/env bash
set -Eeuo pipefail

boolean() {
  case "$2" in
    true) return 0 ;;
    false) return 1 ;;
    *) printf '%s must be true or false, got %s\n' "$1" "$2" >&2; exit 2 ;;
  esac
}

osdk_bin=${OSDK_ACTION_OSDK:-"${OSDK_BIN_DIR:?OSDK_BIN_DIR is required}/osdk"}
global=(--yes)
if [[ -n ${OSDK_ACTION_JOBS:-} ]]; then
  global+=(--jobs "$OSDK_ACTION_JOBS")
fi
if [[ -n ${OSDK_ACTION_SOURCE_MODE:-} ]]; then
  global+=(--source-mode "$OSDK_ACTION_SOURCE_MODE")
fi
if boolean offline "${OSDK_ACTION_OFFLINE:-false}"; then
  global+=(--offline)
fi
if boolean require-checksums "${OSDK_ACTION_REQUIRE_CHECKSUMS:-false}"; then
  global+=(--require-checksums)
fi
if [[ -n ${OSDK_ACTION_ATTESTATIONS:-} ]]; then
  global+=(--attestations "$OSDK_ACTION_ATTESTATIONS")
fi

if boolean install-tools "${OSDK_ACTION_INSTALL_TOOLS:-true}"; then
  "$osdk_bin" "${global[@]}" install --no-deps
fi

if boolean install-deps "${OSDK_ACTION_INSTALL_DEPS:-true}"; then
  deps=(deps)
  if boolean frozen "${OSDK_ACTION_FROZEN:-true}"; then
    deps+=(--frozen)
  fi
  if ! boolean allow-deps-tool-install "${OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL:-false}"; then
    deps+=(--no-install-tools)
  fi
  "$osdk_bin" "${global[@]}" "${deps[@]}"
fi
