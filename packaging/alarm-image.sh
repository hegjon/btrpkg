#!/bin/bash
# Docker image btrpkg-alarm:latest from Arch Linux ARM's aarch64 root filesystem, for
# ARCH=aarch64 packaging/build.sh. The tarball's signature is checked against Arch Linux ARM's
# key (mirror/keyrings/archlinuxarm.gpg, from archlinuxarm-keyring).
#   packaging/alarm-image.sh [IMAGE]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
image=${1:-btrpkg-alarm:latest}
base=${ALARM_URL:-http://os.archlinuxarm.org/os}
t=ArchLinuxARM-aarch64-latest.tar.gz
w=$(mktemp -d); trap 'rm -rf "$w"' EXIT
curl -fsSL -o "$w/$t" "$base/$t"
curl -fsSL -o "$w/$t.sig" "$base/$t.sig"
gpg --dearmor < "$here/../mirror/keyrings/archlinuxarm.gpg" > "$w/alarm.kr"
gpgv --keyring "$w/alarm.kr" "$w/$t.sig" "$w/$t"
docker import --platform linux/arm64 "$w/$t" "$image" >/dev/null
docker run --rm --platform linux/arm64 "$image" uname -m
