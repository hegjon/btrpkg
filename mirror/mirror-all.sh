#!/bin/bash
# Run repo-mirror.sh for every repository in repos.conf, one after the other (btrpkg-mirror.timer).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rc=0
while read -r repo up dir keyring want parts; do
  [[ -z $repo || $repo == \#* ]] && continue
  [[ $keyring == /* ]] || keyring=$here/$keyring
  REPO=$repo UP=$up M=$HOME/Work/btrpkg-repo/$dir KEYRING=$keyring WANT=$here/$want PARTS=${parts:-../btrparts} "$here/repo-mirror.sh" || { echo "$repo: failed"; rc=1; }
done < "$here/repos.conf"
# cleanup, once per btrparts directory (shared by several repositories) and only after all of
# them: current + previous version of each package stay (btrpkg gc, see src/gc.rs); skipped
# when a repository failed, so a half-updated database can't make its new versions look unused
if [[ $rc == 0 && -z ${NO_GC:-} ]]; then
  B=${B:-$here/../target/release/btrpkg}
  mapfile -t pdirs < <(while read -r repo up dir keyring want parts; do
    [[ -z $repo || $repo == \#* ]] && continue
    realpath -m "$HOME/Work/btrpkg-repo/$dir/${parts:-../btrparts}"
  done < "$here/repos.conf" | sort -u)
  for p in "${pdirs[@]}"; do
    [[ -d $p ]] || continue
    out=$("$B" gc --keep "${GC_KEEP:-2}" "$p" 2>&1) || rc=1
    # printed by this shell, not a short-lived sed: journalctl -u attributes those lines poorly
    while IFS= read -r l; do
      [[ $l == removed\ * ]] || printf '%s %s: %s\n' "$(date '+%F %T')" "${p#"$HOME"/Work/btrpkg-repo/}" "$l"
    done <<<"$out"
  done
fi
# Arch's core and extra databases as zchunk files (zdb/, prototype); not counted in rc
if [[ -z ${NO_ZDB:-} && -x $HOME/.local/opt/zchunk/bin/zck ]]; then
  while IFS= read -r l; do printf '%s zdb: %s\n' "$(date '+%F %T')" "$l"; done \
    < <("$here/../zdb/zdb-publish.sh" "$HOME/Work/btrpkg-repo/zdb" http://127.0.0.1:9129/repo/archlinux core extra 2>&1 || echo "failed")
fi
exit $rc
