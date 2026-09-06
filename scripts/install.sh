#!/bin/sh
set -eu

REPO="Rizzist/haider-agent"
# Ask for the ONE release marked latest, never the list. The list endpoint
# returns every release on a single line, and the greedy `.*` in the sed below
# then matches the LAST tag_name on that line rather than the first -- which
# silently installed the OLDEST of the 20 most recent releases (0.0.945 while
# 0.0.964 was current). `latest` also only moves once a release has been
# verified, so this respects that promotion gate.
API_URL="https://api.github.com/repos/$REPO/releases/latest"

fail() {
  echo "haider install: $*" >&2
  exit 1
}

# Registry #94: each attempt reserves connection setup plus resource capacity
# at a conservative 1 MiB/s transfer floor. The archive capacity is the shared
# updater's MAX_ARCHIVE_BYTES, checksum is MAX_CHECKSUM_BYTES. Two independent
# attempts never inherit the overall install/member-verification watchdog.
FETCH_ATTEMPTS=2
FETCH_CONNECT_SECONDS=30
FETCH_TRANSFER_BYTES_PER_SECOND=$((1024 * 1024))
FETCH_ARCHIVE_BYTES=$((128 * 1024 * 1024))
FETCH_CHECKSUM_BYTES=$((16 * 1024))
FETCH_METADATA_BYTES=$((1024 * 1024))
fetch() (
  fetch_capacity=${2:-$FETCH_METADATA_BYTES}
  FETCH_ATTEMPT_SECONDS=$((FETCH_CONNECT_SECONDS + (fetch_capacity + FETCH_TRANSFER_BYTES_PER_SECOND - 1) / FETCH_TRANSFER_BYTES_PER_SECOND))
  fetch_tmp=$(mktemp "${TMPDIR:-/tmp}/haider-fetch.XXXXXX")
  trap 'rm -f "$fetch_tmp"' 0
  attempt=0
  while [ "$attempt" -lt "$FETCH_ATTEMPTS" ]; do
    attempt=$((attempt + 1))
    if command -v curl >/dev/null 2>&1; then
      if curl -fsSL --max-time "$FETCH_ATTEMPT_SECONDS" --connect-timeout "$FETCH_CONNECT_SECONDS" -H "Accept: application/vnd.github+json" -H "User-Agent: HaiderInstaller" "$1" > "$fetch_tmp"; then
        cat "$fetch_tmp"
        exit 0
      fi
    elif command -v wget >/dev/null 2>&1; then
      # wget --timeout only bounds idle IO, not total wall time. A separate
      # timer bounds the whole attempt, including a peer sending a slow trickle.
      wget -qO- --tries=1 --timeout="$FETCH_ATTEMPT_SECONDS" --header="Accept: application/vnd.github+json" --header="User-Agent: HaiderInstaller" "$1" > "$fetch_tmp" &
      fetch_pid=$!
      (
        sleep "$FETCH_ATTEMPT_SECONDS" &
        sleep_pid=$!
        trap 'kill "$sleep_pid" 2>/dev/null || :; wait "$sleep_pid" 2>/dev/null || :; exit' TERM INT
        wait "$sleep_pid"
        kill "$fetch_pid" 2>/dev/null || :
      ) &
      timer_pid=$!
      result=0
      wait "$fetch_pid" || result=$?
      kill "$timer_pid" 2>/dev/null || :
      wait "$timer_pid" 2>/dev/null || :
      if [ "$result" -eq 0 ]; then cat "$fetch_tmp"; exit 0; fi
    else
      fail "curl or wget is required"
    fi
  done
  fail "download failed after $FETCH_ATTEMPTS bounded attempts: $1"
)

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required"
  fi
}

# Match the bundle transaction's directory authority without changing its
# ownership or permissions. find's numeric owner/mode predicates work on both
# BSD/macOS and GNU/Linux; writable alone also admits shared/managed prefixes.
install_dir_eligible() {
  [ -d "$1" ] && [ ! -L "$1" ] && [ -w "$1" ] || return 1
  [ "$(find "$1" -prune -user "$(id -u)" -perm -0200 ! -perm -0020 ! -perm -0002 -print 2>/dev/null)" = "$1" ]
}

choose_install_dir() {
  if [ -n "${HAIDER_INSTALL_DIR:-}" ]; then
    if [ -n "$BUNDLE_SUFFIX" ] && { [ -e "$HAIDER_INSTALL_DIR" ] || [ -L "$HAIDER_INSTALL_DIR" ]; } && ! install_dir_eligible "$HAIDER_INSTALL_DIR"; then
      fail "HAIDER_INSTALL_DIR must be a real directory owned and writable by the current user, with no group/other write permission; choose an owned prefix such as $2"
    fi
    printf '%s\n' "$HAIDER_INSTALL_DIR"
  elif [ -w "$1" ] && { [ -z "$BUNDLE_SUFFIX" ] || install_dir_eligible "$1"; }; then
    printf '%s\n' "$1"
  else
    printf '%s\n' "$2"
  fi
}

detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os:$arch" in
    Darwin:arm64|Darwin:aarch64) echo "aarch64-apple-darwin" ;;
    Darwin:x86_64|Darwin:amd64) echo "x86_64-apple-darwin" ;;
    Linux:x86_64|Linux:amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) echo "aarch64-unknown-linux-gnu" ;;
    *) fail "unsupported platform $os/$arch" ;;
  esac
}

VERSION="${HAIDER_VERSION:-}"
if [ -z "$VERSION" ]; then
  release_json=$(fetch "$API_URL" 2>/dev/null || true)
  # Split on commas so every JSON field sits on its own line: POSIX sed has no
  # non-greedy match, so `.*` would otherwise run past the field we want.
  VERSION=$(printf '%s\n' "$release_json" | tr ',' '\n' \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$VERSION" ] || fail "could not determine latest version; set HAIDER_VERSION=vX.Y.Z"

case "$VERSION" in
  v*) TAG="$VERSION"; VERSION="${VERSION#v}" ;;
  *) TAG="v$VERSION" ;;
esac

TARGET=$(detect_target)
# Historical releases predate both the split archive and --install-bundle.
BUNDLE_SUFFIX="-split"
case "$VERSION" in
  0.0.*)
    patch="${VERSION#0.0.}"
    patch="${patch%%[-+]*}"
    case "$patch" in
      ''|*[!0-9]*) fail "invalid version $VERSION" ;;
    esac
    if [ "$patch" -lt 970 ]; then BUNDLE_SUFFIX=""; fi
    ;;
esac
ARTIFACT="haider-$TAG-$TARGET$BUNDLE_SUFFIX.tar.xz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

TMPDIR_ROOT="${TMPDIR:-/tmp}"
TMP=$(mktemp -d "$TMPDIR_ROOT/haider-install.XXXXXX")
trap 'rm -rf "$TMP"' 0
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading $ARTIFACT"
# Independent resources can arrive concurrently; both must succeed before hash
# validation, extraction, or execution. Always reap both before scratch cleanup.
fetch "$BASE_URL/$ARTIFACT" "$FETCH_ARCHIVE_BYTES" > "$TMP/$ARTIFACT" &
archive_pid=$!
fetch "$BASE_URL/$ARTIFACT.sha256" "$FETCH_CHECKSUM_BYTES" > "$TMP/$ARTIFACT.sha256" &
sidecar_pid=$!
fetch_failed=0
wait "$archive_pid" || fetch_failed=1
wait "$sidecar_pid" || fetch_failed=1
[ "$fetch_failed" -eq 0 ] || fail "archive or checksum download failed"

EXPECTED=$(awk -v file="$ARTIFACT" '
  /^[[:space:]]*$/ { next }
  {
    hash=tolower($1)
    if (NF == 1) { print hash; exit }
    name=$2
    sub(/^\*/, "", name)
    sub(/^\.\//, "", name)
    sub(/^.*\//, "", name)
    if (name == file) { print hash; exit }
  }
' "$TMP/$ARTIFACT.sha256")
case "$EXPECTED" in
  ""|*[!0-9a-f]*) fail "$ARTIFACT.sha256 did not contain a valid checksum" ;;
esac
[ "${#EXPECTED}" -eq 64 ] || fail "$ARTIFACT.sha256 did not contain a valid checksum"
ACTUAL=$(checksum "$TMP/$ARTIFACT")
[ "$EXPECTED" = "$ACTUAL" ] || fail "checksum mismatch for $ARTIFACT"

tar -xJf "$TMP/$ARTIFACT" -C "$TMP"
BUNDLE_DIR="$TMP/haider-$TAG-$TARGET$BUNDLE_SUFFIX"
[ -f "$BUNDLE_DIR/haider" ] || fail "archive did not contain haider"
[ -f "$BUNDLE_DIR/haiderd" ] || fail "archive did not contain haiderd"
if [ -n "$BUNDLE_SUFFIX" ]; then
  [ -f "$BUNDLE_DIR/haider-tui" ] || fail "archive did not contain haider-tui"
fi

INSTALL_DIR=$(choose_install_dir /usr/local/bin "$HOME/.local/bin")

if [ -n "$BUNDLE_SUFFIX" ]; then
  # The verified archive's thin binary owns version checks, the shared OS lock,
  # staging, commit, rollback and interrupted-transaction recovery.
  "$BUNDLE_DIR/haider" --install-bundle "$BUNDLE_DIR" "$INSTALL_DIR"
  echo "Installed haider $VERSION, haider-tui, and haiderd to $INSTALL_DIR"
else
  # Keep pinned historic installs usable with their original two-file layout.
  [ ! -e "$INSTALL_DIR/.haider-update-transaction.json" ] || fail "finish the pending bundle transaction before installing a historical release"
  mkdir -p "$INSTALL_DIR"
  cp "$BUNDLE_DIR/haider" "$INSTALL_DIR/haider"
  cp "$BUNDLE_DIR/haiderd" "$INSTALL_DIR/haiderd"
  chmod 755 "$INSTALL_DIR/haider" "$INSTALL_DIR/haiderd"
  if [ -f "$BUNDLE_DIR/haider-wayland-portal" ]; then
    cp "$BUNDLE_DIR/haider-wayland-portal" "$INSTALL_DIR/haider-wayland-portal"
    chmod 755 "$INSTALL_DIR/haider-wayland-portal"
  fi
  echo "Installed haider $VERSION and haiderd to $INSTALL_DIR"
fi
case ":$PATH:" in
  *":$INSTALL_DIR:"*) echo "Run: haider" ;;
  *) echo "Warning: $INSTALL_DIR is not on PATH; add it, then run: haider" >&2 ;;
esac

case "$TARGET" in
  *apple-darwin) echo "macOS binaries are Developer ID signed and Apple-notarized." ;;
  *linux-gnu) echo "Note: Linux binaries are currently unsigned; the release SHA-256 was verified." ;;
esac
