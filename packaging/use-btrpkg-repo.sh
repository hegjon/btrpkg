#!/bin/bash
# Put this machine on the [btrpkg] repository (CI builds from github.com/hegjon/btrpkg) and
# upgrade btrpkg-git and pacman-btrpkg from it. Only [btrpkg]'s database is refreshed: the
# machine's other sync databases (possibly kept old on purpose) are left as they are.
set -euo pipefail
c=/etc/pacman.conf
grep -q '^\[btrpkg\]' $c || printf '\n[btrpkg]\n# btrpkg-git and pacman-btrpkg from CI (github.com/hegjon/btrpkg); not signed yet\nSigLevel = Optional\nServer = https://github.com/hegjon/btrpkg/releases/download/$arch\n' >> $c
t=$(mktemp)
printf '[options]\nArchitecture = auto\n\n[btrpkg]\nSigLevel = Optional\nServer = https://github.com/hegjon/btrpkg/releases/download/$arch\n' > "$t"
pacman --config "$t" -Sy >/dev/null
rm -f "$t"
pacman -S --noconfirm btrpkg/btrpkg-git btrpkg/pacman-btrpkg 2>&1 | grep -E 'upgrading|installing|error|warning' || true
echo "$(hostname): $(pacman -Q btrpkg-git pacman-btrpkg | tr '\n' ' ')$(pacman-btrpkg --version | grep -o 'Pacman v[^ ]*'); btrpkg runs: $(btrpkg 2>&1 | head -1 | cut -c1-20)"
