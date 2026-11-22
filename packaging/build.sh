#!/bin/bash
# Build btrpkg-git and pacman-btrpkg in a clean Arch container (no root on the host needed), with
# both test suites.
#   [ARCH=x86_64|aarch64] packaging/build.sh [OUTDIR]          (default: packaging/out)
# x86_64 uses archlinux:latest; aarch64 an Arch Linux ARM image (packaging/alarm-image.sh builds
# it; on an x86_64 host docker runs it through qemu, slowly).
# Mirrors: MIRRORLIST=FILE if given; else on x86_64 the host's /etc/pacman.d/mirrorlist when there
# is one (a local pacoloco: --network host), so the packages link against the same library
# versions as the host and the lab; else the mirror Omarchy machines use for that architecture.
# btrpkg-git builds from GitHub unless BTRPKG_SRC is set (git+file:///src uses this checkout,
# mounted read-only).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
out=$(mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd)
arch=${ARCH:-x86_64}
case $arch in
  x86_64)  image=${IMAGE:-archlinux:latest}; platform=linux/amd64; keyring=archlinux
           default='Server = https://mirror.omarchy.org/$repo/os/$arch' ;;
  aarch64) image=${IMAGE:-btrpkg-alarm:latest}; platform=linux/arm64; keyring=archlinuxarm
           default='Server = https://arm-mirror.omarchy.org/$arch/$repo' ;;
  *) echo "ARCH: x86_64 or aarch64" >&2; exit 2 ;;
esac
ml=$(mktemp); trap 'rm -f "$ml"' EXIT
if [[ -n ${MIRRORLIST:-} ]]; then cp "$MIRRORLIST" "$ml"
elif [[ $arch == x86_64 && -r /etc/pacman.d/mirrorlist ]]; then cp /etc/pacman.d/mirrorlist "$ml"
else echo "$default" > "$ml"; fi
chmod 644 "$ml"
docker run --rm --network host --platform "$platform" \
  -v "$repo:/src:ro" -v "$out:/out" -v "$ml:/etc/pacman.d/mirrorlist.host:ro" \
  -e BTRPKG_SRC="${BTRPKG_SRC:-}" -e PACKAGER="${PACKAGER:-btrpkg build.sh}" -e KEYRING="$keyring" -e OWNER="$(id -u):$(id -g)" "$image" bash -euo pipefail -c '
    cp /etc/pacman.d/mirrorlist.host /etc/pacman.d/mirrorlist
    # (a container has no meaningful mount points to check. The download sandbox needs Landlock
    # and a switch to the alpm user, and qemu-user has neither; archlinux:latest itself sets
    # DisableSandboxFilesystem. A throwaway build container does without it.)
    sed -i "s/^CheckSpace/#CheckSpace/" /etc/pacman.conf
    grep -q "^DisableSandbox" /etc/pacman.conf || sed -i "s/^\[options\]/[options]\nDisableSandbox/" /etc/pacman.conf
    if [[ ! -s /etc/pacman.d/gnupg/pubring.gpg ]]; then pacman-key --init >/dev/null 2>&1; pacman-key --populate "$KEYRING" >/dev/null 2>&1; fi
    pacman -Sy --noconfirm --needed "$KEYRING-keyring" >/dev/null
    pacman -Su --noconfirm >/dev/null
    pacman -S --noconfirm --needed base-devel git meson rust fakechroot python >/dev/null
    useradd -m builder
    cp -rL /src/packaging /home/builder/packaging    # -L: the patch symlinks become files
    chown -R builder: /home/builder/packaging
    git config --system --add safe.directory "*"
    b() { su builder -c "cd /home/builder/packaging/$1 && BTRPKG_SRC=\"$BTRPKG_SRC\" PACKAGER=\"$PACKAGER\" makepkg -f --noconfirm $2"; }
    pkgs() { find /home/builder/packaging/$1 -maxdepth 1 -name "*.pkg.tar.*" ! -name "*.sig" ! -name "*-debug-*"; }
    echo "== btrpkg-git"; b btrpkg ""
    pacman -U --noconfirm $(pkgs btrpkg) >/dev/null
    echo "== pacman-btrpkg"; b pacman-btrpkg "" || { cp /home/builder/packaging/pacman-btrpkg/src/pacman/build/meson-logs/testlog.txt /out/pacman-testlog.txt 2>/dev/null; chown -R "$OWNER" /out; exit 1; }
    cp /home/builder/packaging/pacman-btrpkg/src/pacman/build/meson-logs/testlog.txt /out/pacman-testlog.txt
    cp $(pkgs btrpkg) $(pkgs pacman-btrpkg) /out/
    echo "== versions in the build container"; pacman -Q openssl curl libarchive gpgme zstd glibc rust
    chown -R "$OWNER" /out
  '
ls -l "$out"
