# zdb: pacman sync databases as zchunk files (prototype)

`pacman -Sy` downloads a changed sync database whole; Arch's `extra.db` is about 9 MB and changes
many times a day. Republished as a [zchunk](https://github.com/zchunk/zchunk) file, a client
downloads only the package entries that changed since its last sync.

- `normalize.py IN OUT`: rewrites a database tar with every header owned by root/0, everything
  else unchanged. repo-add re-creates the whole tar on each update with the owner of whoever ran
  it, which otherwise changes every chunk (HISTORY.md, "zchunk for the sync databases").
- `zdb-publish.sh OUT UPSTREAM REPO...` (server): conditional GET of each `REPO.db`, then
  `OUT/REPO/os/x86_64/REPO.db.zck`: one chunk per package entry (`zck -s %FILENAME%`), a zstd
  dictionary trained once from the first version (`OUT/.dict/`, kept; a new one changes every
  chunk), mtime = the upstream db's Last-Modified. Old versions stay as `REPO.db.zck.<mtime>`.
  About 2 s per changed `extra.db` on the desktop.
- `zdb-sync.sh URL SYNCDIR REPO` (client): a HEAD first (same Last-Modified as the last sync: done,
  like pacman's 304); else `zckdl -s SYNCDIR/REPO.db.zck` (lead and header by range, then the
  missing chunks in multi-range requests; header and chunk checksums checked), `unzck` to the plain
  tar pacman reads, stamped with the Last-Modified so that a later `pacman -Sy` against the normal
  mirror gets 304.
- `caddy-bytes.py LOG LINE IP`: what caddy sent one client under `/zdb/` since a log line.

zchunk tools: built from 1.5.5 into `~/.local/opt/zchunk` (meson from `$TMPDIR/mtools`); a lab
client gets `zckdl`, `unzck` and `libzck.so.1` in `/opt/zdb` (`ZCK_BIN=/opt/zdb/bin`).

Served by the desktop's caddy: `http://192.168.2.2:1764/zdb/{core,extra}/os/x86_64/REPO.db.zck`
(`.dict/` and `.upstream/` are hidden). `mirror/mirror-all.sh` (the 10-minute mirror timer) runs
`zdb-publish.sh ~/Work/btrpkg-repo/zdb http://127.0.0.1:9129/repo/archlinux core extra` after the
btrpkg repositories (`NO_ZDB=1` skips it; its log lines start with `zdb:`).

Trial: `zdb-trial.sh` + `systemd/zdb-trial.{service,timer}` on p14s (2026-11-23 to 11-26 12:00,
then it disables itself): an hourly and a daily scratch copy of core.db/extra.db, logging per sync
the zck bytes, what `pacman -Sy` would have downloaded at that moment, and a content check against
pacoloco's db (`/var/log/zdb-trial.tsv`).

Not built: pacman support (libalpm reads databases through libarchive, which has no zchunk
reader; the client script writes a plain tar instead), signing (Arch's databases are unsigned
today, and zchunk's header has room for signatures), the `.files` databases.
