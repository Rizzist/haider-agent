#!/bin/sh
set -eu

REPO="Rizzist/haider-agent"
API_URL="https://api.github.com/repos/$REPO/releases?per_page=20"

fail() {
  echo "haider install: $*" >&2
  exit 1
}

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -H "Accept: application/vnd.github+json" -H "User-Agent: HaiderInstaller" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --header="Accept: application/vnd.github+json" --header="User-Agent: HaiderInstaller" "$1"
  else
    fail "curl or wget is required"
  fi
}

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required"
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
  releases_json=$(fetch "$API_URL" 2>/dev/null || true)
  VERSION=$(printf '%s\n' "$releases_json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$VERSION" ] || fail "could not determine latest version; set HAIDER_VERSION=vX.Y.Z"

case "$VERSION" in
  v*) TAG="$VERSION"; VERSION="${VERSION#v}" ;;
  *) TAG="v$VERSION" ;;
esac

TARGET=$(detect_target)
ARTIFACT="haider-$TAG-$TARGET.tar.xz"
BASE_URL="https://github.com/$REPO/releases/download/$TAG"

TMPDIR_ROOT="${TMPDIR:-/tmp}"
TMP=$(mktemp -d "$TMPDIR_ROOT/haider-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "Downloading $ARTIFACT"
fetch "$BASE_URL/$ARTIFACT" > "$TMP/$ARTIFACT"
fetch "$BASE_URL/$ARTIFACT.sha256" > "$TMP/$ARTIFACT.sha256"

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
BUNDLE_DIR="$TMP/haider-$TAG-$TARGET"
[ -f "$BUNDLE_DIR/haider" ] || fail "archive did not contain haider"
[ -f "$BUNDLE_DIR/haiderd" ] || fail "archive did not contain haiderd"

if [ -n "${HAIDER_INSTALL_DIR:-}" ]; then
  INSTALL_DIR="$HAIDER_INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.local/bin"
fi

mkdir -p "$INSTALL_DIR"
cp "$BUNDLE_DIR/haider" "$INSTALL_DIR/haider"
cp "$BUNDLE_DIR/haiderd" "$INSTALL_DIR/haiderd"
chmod 755 "$INSTALL_DIR/haider" "$INSTALL_DIR/haiderd"

if [ -f "$BUNDLE_DIR/haider-wayland-portal" ]; then
  cp "$BUNDLE_DIR/haider-wayland-portal" "$INSTALL_DIR/haider-wayland-portal"
  chmod 755 "$INSTALL_DIR/haider-wayland-portal"
fi

echo "Installed haider $VERSION and haiderd to $INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) echo "Run: haider" ;;
  *) echo "Warning: $INSTALL_DIR is not on PATH; add it, then run: haider" >&2 ;;
esac

case "$TARGET" in
  *apple-darwin) echo "macOS binaries are Developer ID signed and Apple-notarized." ;;
  *linux-gnu) echo "Note: Linux binaries are currently unsigned; the release SHA-256 was verified." ;;
esac
