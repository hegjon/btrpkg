# btrpkg mirrors

`repo-mirror.sh` keeps one repository's mirror in step with its upstream (through pacoloco):
upstream's database plus btrpkg data for the wanted packages (`ALL=1`: every package).
`mirror-all.sh` runs it for every line of `repos.conf`, then `btrpkg gc` once per btrparts
directory. Served by caddy at `http://192.168.2.2:1764/` from `~/Work/btrpkg-repo`:

| repo | arch | path | wanted | btrparts |
|---|---|---|---|---|
| omarchy | x86_64 | `omarchy-mirror/x86_64` | `wanted.txt` (58) | `omarchy-mirror/pkg` |
| omarchy | aarch64 | `omarchy-mirror/aarch64` | `omarchy-aarch64.wanted` (96) | `omarchy-mirror/pkg` |
| core | x86_64 | `arch-mirror/core/os/x86_64` | `core.wanted` (201) | `arch-mirror/pkg` |
| extra | x86_64 | `arch-mirror/extra/os/x86_64` | `extra.wanted` (627) | `arch-mirror/pkg` |
| core | aarch64 (ALARM) | `arch-mirror/core/os/aarch64` | `core-aarch64.wanted` (204) | `arch-mirror/pkg` |
| extra | aarch64 (ALARM) | `arch-mirror/extra/os/aarch64` | `extra-aarch64.wanted` (715) | `arch-mirror/pkg` |
| alarm | aarch64 | `arch-mirror/alarm/os/aarch64` | `alarm.wanted` (1) | `arch-mirror/pkg` |
| asahi-alarm | aarch64 | `arch-mirror/asahi-alarm/os/aarch64` | `asahi-alarm.wanted` (9) | `arch-mirror/pkg` |

Client URLs: `http://192.168.2.2:1764/omarchy-mirror/$arch` and
`http://192.168.2.2:1764/arch-mirror/$repo/os/$arch`. asahi-alarm comes straight from its GitHub
releases, the others through pacoloco (`127.0.0.1:9129`). Keyrings: Arch's and Omarchy's from
`/usr/share/pacman/keyrings/`, ALARM's and asahi-alarm's in `keyrings/`.

Wanted lists: Omarchy x86_64 is what's installed on any lab machine plus omarchy-iso's root image;
core/extra x86_64 are the root image's dependency closure (omarchy-iso's archinstall and image
package lists, Omarchy's base packages, omarchy-dev, omarchy-settings-dev, omarchy-nvim: 878
packages, resolved with `pacman -Sp` against an empty database); the aarch64 lists come from
`pacman -Sl` on macmini-m1 and galaxybook (what they have installed), plus the x86_64 lists.

## Layout

```
omarchy-mirror/
├── version                                   repository format version, "0\n"
├── x86_64/
│   ├── omarchy.db -> omarchy.db.tar.zst      upstream's database with btrpkg fields
│   ├── NAME.pkg.tar.zst (+ .sig)             upstream's package file (stock pacman, fallback)
│   ├── NAME.btrpkg (+ .sig)                  the thin index, signed with the mirror's key
│   ├── NAME.pkg.tar.zst.sha256sum            "HASH  FILE": the package file the index was made from
│   ├── .upstream.db, .done, .lock, .wanted-learned
├── aarch64/
└── pkg/xx/HASH                               btrparts, shared by both architectures (NOCOMPRESS)
```

Arch's repositories use `$repo/os/$arch` and share `arch-mirror/pkg`
(`btrpkg.parts=../../../pkg`).

Arch's core and extra databases are also published as thin `.btrdb` indexes with
content-addressed desc blobs (`btrdb/`, into `~/Work/btrpkg-repo/btrdb/`) by `btrdb-publish.sh`,
on a timer of its own: a conditional GET of upstream's db every minute into the hidden
`.upstream/`, and a publish when it changed.

## Each run

1. Fetches upstream's database (conditional GET). If it hasn't changed, the last run finished, and
   nothing new is wanted, the run stops there (well under a second).
2. Downloads the `.pkg.tar.*` and `.sig` of the wanted packages the mirror doesn't have yet. A run
   counts as finished (`.done`) only if every download worked, so failed ones are retried.
3. Runs `btrpkg repo-update --keep-going --db-in .upstream.db`. Every new file is checked against
   the database's SHA-256 and upstream's OpenPGP signature (gpgv), converted, its btrparts and
   signed index published, and the served database replaced last by upstream's with four
   `btrpkg.*` lines in `%DATA%` per converted package. A package that can't be converted (e.g. no
   `.sig` upstream: Omarchy aarch64's obsidian) keeps its plain entry.
4. After all repositories: `btrpkg gc --keep 2` per btrparts directory (skipped if any repository
   failed; `NO_GC=1`, `GC_KEEP=N`). Each package keeps its current version, anything newer and one
   older; other versions lose index, signature, package file and its `.sha256sum`, then unreferenced btrparts
   go. Nothing younger than an hour is removed; repository locks are held.

The mirror signs an index only after upstream's own signature has checked out, so a client that
trusts the mirror's key gets nothing upstream didn't sign.

Packages that aren't converted keep their plain entry. pacman downloads those as usual: the
mirror doesn't have the file (404), so pacman tries the next `Server`. A client lists the mirror
first and upstream after it:

    [omarchy]
    Server = http://192.168.2.2:1764/omarchy-mirror/$arch
    Server = http://desktop.lab:9129/repo/omarchy-edge-$arch
    Server = https://pkgs.omarchy.org/edge/$arch

The mirror also learns from misses: each package whose `.pkg.tar.*` a client asked the mirror for
and got a 404 goes into `.wanted-learned` (read from caddy's access log), and the next run converts
it. A file the mirror stores but didn't convert is served plain and not learned.

## Copying a mirror elsewhere (rsync, rclone)

A client only fetches what the database names, so a copy (another host, R2/S3) stays usable at
every moment if the database arrives last and nothing it names is deleted before it is replaced.
Copy in four passes, each only if the one before succeeded:

1. `pkg/`, adding only. A btrpart's name is the hash of its content, so a file that is already
   there is right: no checksums, never overwrite. When several repositories share one `pkg/`
   (`arch-mirror`), this pass runs once, before any of their databases.
2. The architecture directories without databases: package files and `.sig`, `NAME.btrpkg` and
   `.sig`, `.sha256sum`. Their names carry the version, so they are added, never changed. The
   package files must be there before the database too: stock pacman and the fallback fetch them.
3. The databases (`REPO.db.tar.zst` and the `REPO.db` symlink) and `version`.
4. Deletions, in the architecture directories first, then in `pkg/`. Nothing the new database
   names is gone at the source (`gc` keeps the current version), and `gc --keep 2` with its hour of
   minimum age keeps the previous version for a client that fetched the old database just before
   pass 3. A copy updated much less often than the source can still drop files such a client asks
   for; it then gets a 404 and falls back to the next `Server`.

Dotfiles (`.lock`, `.done`, `.upstream.db`, `.keyring.gpg`, `.wanted-learned`) stay behind.

```sh
S=~/Work/btrpkg-repo/omarchy-mirror  D=host:/srv/omarchy-mirror    # no trailing /
db=(--exclude='*.db' --exclude='*.db.tar.*' --exclude='*.files' --exclude='*.files.tar.*')
set -e
rsync -a --mkpath --ignore-existing "$S/pkg/" "$D/pkg/"
rsync -a --ignore-existing --exclude='.*' --exclude=/pkg/ --exclude=/version "${db[@]}" "$S/" "$D/"
rsync -a --delay-updates --exclude='.*' --exclude=/pkg/ "$S/" "$D/"
rsync -a --delete --exclude='.*' --exclude=/pkg/ "$S/" "$D/"
rsync -a --delete "$S/pkg/" "$D/pkg/"
```

With rclone (`D=r2:bucket/omarchy-mirror`): `copy --ignore-existing` of `pkg/` (pass 1) and of the
tree with `--exclude '.*' --exclude '/pkg/**' --exclude /version` and the database patterns (pass
2), `copy
--copy-links` for pass 3 and `sync --copy-links` for pass 4 (both `--exclude '.*' --exclude
'/pkg/**'`), then `sync` of `pkg/` on its own. Object storage has no symlinks:
`--copy-links` uploads `REPO.db` as a copy of its target, and pass 4 needs it too, or `sync`
deletes `REPO.db` as missing at the source. Both sets of commands were checked on a small tree
with this layout (rsync 3.5.2, rclone in its docker image): each pass copied exactly its files,
pass 4 deleted only the removed ones, and no dotfile was copied.

Everything from passes 1 and 2 can be served with `Cache-Control: immutable` and a long lifetime;
only the databases and `version` change, so a CDN must keep those short, or it serves an old
database next to new btrparts (or the other way round).

Object counts for R2/S3, 2026-11-24: `arch-mirror/pkg` 167,300 files (5.9 GB),
`omarchy-mirror/pkg` 88,966 (3.2 GB).

## Services and maintenance

Systemd user units in `systemd/`, installed in `~/.config/systemd/user/`, lingering on:
- `btrpkg-repo.service`: caddy serving `~/Work/btrpkg-repo` (`:8790` local, `:1764` lab,
  `:1763` TLS). Dotfiles, `keys/`, logs and the Caddyfile are hidden.
- `btrpkg-mirror.timer`: runs `mirror-all.sh` 2 minutes after boot, then 10 minutes after each run
  ends.
- `btrpkg-btrdb.timer`: runs `btrdb-publish.sh` every minute (a minute after each run ends; a
  lock in `.upstream/` keeps a manual run from overlapping). It logs only changes.

Check: `systemctl --user list-timers 'btrpkg-*'`, `journalctl --user -u btrpkg-mirror -u btrpkg-btrdb`.
Stop the timer and wait for the service to go idle before changing a repository by hand.

