#!/bin/bash
# Build btrpkg-git and pacman-btrpkg on a machine whose system can't take new packages (stale
# sync databases, no partial upgrades on a lab machine): rust (rustup) and meson/ninja (a venv)
# live in one directory, makepkg runs with --nodeps, the packages link against the machine's own
# libraries. Run as the build user, one package at a time (install btrpkg-git before building
# pacman-btrpkg, whose tests run it):
#   bash build-local-toolchain.sh PKGDIR btrpkg|pacman-btrpkg [MAKEPKG ARGS]
#   PKGDIR: a copy of packaging/ with btrpkg/ and pacman-btrpkg/ (symlinks resolved)
# Remove $T afterwards (rm -rf ~/.btrpkg-toolchain) to leave the machine as it was.
set -euo pipefail
P=$(cd "$1" && pwd)
T=$HOME/.btrpkg-toolchain
export RUSTUP_HOME=$T/rustup CARGO_HOME=$T/cargo
if [[ ! -x $CARGO_HOME/bin/cargo ]]; then
  mkdir -p "$T"
  curl -sSf https://static.rust-lang.org/rustup/dist/"$(uname -m)"-unknown-linux-gnu/rustup-init -o "$T/rustup-init"
  chmod +x "$T/rustup-init"
  "$T/rustup-init" -y --no-modify-path --profile minimal --default-toolchain stable >/dev/null
fi
if [[ ! -x $T/venv/bin/meson ]]; then
  python3 -m venv "$T/venv"
  "$T/venv/bin/pip" install -q meson ninja
fi
export PATH=$CARGO_HOME/bin:$T/venv/bin:$PATH
echo "toolchain: $(rustc --version), meson $(meson --version), ninja $(ninja --version)"
# more arguments go to makepkg (--nocheck: without fakechroot, pacman's hook/scriptlet/ldconfig
# tests, 19 of them, can't chroot as a user)
cd "$P/$2" && makepkg -f --noconfirm --nodeps "${@:3}"
