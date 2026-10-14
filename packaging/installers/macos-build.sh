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
create_started=0
keychain_created=0
original_keychains=()
mount_is_clear() {
  local work_device mount_device
  work_device=$(stat -f %d "$work") || return 1
  mount_device=$(stat -f %d "$work/mount") || return 1
  [ "$work_device" = "$mount_device" ]
}
# A device may outlive its mount or be recycled. Record identities from this
# attach only, then require both the device and backing path on every audit.
refresh_attached_device() {
  local device
  device=$(python3 -c '
import importlib.util, json, pathlib, plistlib, re, subprocess, sys, time
spec = importlib.util.spec_from_file_location("image_identity", sys.argv[7])
helper = importlib.util.module_from_spec(spec); spec.loader.exec_module(helper)
expected = json.loads(pathlib.Path(sys.argv[8]).read_text())
image_path = pathlib.Path(sys.argv[1]).resolve()
record = pathlib.Path(sys.argv[2])
before = pathlib.Path(sys.argv[5])
snapshot = sys.argv[6] == "snapshot"
saved = json.loads(record.read_text()) if record.exists() and not snapshot else None
attach_devices = []
if saved is None and not snapshot:
    try:
        attached = plistlib.loads(pathlib.Path(sys.argv[3]).read_bytes())
        attach_devices = [entity["dev-entry"] for entity in attached["system-entities"]
                          if re.fullmatch(r"/dev/disk[0-9]+", entity.get("dev-entry", ""))]
    except (OSError, ValueError, KeyError):
        pass  # No plist: only new same-path devices can belong to this attach.
previous = set(json.loads(before.read_text())) if before.exists() else set()
def same_path(path, other):
    if path == other:
        return True
    try:
        return path.samefile(other)
    except OSError:
        return False
for attempt in range(4):
    result = subprocess.run(["hdiutil", "info", "-plist"], capture_output=True)
    if result.returncode:
        sys.stderr.buffer.write(result.stderr)
        sys.exit("hdiutil attachment inventory failed (exit %s)" % result.returncode)
    images = plistlib.loads(result.stdout).get("images", [])
    incomplete = b"cannot get IO media array" in result.stderr
    backing = {entity["dev-entry"]: image for image in images
               for entity in image.get("system-entities", []) if "dev-entry" in entity}
    verified = {}
    def is_expected(device):
        image = backing[device]
        pid = image.get("hdid-pid")
        if pid not in verified:
            verified[pid] = helper.backing_identity(image, expected)
        if verified[pid] is None:
            if not pathlib.Path(device).exists():
                return False
            sys.exit("backing inode unavailable; retaining recorded device " + device)
        return verified[pid]
    identities = [(pathlib.Path(image["image-path"]).resolve(), entity["dev-entry"])
                  for image in images if "image-path" in image
                  for entity in image.get("system-entities", [])
                  if re.fullmatch(r"/dev/disk[0-9]+", entity.get("dev-entry", ""))]
    inventory_devices = {device for path, device in identities}
    if snapshot:
        owned = []
    elif saved is not None:
        owned = [(path, device) for path, device in identities
                 if any(device == item["device"] and same_path(path, pathlib.Path(item["image"]))
                        for item in saved) and is_expected(device)]
    elif attach_devices:
        # Native plists supply devices, not paths. Bind each to the expected
        # open backing vnode; a recycled device cannot establish ownership.
        owned = [(path, device) for path, device in identities
                 if device in attach_devices and device not in previous and is_expected(device)]
    else:
        owned = [(path, device) for path, device in identities
                 if same_path(path, image_path) and device not in previous and is_expected(device)]
    known_devices = [item["device"] for item in saved] if saved is not None else attach_devices
    if any(pathlib.Path(device).exists() and device not in inventory_devices for device in known_devices):
        incomplete = True
    if not owned and not snapshot:
        point = pathlib.Path(sys.argv[4])
        try:
            if point.stat().st_dev != point.parent.stat().st_dev:
                incomplete = True
        except FileNotFoundError:
            pass
    if incomplete:
        if attempt == 3:
            sys.stderr.buffer.write(result.stderr)
            sys.exit("hdiutil attachment inventory remained incomplete; recorded devices: %s" %
                     ", ".join(known_devices))
        time.sleep(0.1 * (attempt + 1))
        continue
    if snapshot:
        before.write_text(json.dumps([device for path, device in identities if same_path(path, image_path)]))
    else:
        if saved is None and not owned and any(device not in previous for path, device in identities
                if device in attach_devices or same_path(path, image_path)):
            sys.exit("cannot attribute attachment to the created backing inode; keeping everything")
        record.write_text(json.dumps([{ "image": str(image_path), "device": device, "identity": expected} for path, device in owned]))
        for path, device in owned:
            print(device)
    sys.exit(0)
' "$dmg" "$work/attached-image.json" "$work/attach.plist" "$work/mount" "$work/before-attach.json" "${1:-refresh}" "$here/macos-image-identity.py" "$work/image-identity.json") || return 1
  attached_device=$device
}
attachment_is_clear() {
  refresh_attached_device || return 1
  [ -z "$attached_device" ] && mount_is_clear
}
detach_owned_image() {
  local status=0
  # Recheck image ownership before using a device name that can be recycled.
  refresh_attached_device || return 1
  [ -n "$attached_device" ] || return 0
  local device devices=$attached_device
  while IFS= read -r device; do
    # Ejecting the backing disk can also remove its synthesized APFS disk.
    # Recheck each remaining identity instead of using a now-stale device list.
    refresh_attached_device || return 1
    case $'\n'"$attached_device"$'\n' in
      *$'\n'"$device"$'\n'*) ;;
      *) continue ;;
    esac
    hdiutil detach "$device" || { status=$?; break; }
  done <<< "$devices"
  if attachment_is_clear; then return 0; fi
  printf 'Owned image still attached as %s after detach\n' "${attached_device//$'\n'/, }" >&2
  if [ "$status" -eq 0 ]; then status=1; fi
  return "$status"
}
approve_staging_from_mount() {
  if [ -e "$work/mount/$name.pkg" ] && /usr/bin/cmp -s "$pkg" "$work/mount/$name.pkg"; then
    python3 "$here/macos-image-identity.py" approve "$dmg" "$work/image-identity.json" "$work" || return 1
  fi
}
cleanup() {
  local keep_work=0
  if [ "$mounted" = 1 ]; then approve_staging_from_mount >/dev/null 2>&1 || true; fi
  if [ "$mounted" = 1 ]; then
    hdiutil_retry detach no_cleanup >/dev/null 2>&1 || true
    if ! attachment_is_clear || ! mount_is_clear; then
      if [ -z "$attached_device" ]; then
        attached_device=$(python3 -c 'import pathlib,plistlib,re,sys; p=pathlib.Path(sys.argv[1]); r=plistlib.loads(p.read_bytes()) if p.exists() else {}; print(", ".join(e["dev-entry"] for e in r.get("system-entities",[]) if re.fullmatch(r"/dev/disk[0-9]+",e.get("dev-entry",""))))' "$work/attach.plist") || attached_device=unknown
      fi
      [ -n "$attached_device" ] || attached_device="unknown (attach plist unavailable)"
      printf 'Could not detach %s (%s); retaining temporary mount for manual cleanup\n' "${attached_device//$'\n'/, }" "$work/mount" >&2
      keep_work=1
    fi
  fi
  if [ "$keychain_created" = 1 ]; then
    security list-keychains -d user -s "${original_keychains[@]}" >/dev/null || true
    security delete-keychain "$work/signing.keychain-db" >/dev/null || true
  fi
  rm -f "$work/cert.p12" "$work/signing.keychain-db"
  if [ -f "$work/retain-create" ]; then
    printf 'Retaining unattributed create output in %s\n' "$work" >&2
    keep_work=1
  fi
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
  local action=$1 diagnostic=$2 status=0
  shift 2
  if [ "$action" = create ]; then
    create_started=0
    if [ -e "$dmg" ] || [ -L "$dmg" ]; then
      printf 'Refusing to replace existing disk image %s\n' "$dmg" >&2
      return 1
    fi
    rm -f "$work/image-identity.json"
    python3 "$here/macos-image-identity.py" create "$dmg" "$work/image-identity.json" "$work" "${@:1:$#-1}" || status=$?
    if [ -f "$work/image-identity.json" ]; then create_started=1; fi
    return "$status"
  fi
  if [ "$action" = attach ]; then
    # Snapshot before attach: a partial failure may return no device plist.
    # Refuse to attach if the pre-existing devices cannot be inventoried.
    python3 "$here/macos-image-identity.py" check "$dmg" "$work/image-identity.json" || return 1
    refresh_attached_device snapshot || return 1
    # Arm the exit trap even when attach creates a device and then returns error.
    mounted=1
    rm -f "$work/attached-image.json"
    hdiutil attach "$@" -plist > "$work/attach.plist" || status=$?
    if ! refresh_attached_device; then
      if [ "$status" -eq 0 ]; then status=1; fi
    fi
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
    # The callback reads diagnostics only after the native command has exited.
    # shellcheck disable=SC2094
    if run_hdiutil "$action" "$diagnostic" "$@" 2>"$diagnostic"; then
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
remove_partial_dmg() {
  [ "$create_started" = 1 ] || return 0
  python3 "$here/macos-image-identity.py" remove "$dmg" "$work/image-identity.json" || return $?
  # A nonempty failed-create output could be another caller's replacement.
  if [ -f "$work/create-unattributed-nonempty" ]; then return 1; fi
  create_started=0
}
no_cleanup() { :; }
detach_partial_mount() {
  approve_staging_from_mount >/dev/null 2>&1 || true
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
exec 9< "$dmg"
python3 "$here/macos-image-identity.py" check "$dmg" "$work/image-identity.json"
if [ "$notarized" = 1 ]; then notarize "$dmg"; fi
mkdir "$work/mount"
hdiutil_retry attach detach_partial_mount "$dmg" -readonly -nobrowse -mountpoint "$work/mount"
mounted=1
python3 "$here/macos-image-identity.py" check "$dmg" "$work/image-identity.json"
cmp "$pkg" "$work/mount/$name.pkg"
python3 "$here/macos-image-identity.py" approve "$dmg" "$work/image-identity.json" "$work"
python3 "$here/macos-image-identity.py" check "$dmg" "$work/image-identity.json"
hdiutil_retry detach no_cleanup
mounted=0
python3 "$here/macos-image-identity.py" finish "$dmg" "$work/image-identity.json" "$work"
(cd "$out" && shasum -a 256 "$name.pkg" > "$name.pkg.sha256")
python3 "$here/macos-image-identity.py" checksum "$dmg" "$work/image-identity.json" "$dmg.sha256"
printf 'macOS installer gate PASS signed=%s notarized=%s\n' "$signed" "$notarized"
