#!/bin/bash
# zchunk vs btrpkg for upgrades: each pair's package files as .tar.zck (zck 1.5.5 defaults:
# content-defined chunks, zstd, no dictionary), then zck_delta_size old new. Runs in an archlinux
# container, no root on the host; inputs (the .pkg.tar.zst pairs below) in the current directory:
#   docker run --rm --network host -v /etc/pacman.d/mirrorlist:/etc/pacman.d/mirrorlist:ro \
#     -v $PWD:/w archlinux:latest bash /w/zchunk-compare.sh
# btrpkg side: convert old then new into an empty repo per pair; "N new (BYTES), index BYTES".
set -euo pipefail
pacman -Syu --noconfirm zchunk zstd >/dev/null 2>&1
zck --version | head -1
cd /w; mkdir -p out
pairs="aquamarine-0.15.1-1 aquamarine-0.16.0-1
hyprcursor-0.1.13-7 hyprcursor-0.1.14-1
hyprgraphics-0.5.1-4 hyprgraphics-0.6.0-1
hyprland-0.57.1-2 hyprland-0.57.2-2
hyprland-guiutils-0.2.2-5 hyprland-guiutils-0.2.2-6
hyprpicker-0.4.7-4 hyprpicker-0.4.8-1
hyprtoolkit-0.6.0-2 hyprtoolkit-0.6.0-3
hyprutils-0.14.3-1 hyprutils-0.14.4-1
xdg-desktop-portal-hyprland-1.4.1-2 xdg-desktop-portal-hyprland-1.5.0-1
linux-omarchy-headers-7.2.15-3 linux-omarchy-headers-7.2.15-4"
mk() { # $1 stem: .tar, .zck (default), .zck with -19
  [[ -e out/$1.tar ]] || zstd -qdc "$1-x86_64.pkg.tar.zst" > out/$1.tar
  [[ -e out/$1.tar.zck ]] || zck -o out/$1.tar.zck out/$1.tar
}
printf 'pkg\tpkgzst_new\tzck_new\tzck_delta\n'
echo "$pairs" | while read -r a b; do
  mk "$a"; mk "$b"
  d=$(zck_delta_size out/$a.tar.zck out/$b.tar.zck 2>&1 | tr '\n' ' ')
  printf '%s\t%s\t%s\t%s\n' "$b" "$(stat -c %s $b-x86_64.pkg.tar.zst)" "$(stat -c %s out/$b.tar.zck)" "$d"
done
chown -R 1000:1000 out
