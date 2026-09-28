#!/usr/bin/env bash
set -Eeuo pipefail

: "${GITHUB_ENV:?GITHUB_ENV is required}"
: "${GITHUB_PATH:?GITHUB_PATH is required}"
osdk_bin=${OSDK_ACTION_OSDK:-"${OSDK_BIN_DIR:?OSDK_BIN_DIR is required}/osdk"}

path_before=$PATH
snippet=$("$osdk_bin" hook-env --shell bash)
eval "$snippet"

original_path=${OSDK_ORIGINAL_PATH:-$path_before}
if [[ $PATH == "$original_path" ]]; then
  managed_path=
elif [[ $PATH == *":$original_path" ]]; then
  managed_path=${PATH%":$original_path"}
else
  printf 'osdk activation did not preserve the original PATH suffix\n' >&2
  exit 1
fi
if [[ -n "$managed_path" ]]; then
  IFS=':' read -r -a managed_directories <<< "$managed_path"
  for directory in "${managed_directories[@]}"; do
    [[ -z "$directory" ]] || printf '%s\n' "$directory" >> "$GITHUB_PATH"
  done
fi

write_env() {
  local name=$1
  local value=${!name-}
  local delimiter="OSDK_${RANDOM}_${RANDOM}"
  printf '%s<<%s\n%s\n%s\n' "$name" "$delimiter" "$value" "$delimiter" >> "$GITHUB_ENV"
}

IFS=',' read -r -a managed_names <<< "${OSDK_MANAGED_ENV:-}"
for name in "${managed_names[@]}"; do
  [[ -z "$name" ]] || write_env "$name"
done
write_env OSDK_MANAGED_ENV
