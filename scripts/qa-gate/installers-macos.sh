#!/bin/bash
# Opt-in local T1 lifecycle. Requires a disposable macOS account/runner and sudo.
set -euo pipefail
[ "$#" -eq 3 ] || { echo 'usage: installers-macos.sh FIRST.pkg UPGRADE.pkg EXPECTED-PAYLOAD' >&2; exit 2; }
[ "${HAIDER_INSTALLER_QA_ALLOW_SYSTEM:-}" = 1 ] || { echo 'Refusing system install: set HAIDER_INSTALLER_QA_ALLOW_SYSTEM=1 on a disposable macOS runner' >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || exit 2
first=$1
upgrade=$2
payload=$(cd "$3" && pwd)
here=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
started=0
missing_dirs=()
cleanup() {
  status=$?
  if [ "$started" = 1 ]; then
    if [ -x /usr/local/bin/uninstall-haider.sh ]; then sudo /usr/local/bin/uninstall-haider.sh --keep-state || true; fi
    if [ -f "$HOME/.haider/installer-qa-sentinel" ]; then
      rm "$HOME/.haider/installer-qa-sentinel"
      rmdir "$HOME/.haider" 2>/dev/null || true
    fi
    for ((i=${#missing_dirs[@]}-1; i>=0; i--)); do sudo rmdir "${missing_dirs[$i]}" 2>/dev/null || true; done
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT
# Verify both inputs before allowing privileged installation, including exact files.
pkgutil --expand-full "$upgrade" "$work/upgrade"
read -r version target < <(python3 - "$payload/manifest.json" <<'PY'
import json,sys
m=json.load(open(sys.argv[1])); print(m['version'],m['target'])
PY
)
python3 "$here/packaging/installers/macos-verify.py" "$work/upgrade" "$payload" "$version" "$target"
pkgutil --expand-full "$first" "$work/first"
# First package may be a previous version; still reject unexpected installation paths/scripts.
python3 - "$work/first" "$work/upgrade" > "$work/paths" <<'PY'
import pathlib,sys,xml.etree.ElementTree as E
paths=set()
versions=[E.parse(pathlib.Path(p)/'PackageInfo').getroot().get('version') for p in sys.argv[1:]]
assert tuple(map(int,versions[0].split('.'))) < tuple(map(int,versions[1].split('.'))), 'first pkg must be an older version for a real upgrade'
for root in map(pathlib.Path,sys.argv[1:]):
    info=E.parse(root/'PackageInfo').getroot()
    assert info.get('identifier')=='ai.haidercode.haider' and info.get('install-location')=='/'
    assert not (root/'Scripts').exists()
    for p in (root/'Payload').rglob('*'):
        if not p.is_dir() or p.is_symlink():
            assert not p.is_symlink()
            n=str(p.relative_to(root/'Payload'))
            assert (n.startswith('usr/local/bin/haider') and '/' not in n[len('usr/local/bin/'):]) or n in ('usr/local/bin/uninstall-haider.sh','usr/local/share/haider/installer-manifest.json'),n
            paths.add('/'+n)
for p in sorted(paths): print(p)
PY
while IFS= read -r path; do
  [ ! -e "$path" ] && [ ! -L "$path" ] || { echo "Refusing collision: $path" >&2; exit 2; }
done < "$work/paths"
if pkgutil --pkg-info ai.haidercode.haider >/dev/null 2>&1; then echo 'Refusing existing receipt' >&2; exit 2; fi
for path in "$HOME/.haider" "$HOME/Library/LaunchAgents/ai.haidercode.haider.plist" "$HOME/Library/LaunchAgents/ai.haidercode.haiderd.plist" /Library/LaunchAgents/ai.haidercode.haider.plist /Library/LaunchAgents/ai.haidercode.haiderd.plist /usr/local/share/haider; do
  [ ! -e "$path" ] && [ ! -L "$path" ] || { echo "Refusing existing state/agent/directory: $path" >&2; exit 2; }
done
# Record pre-existing parent directories; remove only those this QA run creates.
missing_dirs=()
for path in /usr/local /usr/local/bin /usr/local/share; do [ -d "$path" ] || missing_dirs+=("$path"); done
started=1
mkdir "$HOME/.haider"
printf 'installer-qa-preserve\n' > "$HOME/.haider/installer-qa-sentinel"
receipt_matches() {
  pkgutil --pkg-info-plist ai.haidercode.haider > "$work/receipt.plist"
  python3 - "$1/PackageInfo" "$work/receipt.plist" <<'PYCODE'
import plistlib,sys,xml.etree.ElementTree as E
assert E.parse(sys.argv[1]).getroot().get('version') == plistlib.load(open(sys.argv[2],'rb'))['pkg-version']
PYCODE
}
sudo installer -pkg "$first" -target /
receipt_matches "$work/first"
sudo installer -pkg "$upgrade" -target /
receipt_matches "$work/upgrade"
python3 - "$payload/manifest.json" <<'PY'
import hashlib,json,pathlib,sys
m=json.load(open(sys.argv[1]))
for name,digest in m['members'].items():
    assert hashlib.sha256((pathlib.Path('/usr/local/bin')/name).read_bytes()).hexdigest()==digest,name
PY
sudo /usr/local/bin/uninstall-haider.sh --keep-state
while IFS= read -r path; do
  if [ -e "$path" ] || [ -L "$path" ]; then echo "Uninstall left $path" >&2; exit 1; fi
done < "$work/paths"
if pkgutil --pkg-info ai.haidercode.haider >/dev/null 2>&1; then
  echo 'Uninstall left package receipt' >&2; exit 1
fi
[ "$(cat "$HOME/.haider/installer-qa-sentinel")" = installer-qa-preserve ]
rm "$HOME/.haider/installer-qa-sentinel"
rmdir "$HOME/.haider"
for ((i=${#missing_dirs[@]}-1; i>=0; i--)); do sudo rmdir "${missing_dirs[$i]}"; done
started=0
echo 'T1 macOS PASS: install -> upgrade -> uninstall; files/receipt removed, state preserved, parent directories restored'
