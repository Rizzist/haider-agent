#!/usr/bin/env bash
# T1 installer lifecycle proof. Isolated Linux containers; no host installation.
# Usage: installers-linux.sh PAYLOAD OUTPUT VERSION TARGET
# Upgrade fixture deliberately uses identical release bytes with older package
# metadata (0.0.0); it tests package upgrade semantics, not historical migration.
set -euo pipefail
if [[ $# != 4 ]]; then
  echo "usage: $0 PAYLOAD OUTPUT VERSION TARGET" >&2
  exit 2
fi
repo=$(cd "$(dirname "$0")/../.." && pwd)
payload=$(cd "$1" && pwd)
mkdir -p "$2"
output=$(cd "$2" && pwd)
version=$3
target=$4
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
  *) echo "unsupported Linux target: $target" >&2; exit 2 ;;
esac
if [[ "$version" == 0.0.0 ]]; then
  echo 'lifecycle upgrade needs a release version greater than synthetic 0.0.0' >&2
  exit 2
fi
for format in deb rpm; do
  if [[ "$format" == deb ]]; then image=ubuntu:24.04; else image=fedora:42; fi
  docker run --rm -i --network bridge \
    --mount "type=bind,src=$repo,dst=/repo,readonly" \
    --mount "type=bind,src=$payload,dst=/payload,readonly" \
    --mount "type=bind,src=$output,dst=/output,readonly" \
    -e FORMAT="$format" -e VERSION="$version" -e TARGET="$target" "$image" bash -s <<'CONTAINER'
set -euo pipefail
if [[ "$FORMAT" == deb ]]; then
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3 dpkg-dev libasound2t64 libglib2.0-0t64
else
  dnf install -y -q python3 rpm-build cpio
fi
python3 /repo/packaging/installers/linux-build.py --payload /payload --output /older \
  --version "$VERSION" --target "$TARGET" --format "$FORMAT" --package-version 0.0.0
old="/older/haider-v0.0.0-$TARGET.$FORMAT"
new="/output/haider-v$VERSION-$TARGET.$FORMAT"
(cd /output && sha256sum --check "$(basename "$new").sha256")
# Refuse to overwrite anything present in the base system.
python3 - <<'PY'
import json
from pathlib import Path
manifest = json.loads(Path('/payload/manifest.json').read_text())
for name in manifest['members']:
    assert not Path('/usr/bin', name).exists(), name
assert not Path('/usr/share/haider').exists()
PY
mkdir -p /root/.haider /home/qa/.haider
printf 'retained root state\n' >/root/.haider/installer-sentinel
printf 'retained user state\n' >/home/qa/.haider/installer-sentinel
check_payload() {
  python3 - <<'PY'
import hashlib
import json
from pathlib import Path
manifest = json.loads(Path('/payload/manifest.json').read_text())
assert Path('/usr/share/haider/manifest.json').read_bytes() == Path('/payload/manifest.json').read_bytes()
for name, sha in manifest['members'].items():
    path = Path('/usr/bin', name)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == sha, name
    assert path.stat().st_mode & 0o111 == 0o111, name
PY
  /usr/bin/haider --version | grep -Fx "haider $VERSION"
}
check_absent() {
  python3 - <<'PY'
import json
from pathlib import Path
for name in json.loads(Path('/payload/manifest.json').read_text())['members']:
    assert not Path('/usr/bin', name).exists(), name
assert not Path('/usr/share/haider').exists()
assert Path('/root/.haider/installer-sentinel').read_text() == 'retained root state\n'
assert Path('/home/qa/.haider/installer-sentinel').read_text() == 'retained user state\n'
PY
}
if [[ "$FORMAT" == deb ]]; then
  # Native architecture container: dependencies are installed by apt.
  apt-get install -y "$old"
  [[ $(dpkg-query -W -f='${Version}' haider) == 0.0.0 ]]
  check_payload
  apt-get install -y "$new"
  [[ $(dpkg-query -W -f='${Version}' haider) == "$VERSION" ]]
  check_payload
  dpkg --remove haider
  check_absent
  dpkg --purge haider
  if dpkg-query -W -f='${Status}' haider 2>/dev/null | grep -q 'installed\|config-files'; then
    echo 'Debian package registration remains after purge' >&2; exit 1
  fi
  check_absent
  # Also exercise direct purge of a currently installed package.
  apt-get install -y "$new"
  dpkg --purge haider
  check_absent
  if dpkg-query -W -f='${Status}' haider 2>/dev/null | grep -q 'installed\|config-files'; then
    echo 'Debian package registration remains after purge' >&2; exit 1
  fi
  if compgen -G '/var/lib/dpkg/info/haider.*' >/dev/null; then
    echo 'Debian package metadata remains after purge' >&2; exit 1
  fi
else
  dnf install -y "$old"
  [[ $(rpm -q --qf '%{VERSION}' haider) == 0.0.0 ]]
  check_payload
  dnf upgrade -y "$new"
  [[ $(rpm -q --qf '%{VERSION}' haider) == "$VERSION" ]]
  check_payload
  # RPM has erase, not a separate purge: no config files or state are owned.
  rpm -e haider
  if rpm -q haider; then
    echo 'RPM package registration remains after erase' >&2; exit 1
  fi
  check_absent
fi
printf 'T1 PASS %s %s: install -> synthetic older-to-release upgrade -> remove/purge; package files and metadata absent; both user state sentinels retained\n' "$FORMAT" "$TARGET"
CONTAINER
done
