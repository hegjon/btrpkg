#!/bin/bash
# Publish Arch's core and extra databases as thin .btrdb indexes (btrdb/, prototype) into
# ~/Work/btrpkg-repo/btrdb (btrpkg-btrdb.timer, every minute). Its own timer rather than a step
# of mirror-all.sh: a mirror pass that converts packages runs for minutes, and a btrdb that lags
# upstream's database gives its clients an older database than pacman -Sy would.
# Upstream's db comes by conditional GET (a 304 most minutes) into btrdb/.upstream (caddy hides
# dot paths); publish skips a database whose timestamp it already has.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
BD=${BD:-$HOME/Work/btrpkg-repo/btrdb}
B=${B:-$here/../target/release/btrpkg}
UP=${UP:-http://127.0.0.1:9129/repo/archlinux}
rc=0
mkdir -p "$BD/.upstream"
exec 9>"$BD/.upstream/lock"
flock -n 9 || { echo "btrdb: another publish is running"; exit 0; }
for r in core extra; do
  up=$BD/.upstream/$r.db
  args=(-sfL -R -o "$up.part")
  [[ -e $up ]] && args+=(-z "$up")   # If-Modified-Since: a 304 leaves no (or an empty) file
  if ! curl "${args[@]}" "$UP/$r/os/x86_64/$r.db"; then
    rm -f "$up.part"; echo "btrdb: $r: download failed"; rc=1; continue
  fi
  if [[ -s $up.part ]]; then mv "$up.part" "$up"; else rm -f "$up.part"; fi
  out=$(/usr/bin/python3 "$here/../btrdb/btrdb.py" publish --key "$HOME/Work/btrpkg-repo/keys/btrpkg-test.sec" \
          --btrpkg "$B" "$BD" x86_64 "$r" "$up" 2>&1) || { echo "btrdb: $r: publish failed"; rc=1; }
  # "unchanged" every minute would flood the journal
  while IFS= read -r l; do
    [[ $l == *": unchanged" ]] || printf 'btrdb: %s\n' "$l"
  done <<<"$out"
done
exit $rc
