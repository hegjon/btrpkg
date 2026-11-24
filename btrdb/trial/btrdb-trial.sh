#!/bin/bash
# btrdb sync-database trial on a lab client (root, from btrdb-trial.timer): keeps two copies of
# core.db and extra.db up to date from the desktop's .btrdb indexes (btrdb.py sync, from
# /opt/btrdb), one synced every hour and one once a day, and logs what each sync downloaded
# against what pacman -Sy would have at the same moment (the whole db from pacoloco, if its
# Last-Modified changed since that copy's last sync). Touches nothing of the system's pacman.
#
# Log /var/log/btrdb-trial.tsv: time, copy (hourly|daily), repo, btrdb bytes received, requests,
# pacman bytes, seconds, check, how (patch: an index patch against the copy's last index; full:
# the whole index; unchanged), full_bytes (that sync with the whole index instead of a patch).
# check: "ok" when the rebuilt db has the same entries and contents as Arch's (pacoloco's) of the
# same Last-Modified, "skew" when pacoloco is already newer (the mirror publishes every 10
# minutes), "unchanged" when nothing was downloaded.
# Stops itself (disables the timer) after STOP (date -d format), default 2026-11-26 12:00.
set -uo pipefail
BTRDB=${BTRDB:-http://192.168.2.2:1764/btrdb/x86_64}
UP=${UP:-http://desktop.lab:9129/repo/archlinux}
STATE=${STATE:-/var/lib/btrdb-trial}
LOG=${LOG:-/var/log/btrdb-trial.tsv}
STOP=${STOP:-2026-11-26 12:00}
if (( $(date +%s) > $(date -d "$STOP" +%s) )); then
  systemctl disable --now btrdb-trial.timer; echo "trial over ($STOP)"; exit 0
fi
[[ -e $LOG ]] || printf 'time\tcopy\trepo\tbtrdb_bytes\trequests\tpacman_bytes\tseconds\tcheck\thow\tfull_bytes\n' > "$LOG"
head_of() { curl -sfI "$1" | tr -d '\r'; }
copies=(hourly)
stamp=$STATE/daily/.last-sync
if [[ ! -e $stamp ]] || (( $(date +%s) - $(stat -c %Y "$stamp") >= 86000 )); then copies+=(daily); fi
for c in "${copies[@]}"; do
  d=$STATE/$c
  mkdir -p "$d"
  for r in core extra; do
    db=$UP/$r/os/x86_64/$r.db
    out=$(/usr/bin/python3 /opt/btrdb/btrdb.py sync --measure-full --keys /etc/btrpkg/keys --btrpkg /usr/bin/btrpkg "$BTRDB" "$d" "$r" 2>&1)
    ok=$?
    # pacman -Sy at the same moment: the whole db when its Last-Modified moved since the last sync
    h=$(head_of "$db")
    upl=$(sed -n 's/^[Ll]ast-[Mm]odified: //p' <<<"$h")
    prev=$(cat "$d/$r.pacman-lm" 2>/dev/null)
    plen=0
    if [[ $upl != "$prev" ]]; then plen=$(sed -n 's/^[Cc]ontent-[Ll]ength: //p' <<<"$h"); echo "$upl" > "$d/$r.pacman-lm"; fi
    if (( ok != 0 )); then
      printf '%s\t%s\t%s\t\t\t%s\t\tFAILED %s\n' "$(date '+%F %T')" "$c" "$r" "$plen" "${out//$'\n'/ }" >> "$LOG"
      continue
    fi
    IFS=$'\t' read -r _ got req secs how full <<<"$out"
    check=unchanged
    if [[ $how != unchanged ]]; then
      if [[ -n $upl && $(date -d "$upl" +%s) == $(stat -c %Y "$d/$r.db") ]]; then
        curl -sf -o "$d/.$r.upstream" "$db"
        if /usr/bin/python3 /opt/btrdb/btrdb.py compare "$d/$r.db" "$d/.$r.upstream" >/dev/null; then check=ok; else check=MISMATCH; fi
        rm -f "$d/.$r.upstream"
      else
        check=skew
      fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$c" "$r" "$got" "$req" "$plen" "$secs" "$check" "$how" "$full" >> "$LOG"
  done
  if [[ $c == daily ]]; then touch "$stamp"; fi
done
