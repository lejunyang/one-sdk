#!/bin/sh
set -eu

REPOSITORY=${OSDK_REPOSITORY:-lejunyang/one-sdk}
VERSION=${OSDK_VERSION:-latest}
INSTALL_DIR=${OSDK_BIN_DIR:-"$HOME/.local/bin"}
BASE_URL=${OSDK_DOWNLOAD_BASE_URL:-https://github.com}
TARGET=${OSDK_TARGET:-}
SKIP_VERIFY=${OSDK_SKIP_VERIFY:-0}

usage() {
  cat <<'EOF'
Install osdk from GitHub Releases.

Usage:
  install.sh [options]

Options:
  --version <version>       Release version, with or without "v" (default: latest)
  --install-dir <path>      Binary directory (default: $HOME/.local/bin)
  --repository <owner/repo> GitHub repository (default: lejunyang/one-sdk)
  --base-url <url>          Download base or mirror URL (default: https://github.com)
  --target <target>         Override the detected Rust target triple
  --skip-verify             Skip SHA-256 verification
  -h, --help                Show this help

Environment equivalents:
  OSDK_VERSION, OSDK_BIN_DIR, OSDK_REPOSITORY,
  OSDK_DOWNLOAD_BASE_URL, OSDK_TARGET, OSDK_SKIP_VERIFY
EOF
}

fail() {
  printf 'osdk installer: %s\n' "$*" >&2
  exit 1
}

need_value() {
  [ "$#" -ge 2 ] || fail "$1 requires a value"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      need_value "$@"
      VERSION=$2
      shift 2
      ;;
    --install-dir)
      need_value "$@"
      INSTALL_DIR=$2
      shift 2
      ;;
    --repository)
      need_value "$@"
      REPOSITORY=$2
      shift 2
      ;;
    --base-url)
      need_value "$@"
      BASE_URL=$2
      shift 2
      ;;
    --target)
      need_value "$@"
      TARGET=$2
      shift 2
      ;;
    --skip-verify)
      SKIP_VERIFY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

if [ -z "$TARGET" ]; then
  kernel=$(uname -s)
  machine=$(uname -m)
  case "$kernel:$machine" in
    Linux:x86_64|Linux:amd64)
      TARGET=x86_64-unknown-linux-gnu
      ;;
    Linux:aarch64|Linux:arm64)
      TARGET=aarch64-unknown-linux-gnu
      ;;
    Darwin:x86_64|Darwin:amd64)
      TARGET=x86_64-apple-darwin
      ;;
    Darwin:arm64|Darwin:aarch64)
      TARGET=aarch64-apple-darwin
      ;;
    *)
      fail "unsupported platform: $kernel $machine (use --target to override)"
      ;;
  esac
fi

case "$VERSION" in
  latest)
    release_path=latest/download
    ;;
  v*)
    release_path="download/$VERSION"
    ;;
  *)
    release_path="download/v$VERSION"
    ;;
esac

BASE_URL=${BASE_URL%/}
archive="osdk-$TARGET.tar.gz"
release_url="$BASE_URL/$REPOSITORY/releases/$release_path"
work_dir=$(mktemp -d 2>/dev/null || mktemp -d -t osdk-install)
transaction_dir=
lock_file="$INSTALL_DIR/.osdk-install.lock"
lock_mode=
lock_owner_file=
lock_snapshot_file=
promotion_active=0
preserve_transaction=0
binaries="osdk osdk-shim osdk-aube"

path_exists() {
  [ -e "$1" ] || [ -L "$1" ]
}

sync_file() {
  if sync -f "$1" 2>/dev/null; then
    return 0
  fi
  sync || fail "could not flush installer state to disk"
}

initialize_transaction_journal() {
  journal_path="$transaction_dir/journal"
  printf '%s\n' 'version=1' 'phase=initializing' > "$journal_path" ||
    fail "could not initialize the installer transaction journal"
  sync_file "$journal_path"
}

append_transaction_phase() {
  journal_path="$transaction_dir/journal"
  printf 'phase=%s\n' "$1" >> "$journal_path" ||
    fail "could not update the installer transaction journal"
  sync_file "$journal_path"
}

read_transaction_phase() {
  journal_path="$1/journal"
  recovery_phase=initializing
  journal_version=
  parsed_phase=
  journal_records=0

  if ! path_exists "$journal_path"; then
    return 0
  fi
  [ -f "$journal_path" ] && [ -r "$journal_path" ] ||
    fail "invalid installer transaction journal: $journal_path"

  # read(1) only enters the loop for newline-terminated records. A torn final
  # append is therefore ignored and the last complete phase remains binding.
  while IFS= read -r journal_record; do
    journal_records=$((journal_records + 1))
    case "$journal_record" in
      version=1)
        [ "$journal_records" -eq 1 ] && [ -z "$journal_version" ] ||
          fail "invalid installer transaction journal: $journal_path"
        journal_version=1
        ;;
      phase=initializing)
        [ "$journal_version" = 1 ] && [ -z "$parsed_phase" ] ||
          fail "invalid installer transaction journal: $journal_path"
        parsed_phase=initializing
        ;;
      phase=staging)
        [ "$parsed_phase" = initializing ] ||
          fail "invalid installer transaction journal: $journal_path"
        parsed_phase=staging
        ;;
      phase=prepared)
        [ "$parsed_phase" = staging ] ||
          fail "invalid installer transaction journal: $journal_path"
        parsed_phase=prepared
        ;;
      phase=promoting)
        [ "$parsed_phase" = prepared ] ||
          fail "invalid installer transaction journal: $journal_path"
        parsed_phase=promoting
        ;;
      phase=committed)
        [ "$parsed_phase" = promoting ] ||
          fail "invalid installer transaction journal: $journal_path"
        parsed_phase=committed
        ;;
      *)
        fail "invalid installer transaction journal: $journal_path"
        ;;
    esac
  done < "$journal_path"

  # A versioned directory with no complete record can only precede the first
  # mutation. Likewise, a durable version header without a phase is init debris.
  if [ "$journal_records" -eq 0 ] ||
     { [ "$journal_version" = 1 ] && [ -z "$parsed_phase" ]; }; then
    recovery_phase=initializing
    return 0
  fi
  [ "$journal_version" = 1 ] && [ -n "$parsed_phase" ] ||
    fail "invalid installer transaction journal: $journal_path"
  recovery_phase=$parsed_phase
}

restore_transaction() {
  restore_candidate=$1
  remove_new_installations=$2
  restore_failed=0
  restore_dir="$restore_candidate/restore"

  if ! mkdir -p "$restore_dir"; then
    printf 'osdk installer: failed to create rollback workspace %s\n' \
      "$restore_dir" >&2
    return 1
  fi

  for binary in $binaries; do
    staged="$restore_candidate/new/$binary"
    backup="$restore_candidate/old/$binary"
    destination="$INSTALL_DIR/$binary"
    restore_path="$restore_dir/$binary"

    if path_exists "$backup"; then
      # Copy, rather than consume, the backup. If recovery itself is killed,
      # the next invocation can replay the same rollback idempotently.
      if ! cp -pP "$backup" "$restore_path"; then
        printf 'osdk installer: failed to prepare restoration of %s\n' \
          "$destination" >&2
        restore_failed=1
        continue
      fi
      if path_exists "$destination" && ! rm -f "$destination"; then
        printf 'osdk installer: failed to remove replacement %s during rollback\n' \
          "$destination" >&2
        restore_failed=1
        continue
      fi
      if ! mv "$restore_path" "$destination"; then
        printf 'osdk installer: failed to restore %s during rollback\n' \
          "$destination" >&2
        restore_failed=1
      fi
    elif [ "$remove_new_installations" = 1 ] &&
         ! path_exists "$staged" && path_exists "$destination"; then
      if ! rm -f "$destination"; then
        printf 'osdk installer: failed to remove new %s during rollback\n' \
          "$destination" >&2
        restore_failed=1
      fi
    fi
  done

  [ "$restore_failed" = 0 ]
}

recover_installer_transactions() {
  for candidate in "$INSTALL_DIR"/.osdk-install*; do
    [ -d "$candidate" ] || continue
    candidate_name=${candidate##*/}
    [ "$candidate_name" = .osdk-install.lock ] && continue

    case "$candidate_name" in
      .osdk-install-v2.*)
        read_transaction_phase "$candidate"
        case "$recovery_phase" in
          initializing|staging|prepared)
            # These phases precede all destination mutation, so even a partial
            # directory layout is disposable and installed binaries stay intact.
            ;;
          promoting)
            [ -d "$candidate/new" ] && [ -d "$candidate/old" ] ||
              fail "invalid promoting installer transaction: $candidate"
            restore_transaction "$candidate" 1 ||
              fail "could not recover interrupted installation at $candidate"
            ;;
          committed)
            # All three promotions completed before this record was flushed.
            ;;
          *)
            fail "invalid installer transaction phase in $candidate"
            ;;
        esac
        ;;
      .osdk-install.*|.osdk-install-*)
        # Compatibility with the previous markerless journal. Backups prove
        # promotion began; without one, staging and fresh-install promotion are
        # indistinguishable, so never infer deletion from a missing staged file.
        legacy_has_backup=0
        for binary in $binaries; do
          if path_exists "$candidate/old/$binary"; then
            legacy_has_backup=1
            break
          fi
        done
        if [ "$legacy_has_backup" = 1 ]; then
          restore_transaction "$candidate" 0 ||
            fail "could not recover interrupted installation at $candidate"
        fi
        ;;
      *)
        continue
        ;;
    esac

    rm -rf "$candidate" ||
      fail "could not remove recovered installer directory $candidate"
  done
}

portable_process_identity() (
  identity_pid=$1
  case $identity_pid in
    ''|*[!0-9]*) exit 1 ;;
  esac

  if [ -r "/proc/$identity_pid/stat" ] &&
     [ -r /proc/sys/kernel/random/boot_id ]; then
    identity_stat=$(cat "/proc/$identity_pid/stat") || exit 1
    # Keep the closing parenthesis out of the parameter-expansion pattern so
    # macOS Bash 3.2 can parse this script. The longest-match removal still
    # selects the last `) `, even when the process name contains that sequence.
    identity_stat_separator=') '
    identity_rest=${identity_stat##*"$identity_stat_separator"}
    set -- $identity_rest
    [ "$#" -ge 20 ] || exit 1
    shift 19
    identity_boot=$(cat /proc/sys/kernel/random/boot_id) || exit 1
    printf 'linux:%s:%s\n' "$identity_boot" "$1"
    exit 0
  fi

  identity_start=$(
    LC_ALL=C ps -o lstart= -p "$identity_pid" 2>/dev/null |
      awk '{$1=$1; print}'
  )
  [ -n "$identity_start" ] || exit 1
  if command -v sysctl >/dev/null 2>&1; then
    identity_boot=$(LC_ALL=C sysctl -n kern.boottime 2>/dev/null || :)
  else
    identity_boot=
  fi
  [ -n "$identity_boot" ] || identity_boot=unknown-boot
  printf 'posix:%s:%s:%s\n' "$(uname -n)" "$identity_boot" "$identity_start"
)

read_portable_lock_record() {
  record_path=$1
  lock_record_version=
  lock_record_machine=
  lock_record_pid=
  lock_record_identity=
  lock_record_token=
  lock_record_lines=0

  [ -f "$record_path" ] && [ -r "$record_path" ] || return 1
  [ -s "$record_path" ] || return 1
  record_last_byte=$(tail -c 1 "$record_path" | od -An -t u1 | tr -d ' ')
  [ "$record_last_byte" = 10 ] || return 1

  while IFS= read -r lock_record_line; do
    lock_record_lines=$((lock_record_lines + 1))
    case "$lock_record_line" in
      version=1)
        [ -z "$lock_record_version" ] || return 1
        lock_record_version=1
        ;;
      machine=*)
        [ -z "$lock_record_machine" ] || return 1
        lock_record_machine=${lock_record_line#machine=}
        ;;
      pid=*)
        [ -z "$lock_record_pid" ] || return 1
        lock_record_pid=${lock_record_line#pid=}
        ;;
      identity=*)
        [ -z "$lock_record_identity" ] || return 1
        lock_record_identity=${lock_record_line#identity=}
        ;;
      token=*)
        [ -z "$lock_record_token" ] || return 1
        lock_record_token=${lock_record_line#token=}
        ;;
      *) return 1 ;;
    esac
  done < "$record_path"

  [ "$lock_record_lines" -eq 5 ] &&
    [ "$lock_record_version" = 1 ] &&
    [ -n "$lock_record_machine" ] &&
    [ -n "$lock_record_identity" ] &&
    [ -n "$lock_record_token" ] || return 1
  case $lock_record_token in
    .osdk-install-lock.owner.*) ;;
    *) return 1 ;;
  esac
  case $lock_record_token in
    *[!A-Za-z0-9._-]*) return 1 ;;
  esac
  case $lock_record_pid in
    ''|*[!0-9]*|0) return 1 ;;
  esac
}

portable_lock_is_stale() {
  [ "$lock_record_machine" = "$(uname -n)" ] || return 1
  current_lock_identity=$(portable_process_identity "$lock_record_pid" 2>/dev/null || :)
  if [ -n "$current_lock_identity" ]; then
    case "$lock_record_identity:$current_lock_identity" in
      linux:*:linux:*)
        # Linux boot ID plus /proc start ticks distinguishes a reused PID.
        [ "$current_lock_identity" != "$lock_record_identity" ]
        return
        ;;
      *)
        # Other portable process-start strings can be too coarse to prove PID
        # reuse, so any live PID remains an active/unverifiable owner.
        return 1
        ;;
    esac
  fi

  # EPERM and platforms with an unreadable birth identity are fail-closed.
  if kill -0 "$lock_record_pid" 2>/dev/null ||
     ps -p "$lock_record_pid" >/dev/null 2>&1; then
    return 1
  fi
  return 0
}

acquire_portable_lock() {
  portable_machine=$(uname -n)
  portable_identity=$(portable_process_identity "$$") ||
    fail "could not determine process identity for the portable installer lock"
  lock_owner_file=$(mktemp "$INSTALL_DIR/.osdk-install-lock.owner.XXXXXX") ||
    fail "could not create a portable installer lock owner record"
  portable_token=${lock_owner_file##*/}
  printf '%s\n' \
    'version=1' \
    "machine=$portable_machine" \
    "pid=$$" \
    "identity=$portable_identity" \
    "token=$portable_token" > "$lock_owner_file" ||
    fail "could not initialize the portable installer lock"
  sync_file "$lock_owner_file"

  portable_attempt=0
  while [ "$portable_attempt" -lt 20 ]; do
    portable_attempt=$((portable_attempt + 1))
    if ln "$lock_owner_file" "$lock_file" 2>/dev/null; then
      [ "$lock_owner_file" -ef "$lock_file" ] ||
        fail "could not verify ownership of the portable installer lock"
      lock_mode=portable
      return 0
    fi

    if [ -d "$lock_file" ]; then
      legacy_pid_file="$lock_file/pid"
      [ -f "$legacy_pid_file" ] && [ -r "$legacy_pid_file" ] ||
        fail "another installer may be updating $INSTALL_DIR (unverifiable legacy lock: $lock_file)"
      legacy_pid=$(sed -n '1p' "$legacy_pid_file")
      case $legacy_pid in
        ''|*[!0-9]*|0)
          fail "another installer may be updating $INSTALL_DIR (unverifiable legacy lock: $lock_file)"
          ;;
      esac
      if kill -0 "$legacy_pid" 2>/dev/null ||
         ps -p "$legacy_pid" >/dev/null 2>&1; then
        # A legacy PID has no birth marker, so a live/reused PID is never stolen.
        fail "another installer is updating $INSTALL_DIR (lock: $lock_file)"
      fi
      legacy_stale="$INSTALL_DIR/.osdk-install-lock.stale.$$.$portable_attempt"
      if mv "$lock_file" "$legacy_stale" 2>/dev/null; then
        rm -rf "$legacy_stale"
      fi
      continue
    fi

    if path_exists "$lock_file"; then
      lock_snapshot_file=$(mktemp "$INSTALL_DIR/.osdk-install-lock.snapshot.XXXXXX") ||
        fail "could not inspect the portable installer lock"
      rm -f "$lock_snapshot_file"
      if ! ln "$lock_file" "$lock_snapshot_file" 2>/dev/null; then
        lock_snapshot_file=
        continue
      fi
      if ! read_portable_lock_record "$lock_snapshot_file"; then
        fail "another installer may be updating $INSTALL_DIR (unverifiable lock: $lock_file)"
      fi
      if ! portable_lock_is_stale; then
        fail "another installer is updating $INSTALL_DIR (lock: $lock_file)"
      fi
      # Snapshot and compare the inode immediately before unlinking. This avoids
      # deleting a replacement observed while stale-owner metadata was checked.
      if [ "$lock_snapshot_file" -ef "$lock_file" ]; then
        rm -f "$lock_file" || fail "could not reclaim stale installer lock $lock_file"
        stale_owner_file="$INSTALL_DIR/$lock_record_token"
        if path_exists "$stale_owner_file" &&
           [ "$lock_snapshot_file" -ef "$stale_owner_file" ]; then
          rm -f "$stale_owner_file"
        fi
      fi
      rm -f "$lock_snapshot_file"
      lock_snapshot_file=
      continue
    fi
  done

  fail "another installer may be updating $INSTALL_DIR (lock: $lock_file)"
}

rollback_install() {
  [ "$promotion_active" = "1" ] || return 0
  promotion_active=0
  if ! restore_transaction "$transaction_dir" 1; then
    preserve_transaction=1
    return 1
  fi
}

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if ! rollback_install; then
    status=1
  fi
  if [ -n "$transaction_dir" ]; then
    if [ "$preserve_transaction" = "1" ]; then
      printf 'osdk installer: preserving recovery files at %s\n' \
        "$transaction_dir" >&2
    else
      rm -rf "$transaction_dir"
    fi
  fi
  case $lock_mode in
    portable|portable-flock)
      if [ -n "$lock_owner_file" ] && path_exists "$lock_file" &&
         [ "$lock_owner_file" -ef "$lock_file" ]; then
        rm -f "$lock_file"
      fi
      ;;
  esac
  [ -z "$lock_snapshot_file" ] || rm -f "$lock_snapshot_file"
  [ -z "$lock_owner_file" ] || rm -f "$lock_owner_file"
  rm -rf "$work_dir"
  exit "$status"
}

on_signal() {
  trap - HUP INT TERM
  exit 1
}

trap cleanup EXIT
trap on_signal HUP INT TERM

printf 'Downloading %s\n' "$release_url/$archive"
download() {
  output=$1
  url=$2
  case "$url" in
    https://*)
      curl --fail --location --proto '=https' --tlsv1.2 --output "$output" "$url"
      ;;
    http://*)
      curl --fail --location --proto '=http' --output "$output" "$url"
      ;;
    *)
      fail "download URL must use http or https: $url"
      ;;
  esac
}
download "$work_dir/$archive" "$release_url/$archive"

if [ "$SKIP_VERIFY" != "1" ]; then
  download "$work_dir/SHA256SUMS" "$release_url/SHA256SUMS"
  expected=$(
    awk -v archive="$archive" '
      $2 == archive || $2 == "*" archive { print $1; exit }
    ' "$work_dir/SHA256SUMS"
  )
  [ -n "$expected" ] || fail "checksum for $archive is missing from SHA256SUMS"

  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work_dir/$archive" | awk '{ print $1 }')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$work_dir/$archive" | awk '{ print $1 }')
  else
    fail "sha256sum or shasum is required (or pass --skip-verify)"
  fi
  [ "$actual" = "$expected" ] || fail "checksum verification failed for $archive"
fi

mkdir -p "$work_dir/unpack"
tar -xzf "$work_dir/$archive" -C "$work_dir/unpack"
for binary in $binaries; do
  [ -f "$work_dir/unpack/$binary" ] || fail "$archive does not contain $binary"
  chmod 755 "$work_dir/unpack/$binary"
done

mkdir -p "$INSTALL_DIR"
# Every platform first uses the portable atomic claim. This keeps flock and
# no-flock invocations mutually exclusive instead of creating two lock domains.
# A stable boot/process-start tuple permits dead-owner recovery without treating
# a reused PID as the old owner. flock, when present, is additional hardening.
acquire_portable_lock
if command -v flock >/dev/null 2>&1 &&
   [ "${OSDK_TEST_FORCE_PORTABLE_LOCK:-0}" != 1 ]; then
  exec 9<>"$lock_owner_file"
  flock -n 9 || fail "could not lock the installer owner record"
  lock_mode=portable-flock
fi
recover_installer_transactions
for binary in $binaries; do
  [ ! -d "$INSTALL_DIR/$binary" ] || \
    fail "install destination is a directory: $INSTALL_DIR/$binary"
done

# Copy onto the destination filesystem first. Each per-file move is atomic and
# the journal makes the whole set rollback-capable, but a flat three-file layout
# cannot make all three names become visible as one atomic filesystem operation.
transaction_dir=$(mktemp -d "$INSTALL_DIR/.osdk-install-v2.XXXXXX") || \
  fail "could not create a staging directory in $INSTALL_DIR"
initialize_transaction_journal
mkdir -p "$transaction_dir/new" "$transaction_dir/old"
append_transaction_phase staging
for binary in $binaries; do
  cp "$work_dir/unpack/$binary" "$transaction_dir/new/$binary"
  chmod 755 "$transaction_dir/new/$binary"
done
for binary in $binaries; do
  [ -f "$transaction_dir/new/$binary" ] ||
    fail "staging did not produce $binary"
done
append_transaction_phase prepared

append_transaction_phase promoting
promotion_active=1
for binary in $binaries; do
  destination="$INSTALL_DIR/$binary"
  if path_exists "$destination"; then
    mv "$destination" "$transaction_dir/old/$binary" || \
      fail "failed to stage the existing $binary for replacement"
  fi
done
for binary in $binaries; do
  mv "$transaction_dir/new/$binary" "$INSTALL_DIR/$binary" || \
    fail "failed to install $binary; restoring the previous installation"
done
append_transaction_phase committed
promotion_active=0
rm -rf "$transaction_dir"
transaction_dir=

printf 'Installed osdk, osdk-shim, and osdk-aube to %s\n' "$INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf 'Add %s to PATH to run osdk.\n' "$INSTALL_DIR"
    ;;
esac
