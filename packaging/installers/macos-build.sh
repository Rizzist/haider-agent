#!/bin/bash
# Build only from the verified release archive payload; never re-sign its binaries.
set -euo pipefail
[ "$#" -eq 4 ] || { echo 'usage: macos-build.sh PAYLOAD OUTPUT VERSION TARGET' >&2; exit 2; }
payload=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
version=$3
target=$4
here=$(cd "$(dirname "$0")" && pwd)
case "$target" in aarch64-apple-darwin|x86_64-apple-darwin) ;; *) exit 2 ;; esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
work=$(mktemp -d)
mounted=0
keychain_created=0
original_keychains=()
cleanup() {
  if [ "$mounted" = 1 ]; then hdiutil detach "$work/mount" >/dev/null || true; fi
  if [ "$keychain_created" = 1 ]; then
    security list-keychains -d user -s "${original_keychains[@]}" >/dev/null || true
    security delete-keychain "$work/signing.keychain-db" >/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/root/usr/local/bin" "$work/root/usr/local/share/haider" "$work/image"
python3 - "$payload" "$work/root" "$version" "$target" <<'PY'
import hashlib,json,pathlib,re,shutil,sys
p,r=map(pathlib.Path,sys.argv[1:3]); m=json.loads((p/'manifest.json').read_text())
assert (m['version'],m['target']) == tuple(sys.argv[3:])
assert m['members']
for n,d in m['members'].items():
    assert re.fullmatch(r'haider[A-Za-z0-9_-]*',n), n
    assert not (p/n).is_symlink() and hashlib.sha256((p/n).read_bytes()).hexdigest()==d,n
    shutil.copyfile(p/n,r/'usr/local/bin'/n); (r/'usr/local/bin'/n).chmod(0o755)
shutil.copyfile(p/'uninstall-haider.sh',r/'usr/local/bin/uninstall-haider.sh')
(r/'usr/local/bin/uninstall-haider.sh').chmod(0o755)
shutil.copyfile(p/'manifest.json',r/'usr/local/share/haider/installer-manifest.json')
PY
name="haider-v$version-$target"
pkg="$out/$name.pkg"
dmg="$out/$name.dmg"
pkgbuild --root "$work/root" --identifier ai.haidercode.haider --version "$version" --install-location / --ownership recommended "$pkg"
cert=${MACOS_INSTALLER_CERT_P12_BASE64:-${MACOS_SIGNING_CERT_P12_BASE64:-}}
password=${MACOS_INSTALLER_CERT_PASSWORD:-${MACOS_SIGNING_CERT_PASSWORD:-}}
signed=0
if [ -n "$cert" ] && [ -n "$password" ]; then
  while IFS= read -r key; do [ -z "$key" ] || original_keychains+=("$key"); done < <(security list-keychains -d user | sed -E 's/^[[:space:]]*"//; s/"[[:space:]]*$//')
  keypass=$(openssl rand -base64 32)
  printf '%s' "$cert" | base64 -D > "$work/cert.p12"
  security create-keychain -p "$keypass" "$work/signing.keychain-db"
  keychain_created=1
  # GitHub's default job ceiling is 6 hours: 6 * 60 * 60 = 21600 seconds.
  security set-keychain-settings -lut 21600 "$work/signing.keychain-db"
  security unlock-keychain -p "$keypass" "$work/signing.keychain-db"
  security import "$work/cert.p12" -k "$work/signing.keychain-db" -P "$password" -T /usr/bin/productsign -T /usr/bin/security
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keypass" "$work/signing.keychain-db"
  security list-keychains -d user -s "$work/signing.keychain-db" "${original_keychains[@]}"
  identity=$(security find-identity -v -p basic "$work/signing.keychain-db" | sed -n 's/.*"\(Developer ID Installer: [^"]*\)".*/\1/p' | head -1)
  if [ -n "$identity" ]; then
    productsign --sign "$identity" --keychain "$work/signing.keychain-db" --timestamp "$pkg" "$work/signed.pkg"
    mv "$work/signed.pkg" "$pkg"
    pkgutil --check-signature "$pkg"
    signed=1
  else
    echo 'SKIP macOS pkg signing: no Developer ID Installer identity in certificate bundle'
  fi
else
  echo 'SKIP macOS pkg signing: Developer ID Installer certificate secrets absent'
fi
notarize() {
  xcrun notarytool submit "$1" --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait --output-format json > "$work/notary.json"
  python3 - "$work/notary.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); assert r.get('status')=='Accepted',r
PY
  xcrun stapler staple "$1"
  xcrun stapler validate "$1"
}
notarized=0
if [ "$signed" = 1 ] && [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  notarize "$pkg"
  notarized=1
else
  echo 'SKIP macOS notarization: signed pkg or APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID absent'
fi
pkgutil --expand-full "$pkg" "$work/expanded"
python3 "$here/macos-verify.py" "$work/expanded" "$payload" "$version" "$target"
cp "$pkg" "$work/image/"
hdiutil create -volname "Haider $version" -srcfolder "$work/image" -format UDZO "$dmg"
if [ "$notarized" = 1 ]; then notarize "$dmg"; fi
mkdir "$work/mount"
hdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$work/mount"
mounted=1
cmp "$pkg" "$work/mount/$name.pkg"
hdiutil detach "$work/mount"
mounted=0
(cd "$out" && shasum -a 256 "$name.pkg" > "$name.pkg.sha256" && shasum -a 256 "$name.dmg" > "$name.dmg.sha256")
printf 'macOS installer gate PASS signed=%s notarized=%s\n' "$signed" "$notarized"
