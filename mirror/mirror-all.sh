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
# Arch's core and extra databases as thin .btrdb indexes (btrdb/, prototype; the p14s trial
# syncs them); upstream's db by conditional GET into btrdb/.upstream (caddy hides dot paths);
# not counted in rc
if [[ -z ${NO_BTRDB:-} ]]; then
  BD=$HOME/Work/btrpkg-repo/btrdb
  mkdir -p "$BD/.upstream"
  for r in core extra; do
    up=$BD/.upstream/$r.db
    args=(-sfL -R -o "$up.part")
    [[ -e $up ]] && args+=(-z "$up")   # If-Modified-Since: a 304 leaves no (or an empty) file
    if ! curl "${args[@]}" "http://127.0.0.1:9129/repo/archlinux/$r/os/x86_64/$r.db"; then
      rm -f "$up.part"; printf '%s btrdb: %s: download failed\n' "$(date '+%F %T')" "$r"; continue
    fi
    if [[ -s $up.part ]]; then mv "$up.part" "$up"; else rm -f "$up.part"; fi
    while IFS= read -r l; do printf '%s btrdb: %s\n' "$(date '+%F %T')" "$l"; done \
      < <(/usr/bin/python3 "$here/../btrdb/btrdb.py" publish --key "$HOME/Work/btrpkg-repo/keys/btrpkg-test.sec" \
            --btrpkg "${B:-$here/../target/release/btrpkg}" "$BD" x86_64 "$r" "$up" 2>&1 || echo "$r: failed")
  done
fi
exit $rc
