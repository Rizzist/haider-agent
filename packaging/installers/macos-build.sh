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
attached_device=
keychain_created=0
original_keychains=()
mount_is_clear() {
  local work_device mount_device
  work_device=$(stat -f %d "$work") || return 1
  mount_device=$(stat -f %d "$work/mount") || return 1
  [ "$work_device" = "$mount_device" ]
}
# hdiutil can unmount a volume before an eject fails. Keep the whole-disk
# identity until the owned image disappears from the attachment inventory.
refresh_attached_device() {
  local device
  device=$(python3 -c '
import json, pathlib, plistlib, re, subprocess, sys, time
image_path = pathlib.Path(sys.argv[1]).resolve()
record = pathlib.Path(sys.argv[2])
attach_device = None
if record.exists():
    saved = json.loads(record.read_text())
    image_path = pathlib.Path(saved["image"])
    attach_device = saved["device"]
else:
    try:
        attached = plistlib.loads(pathlib.Path(sys.argv[3]).read_bytes())
        attach_device = next(entity["dev-entry"] for entity in attached["system-entities"]
                             if re.fullmatch(r"/dev/disk[0-9]+", entity.get("dev-entry", "")))
    except (OSError, ValueError, KeyError, StopIteration):
        pass  # A failed attach may emit no plist; recover by exact image path.
for attempt in range(4):
    result = subprocess.run(["hdiutil", "info", "-plist"], capture_output=True)
    result.check_returncode()
    images = plistlib.loads(result.stdout).get("images", [])
    incomplete = b"cannot get IO media array" in result.stderr
    matched = None
    device_in_inventory = False
    for image in images:
        current_path = pathlib.Path(image.get("image-path", "")).resolve()
        entities = image.get("system-entities", [])
        same_image = current_path == image_path
        if not same_image:
            try:
                same_image = current_path.samefile(image_path)
            except OSError:
                pass
        same_device = attach_device and any(entity.get("dev-entry") == attach_device
                                            for entity in entities)
        device_in_inventory |= bool(same_device)
        # Only initial attach output may establish the backing path by device.
        # Once recorded, a recycled device on another image conveys no ownership.
        if same_image or (not record.exists() and same_device):
            matched = (current_path, entities)
    if not matched and attach_device and pathlib.Path(attach_device).exists() and not device_in_inventory:
        incomplete = True
    if incomplete:
        if attempt == 3:
            sys.stderr.buffer.write(result.stderr)
            sys.exit("hdiutil attachment inventory remained incomplete")
        time.sleep(0.1 * (attempt + 1))
        continue
    if matched:
        current_path, entities = matched
        device = next((entity["dev-entry"] for entity in entities
                       if re.fullmatch(r"/dev/disk[0-9]+", entity.get("dev-entry", ""))), None)
        if not device:
            sys.exit("owned image is attached without a whole-disk device")
        record.write_text(json.dumps({"image": str(current_path), "device": device}))
        print(device)
    else:
        # Discard stale attach output after the first complete inventory.
        record.write_text(json.dumps({"image": str(image_path), "device": None}))
    sys.exit(0)
' "$dmg" "$work/attached-image.json" "$work/attach.plist") || return 1
  attached_device=$device
}
attachment_is_clear() {
  refresh_attached_device || return 1
  [ -z "$attached_device" ]
}
detach_owned_image() {
  local status=0
  # Recheck image ownership before using a device name that can be recycled.
  refresh_attached_device || return 1
  [ -n "$attached_device" ] || return 0
  hdiutil detach "$attached_device" || status=$?
  if attachment_is_clear; then return 0; fi
  if [ "$status" -eq 0 ]; then status=1; fi
  return "$status"
}
cleanup() {
  local keep_work=0
  if [ "$mounted" = 1 ]; then
    detach_owned_image >/dev/null 2>&1 || true
    if ! attachment_is_clear || ! mount_is_clear; then
      printf 'Could not detach %s (%s); retaining temporary mount for manual cleanup\n' "$attached_device" "$work/mount" >&2
      keep_work=1
    fi
  fi
  if [ "$keychain_created" = 1 ]; then
    security list-keychains -d user -s "${original_keychains[@]}" >/dev/null || true
    security delete-keychain "$work/signing.keychain-db" >/dev/null || true
  fi
  rm -f "$work/cert.p12" "$work/signing.keychain-db"
  if [ "$keep_work" = 0 ]; then rm -rf "$work"; fi
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
run_hdiutil() {
  local action=$1 status=0
  shift
  if [ "$action" = attach ]; then
    # Arm the exit trap even when attach creates a device and then returns error.
    mounted=1
    rm -f "$work/attached-image.json"
    hdiutil attach "$@" -plist > "$work/attach.plist" || status=$?
    refresh_attached_device || return 1
    return "$status"
  fi
  if [ "$action" = detach ]; then
    detach_owned_image
    return $?
  fi
  hdiutil "$action" "$@"
}
hdiutil_retry() {
  local action=$1 cleanup_attempt=$2 attempt status diagnostic retryable
  shift 2
  for ((attempt=1; attempt<=4; attempt++)); do
    diagnostic=$(mktemp "$work/hdiutil.XXXXXX")
    if run_hdiutil "$action" "$@" 2>"$diagnostic"; then
      cat "$diagnostic" >&2
      rm -f "$diagnostic"
      return 0
    else
      status=$?
    fi
    retryable=0
    if [ "$status" -eq 16 ] || LC_ALL=C grep -Eiq 'resource busy|device busy|resource temporarily unavailable|(^|[^[:alnum:]_])ebusy([^[:alnum:]_]|$)' "$diagnostic"; then
      retryable=1
    fi
    # Negated conditionals suppress set -e inside the callback: it must return
    # failure explicitly when cleanup fails, so the original status survives.
    if ! "$cleanup_attempt"; then
      cat "$diagnostic" >&2
      rm -f "$diagnostic"
      return "$status"
    fi
    cat "$diagnostic" >&2
    rm -f "$diagnostic"
    if [ "$retryable" -eq 0 ]; then return "$status"; fi
    if [ "$attempt" -eq 4 ]; then
      printf 'hdiutil %s failed after %s attempts (last exit %s)\n' "$action" "$attempt" "$status" >&2
      return "$status"
    fi
    sleep "$((1 << (attempt - 1)))"
  done
}
remove_partial_dmg() { rm -f "$dmg"; }
no_cleanup() { :; }
detach_partial_mount() {
  detach_owned_image >/dev/null 2>&1 || true
  if attachment_is_clear; then
    mounted=0
    return 0
  fi
  mounted=1
  # Device equality proves only that the mount point is clear, not that the
  # image was ejected. Retry an unmounted attachment too; package presence is
  # used only when the volume is still mounted.
  if mount_is_clear || [ -e "$work/mount/$name.pkg" ]; then
    hdiutil_retry detach no_cleanup || true
    if attachment_is_clear; then
      mounted=0
      return 0
    fi
  fi
  return 1
}
hdiutil_retry create remove_partial_dmg -volname "Haider $version" -srcfolder "$work/image" -format UDZO "$dmg"
if [ "$notarized" = 1 ]; then notarize "$dmg"; fi
mkdir "$work/mount"
hdiutil_retry attach detach_partial_mount "$dmg" -readonly -nobrowse -mountpoint "$work/mount"
mounted=1
cmp "$pkg" "$work/mount/$name.pkg"
hdiutil_retry detach no_cleanup
mounted=0
(cd "$out" && shasum -a 256 "$name.pkg" > "$name.pkg.sha256" && shasum -a 256 "$name.dmg" > "$name.dmg.sha256")
printf 'macOS installer gate PASS signed=%s notarized=%s\n' "$signed" "$notarized"
