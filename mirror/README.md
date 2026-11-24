# btrpkg mirrors

`repo-mirror.sh` keeps one repository's mirror in step with its upstream (through pacoloco):
upstream's database plus btrpkg data for the wanted packages (`ALL=1`: every package).
`mirror-all.sh` runs it for every line of `repos.conf`, then `btrpkg gc` once per btrparts
directory. Served by caddy at `http://192.168.2.2:1764/` from `~/Work/btrpkg-repo`:

| repo | arch | path | wanted | btrparts |
|---|---|---|---|---|
| omarchy | x86_64 | `omarchy-mirror/x86_64` | `wanted.txt` (58) | `omarchy-mirror/btrparts` |
| omarchy | aarch64 | `omarchy-mirror/aarch64` | `omarchy-aarch64.wanted` (96) | `omarchy-mirror/btrparts` |
| core | x86_64 | `arch-mirror/core/os/x86_64` | `core.wanted` (201) | `arch-mirror/btrparts` |
| extra | x86_64 | `arch-mirror/extra/os/x86_64` | `extra.wanted` (627) | `arch-mirror/btrparts` |
| core | aarch64 (ALARM) | `arch-mirror/core/os/aarch64` | `core-aarch64.wanted` (204) | `arch-mirror/btrparts` |
| extra | aarch64 (ALARM) | `arch-mirror/extra/os/aarch64` | `extra-aarch64.wanted` (715) | `arch-mirror/btrparts` |
| alarm | aarch64 | `arch-mirror/alarm/os/aarch64` | `alarm.wanted` (1) | `arch-mirror/btrparts` |
| asahi-alarm | aarch64 | `arch-mirror/asahi-alarm/os/aarch64` | `asahi-alarm.wanted` (9) | `arch-mirror/btrparts` |

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

After the repositories, `mirror-all.sh` publishes Arch's core and extra databases as thin
`.btrdb` indexes with content-addressed desc blobs for the sync-database trial on p14s (`btrdb/`,
into `~/Work/btrpkg-repo/btrdb/`, upstream's dbs fetched into its hidden `.upstream/`;
`NO_BTRDB=1` skips it).

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

## Services and maintenance

Systemd user units in `systemd/`, installed in `~/.config/systemd/user/`, lingering on:
- `btrpkg-repo.service`: caddy serving `~/Work/btrpkg-repo` (`:8790` local, `:1764` lab,
  `:1763` TLS). Dotfiles, `keys/`, logs and the Caddyfile are hidden.
- `btrpkg-mirror.timer`: runs `mirror-all.sh` 2 minutes after boot, then 10 minutes after each run
  ends.

Check: `systemctl --user list-timers btrpkg-mirror.timer`, `journalctl --user -u btrpkg-mirror`.
Stop the timer and wait for the service to go idle before changing a repository by hand.

