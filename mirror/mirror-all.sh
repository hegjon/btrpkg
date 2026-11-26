#!/bin/bash
# Run repo-mirror.sh for every repository in repos.conf, one after the other (btrpkg-mirror.timer).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rc=0
while read -r repo up dir keyring want parts; do
  [[ -z $repo || $repo == \#* ]] && continue
  [[ $keyring == /* ]] || keyring=$here/$keyring
  REPO=$repo UP=$up M=$HOME/Work/btrpkg-repo/$dir KEYRING=$keyring WANT=$here/$want PARTS=${parts:-../pkg} "$here/repo-mirror.sh" || { echo "$repo: failed"; rc=1; }
done < "$here/repos.conf"
# cleanup, once per btrparts directory (shared by several repositories) and only after all of
# them: current + previous version of each package stay (btrpkg gc, see src/gc.rs); skipped
# when a repository failed, so a half-updated database can't make its new versions look unused
if [[ $rc == 0 && -z ${NO_GC:-} ]]; then
  B=${B:-$here/../target/release/btrpkg}
  mapfile -t pdirs < <(while read -r repo up dir keyring want parts; do
    [[ -z $repo || $repo == \#* ]] && continue
    realpath -m "$HOME/Work/btrpkg-repo/$dir/${parts:-../pkg}"
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
exit $rc
