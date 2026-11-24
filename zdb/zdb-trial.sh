#!/bin/bash
# zchunk database trial on a lab client (root, from zdb-trial.timer): keeps two copies of core.db
# and extra.db up to date from the desktop's .zck files, one synced every hour and one once a day,
# and logs what each sync downloaded against what pacman -Sy would have at the same moment (the
# whole gzip'd db from pacoloco, if its Last-Modified changed since that copy's last sync).
# Touches nothing of the system's pacman.
#
# Log /var/log/zdb-trial.tsv: time, copy (hourly|daily), repo, zck bytes received, pacman bytes,
# seconds, check. check: "ok" when the rebuilt db has the same entries and contents as Arch's
# (pacoloco's) of the same Last-Modified, "skew" when pacoloco is already newer (the mirror
# publishes every 10 minutes), "unchanged" when nothing was downloaded.
# Since 2026-11-24 11:00 the same moments also sync btrdb copies (btrdb/btrdb.py, thin .btrdb
# index + desc blobs, from /opt/btrdb) into STATE/hourly-btrdb and STATE/daily-btrdb, logged to
# /var/log/btrdb-trial.tsv: time, copy, repo, btrdb bytes received, requests, pacman bytes (the
# same figure as the zdb row), seconds, check (as above).
# Stops itself (disables the timer) after STOP (date -d format), default 2026-11-26 12:00.
set -uo pipefail
ZDB=${ZDB:-http://192.168.2.2:1764/zdb}
UP=${UP:-http://desktop.lab:9129/repo/archlinux}
STATE=${STATE:-/var/lib/zdb-trial}
LOG=${LOG:-/var/log/zdb-trial.tsv}
BTRDB=${BTRDB:-http://192.168.2.2:1764/btrdb/x86_64}
BLOG=${BLOG:-/var/log/btrdb-trial.tsv}
STOP=${STOP:-2026-11-26 12:00}
if (( $(date +%s) > $(date -d "$STOP" +%s) )); then
  systemctl disable --now zdb-trial.timer; echo "trial over ($STOP)"; exit 0
fi
[[ -e $LOG ]] || printf 'time\tcopy\trepo\tzck_bytes\tpacman_bytes\tseconds\tcheck\n' > "$LOG"
[[ -e $BLOG ]] || printf 'time\tcopy\trepo\tbtrdb_bytes\trequests\tpacman_bytes\tseconds\tcheck\n' > "$BLOG"
head_of() { curl -sfI "$1" | tr -d '\r'; }
copies=(hourly)
stamp=$STATE/daily/.last-sync
if [[ ! -e $stamp ]] || (( $(date +%s) - $(stat -c %Y "$stamp") >= 86000 )); then copies+=(daily); fi
for c in "${copies[@]}"; do
  mkdir -p "$STATE/$c"
  for r in core extra; do
    db=$UP/$r/os/x86_64/$r.db
    out=$(/opt/zdb/zdb-sync.sh "$ZDB/$r/os/x86_64" "$STATE/$c" "$r" 2>&1) || {
      printf '%s\t%s\t%s\t\t\t\tFAILED %s\n' "$(date '+%F %T')" "$c" "$r" "${out//$'\n'/ }" >> "$LOG"; continue; }
    IFS=$'\t' read -r _ got _ secs state <<<"$out"
    # pacman -Sy at the same moment: the whole db when its Last-Modified moved since the last sync
    h=$(head_of "$db")
    upl=$(sed -n 's/^[Ll]ast-[Mm]odified: //p' <<<"$h")
    prev=$(cat "$STATE/$c/$r.pacman-lm" 2>/dev/null)
    plen=0
    if [[ $upl != "$prev" ]]; then plen=$(sed -n 's/^[Cc]ontent-[Ll]ength: //p' <<<"$h"); echo "$upl" > "$STATE/$c/$r.pacman-lm"; fi
    check=unchanged
    if [[ $state != unchanged ]]; then
      if [[ -n $upl && $(date -d "$upl" +%s) == $(stat -c %Y "$STATE/$c/$r.db") ]]; then
        a=$(curl -sf "$db" | bsdtar -xOf - | sha256sum)
        b=$(bsdtar -xOf "$STATE/$c/$r.db" | sha256sum)
        if [[ $a == "$b" ]]; then check=ok; else check=MISMATCH; fi
      else
        check=skew
      fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$c" "$r" "$got" "$plen" "$secs" "$check" >> "$LOG"
    # btrdb at the same moment, same pacman figure
    bd=$STATE/$c-btrdb
    if bout=$(/usr/bin/python3 /opt/btrdb/btrdb.py sync --keys /etc/btrpkg/keys --btrpkg /usr/bin/btrpkg "$BTRDB" "$bd" "$r" 2>&1); then
      IFS=$'\t' read -r _ bgot breq bsecs bstate <<<"$bout"
      bcheck=unchanged
      if [[ $bstate != unchanged ]]; then
        if [[ -n $upl && $(date -d "$upl" +%s) == $(stat -c %Y "$bd/$r.db") ]]; then
          curl -sf -o "$bd/.$r.upstream" "$db"
          if /usr/bin/python3 /opt/btrdb/btrdb.py compare "$bd/$r.db" "$bd/.$r.upstream" >/dev/null; then bcheck=ok; else bcheck=MISMATCH; fi
          rm -f "$bd/.$r.upstream"
        else
          bcheck=skew
        fi
      fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$c" "$r" "$bgot" "$breq" "$plen" "$bsecs" "$bcheck" >> "$BLOG"
    else
      printf '%s\t%s\t%s\t\t\t%s\t\tFAILED %s\n' "$(date '+%F %T')" "$c" "$r" "$plen" "${bout//$'\n'/ }" >> "$BLOG"
    fi
  done
  if [[ $c == daily ]]; then touch "$stamp"; fi
done
