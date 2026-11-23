#!/bin/bash
# zchunk for pacman sync databases: Arch's daily snapshots from archive.archlinux.org
# (DATE.REPO.{db,files}, gzip'd tar) as .tar.zck, delta size from an older snapshot.
# Inputs in the current directory, e.g. for d in 2026/11/22 2026/11/21 2026/11/15 2026/10/23:
#   curl -fL -o ${d//\//-}.extra.db https://archive.archlinux.org/repos/$d/extra/os/x86_64/extra.db
# (likewise extra.files, core.db), then
#   docker run --rm --network host -v /etc/pacman.d/mirrorlist:/etc/pacman.d/mirrorlist:ro \
#     -v $PWD:/w -e NORM=1 archlinux:latest bash /w/zchunk-db.sh
# NORM=1: every tar header's owner set to root/0 first. repo-add re-creates the whole tar on each
# update with the owner of whoever ran it, so without that every chunk differs between versions.
set -euo pipefail
pacman -Syu --noconfirm zchunk zstd python >/dev/null 2>&1
cd /w; mkdir -p out
trap 'chown -R 1000:1000 out' EXIT   # files in the mounted dir are root's otherwise
NEW=2026-11-22
tarof() {
  [[ -e out/$1.tar ]] && return
  gzip -dc "$1" > "out/$1.tar"
  [[ ${NORM:-0} == 1 ]] || return 0
  python3 - "out/$1.tar" <<'EOF'
import io, os, sys, tarfile
p = sys.argv[1]
with tarfile.open(p) as t, tarfile.open(p + ".n", "w", format=t.format) as o:
    for m in t.getmembers():
        data = t.extractfile(m).read() if m.isfile() else None
        m.uid = m.gid = 0
        m.uname = m.gname = "root"
        o.addfile(m, io.BytesIO(data) if data is not None else None)
os.replace(p + ".n", p)
EOF
}
# variant: default (content-defined), split (a chunk per package entry), dict (split + a zstd
# dictionary trained on the oldest snapshot, the same dict for every version)
mkzck() { # $1 file stem, $2 variant
  local o=out/$1.$2.zck s
  [[ -e $o ]] && return
  case $1 in *.files) s='%FILES%' ;; *) s='%FILENAME%' ;; esac
  case $2 in
    default) zck -o "$o" "out/$1.tar" >/dev/null ;;
    split) zck -s "$s" -o "$o" "out/$1.tar" >/dev/null ;;
    dict) zck -s "$s" -D "out/${1#*.}.dict" -o "$o" "out/$1.tar" >/dev/null ;;
  esac
}
printf 'repo\tfrom\tvariant\tgz_today\tzck_full\theader\tdownload\n'
for r in core.db extra.db extra.files; do
  for d in 2026-10-23 2026-11-15 2026-11-21 $NEW; do tarof $d.$r; done
  # dictionary from the oldest snapshot (zck_gen_zdict writes DIR/*.dict per chunk; use zstd --train
  # on the split entries instead: one dict of 112 KiB)
  if [[ ! -e out/$r.dict ]]; then
    rm -rf out/train; mkdir -p out/train; (cd out/train && bsdtar -xf ../2026-10-23.$r.tar)
    find out/train -type f > out/train.list
    if ! zstd -q --train --maxdict=114688 --filelist=out/train.list -o out/$r.dict >/dev/null 2>&1; then
      mapfile -t tl < <(head -20000 out/train.list)
      zstd -q --train --maxdict=114688 "${tl[@]}" -o out/$r.dict
    fi
    rm -rf out/train out/train.list
  fi
  for v in default split dict; do
    for d in 2026-10-23 2026-11-15 2026-11-21 $NEW; do mkzck $d.$r $v; done
    for d in 2026-11-21 2026-11-15 2026-10-23; do
      line=$(zck_delta_size out/$d.$r.$v.zck out/$NEW.$r.$v.zck 2>&1 | tr '\n' ' ')
      dl=$(sed -n 's/.*Would download in total \([0-9]*\) of.*/\1/p' <<<"$line")
      hd=$(sed -n 's/.*%), \([0-9]*\) in the header.*/\1/p' <<<"$line")
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$r" "$d" "$v" "$(stat -c %s $NEW.$r)" "$(stat -c %s out/$NEW.$r.$v.zck)" "$hd" "$dl"
    done
  done
  # references: the whole new db at zstd -19, and an ideal binary delta (zstd --patch-from)
  for d in 2026-11-21 2026-11-15 2026-10-23; do
    p=$(zstd -q -19 --long=31 --patch-from=out/$d.$r.tar -c out/$NEW.$r.tar | wc -c)
    printf '%s\t%s\t%s\t%s\t%s\t\t%s\n' "$r" "$d" patch-from "$(stat -c %s $NEW.$r)" "$(zstd -q -19 -c out/$NEW.$r.tar | wc -c)" "$p"
  done
done
