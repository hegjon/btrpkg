#!/bin/bash
# Publish built packages as a pacman repository on GitHub releases, one release per architecture
# (tag = the architecture), the way asahi-alarm serves its repository:
#   [btrpkg]
#   Server = https://github.com/hegjon/btrpkg/releases/download/$arch
#   packaging/publish-release-repo.sh DIR       (DIR/packages-ARCH/*.pkg.tar.*, from CI artifacts)
# Needs gh (GH_TOKEN) and docker (repo-add runs in archlinux:latest). Per architecture:
# - the release's btrpkg.db is the starting point; only package files whose name isn't in it
#   are added (a rebuild of the same version would change a file clients may have cached)
# - the new db, files db and package files are uploaded, then package files that neither the new
#   db nor the old one names are deleted: current + previous version stay, as on the mirror
# Packages are not signed yet (no key decided): clients need `SigLevel = Optional` for [btrpkg].
set -euo pipefail
src=$(cd "$1" && pwd)
gh_repo=${GH_REPO:-hegjon/btrpkg}
# FILENAMEs in a repo db (bsdtar, or GNU tar + zstd as on the CI runner)
filenames() {
  if command -v bsdtar >/dev/null; then bsdtar -xOf "$1" '*/desc'; else zstd -dc "$1" | tar -xOf - --wildcards '*/desc'; fi |
    awk '/^%FILENAME%$/{getline; print}'
}
for d in "$src"/packages-*; do
  arch=${d##*/packages-}
  w=$(mktemp -d)
  echo "== $arch"
  gh release view "$arch" -R "$gh_repo" >/dev/null 2>&1 ||
    gh release create "$arch" -R "$gh_repo" --title "btrpkg repository ($arch)" --latest=false \
      --notes "pacman repository for btrpkg-git and pacman-btrpkg ($arch), built and tested by CI. Server = https://github.com/$gh_repo/releases/download/\$arch (SigLevel = Optional: packages are not signed yet)."
  # a release without a database starts one; a database that can't be downloaded stops the run
  # (building it again from this build's packages alone would drop, and delete, all the others)
  if gh release view "$arch" -R "$gh_repo" --json assets -q '.assets[].name' | grep -qxF btrpkg.db; then
    gh release download "$arch" -R "$gh_repo" -p btrpkg.db -D "$w"
    mv "$w/btrpkg.db" "$w/btrpkg.db.tar.zst"
  fi
  old=$( [[ -s $w/btrpkg.db.tar.zst ]] && filenames "$w/btrpkg.db.tar.zst" || true)
  new=()
  for f in "$d"/*.pkg.tar.*; do
    [[ $f == *.sig ]] && continue
    b=${f##*/}
    if grep -qxF "$b" <<<"$old"; then echo "unchanged: $b"; else cp "$f" "$w/"; new+=("$b"); fi
  done
  if (( ${#new[@]} == 0 )); then echo "nothing new"; rm -rf "$w"; continue; fi
  docker run --rm --user "$(id -u):$(id -g)" -v "$w:/r" -w /r archlinux:latest repo-add -q btrpkg.db.tar.zst "${new[@]}"
  for x in db files; do rm -f "$w/btrpkg.$x"; cp "$w/btrpkg.$x.tar.zst" "$w/btrpkg.$x"; done
  cur=$(filenames "$w/btrpkg.db.tar.zst")
  (cd "$w" && gh release upload "$arch" -R "$gh_repo" --clobber "${new[@]}" btrpkg.db btrpkg.db.tar.zst btrpkg.files btrpkg.files.tar.zst)
  echo "added: ${new[*]}"
  gh release view "$arch" -R "$gh_repo" --json assets -q '.assets[].name' | while read -r a; do
    [[ $a == *.pkg.tar.* ]] || continue
    grep -qxF "$a" <<<"$cur" && continue
    grep -qxF "$a" <<<"$old" && continue
    gh release delete-asset "$arch" "$a" -R "$gh_repo" -y && echo "deleted: $a"
  done
  rm -rf "$w"
done
