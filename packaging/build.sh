#!/bin/bash
# Build btrpkg-git and pacman-btrpkg in a clean Arch container (no root on the host needed).
#   packaging/build.sh [OUTDIR]          (default: packaging/out)
# The container uses the host's mirrorlist (--network host, so a local pacoloco works too), so the
# packages link against the same library versions as the host and the lab. btrpkg-git builds from
# GitHub unless BTRPKG_SRC is set (git+file:///src uses this checkout, mounted read-only).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
out=$(mkdir -p "${1:-$here/out}" && cd "${1:-$here/out}" && pwd)
image=${IMAGE:-archlinux:latest}
docker run --rm --network host \
  -v "$repo:/src:ro" -v "$out:/out" -v /etc/pacman.d/mirrorlist:/etc/pacman.d/mirrorlist.host:ro \
  -e BTRPKG_SRC="${BTRPKG_SRC:-}" "$image" bash -euo pipefail -c '
    cp /etc/pacman.d/mirrorlist.host /etc/pacman.d/mirrorlist
    pacman -Sy --noconfirm --needed archlinux-keyring >/dev/null
    pacman -Su --noconfirm >/dev/null
    pacman -S --noconfirm --needed base-devel git meson rust fakechroot python >/dev/null
    useradd -m builder
    cp -rL /src/packaging /home/builder/packaging    # -L: the patch symlinks become files
    chown -R builder: /home/builder/packaging
    git config --system --add safe.directory "*"
    b() { su builder -c "cd /home/builder/packaging/$1 && BTRPKG_SRC=\"$BTRPKG_SRC\" makepkg -f --noconfirm $2"; }
    echo "== btrpkg-git"; b btrpkg ""
    pacman -U --noconfirm /home/builder/packaging/btrpkg/btrpkg-git-[0-9]*.pkg.tar.zst >/dev/null
    echo "== pacman-btrpkg"; b pacman-btrpkg "" || { cp /home/builder/packaging/pacman-btrpkg/src/pacman/build/meson-logs/testlog.txt /out/ 2>/dev/null; chown -R '"$(id -u):$(id -g)"' /out; exit 1; }
    cp /home/builder/packaging/pacman-btrpkg/src/pacman/build/meson-logs/testlog.txt /out/pacman-testlog.txt
    for f in /home/builder/packaging/*/*.pkg.tar.zst; do case $f in *-debug-*) ;; *) cp "$f" /out/ ;; esac; done
    echo "== versions in the build container"; pacman -Q openssl curl libarchive gpgme zstd glibc rust
    chown -R '"$(id -u):$(id -g)"' /out
  '
ls -l "$out"
