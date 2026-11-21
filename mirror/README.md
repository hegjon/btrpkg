# btrpkg mirrors: Omarchy's edge repository, Arch's core and extra

`repo-mirror.sh` keeps one repository's mirror in step with its upstream (through pacoloco):
upstream's database plus btrpkg data for the wanted packages (`ALL=1`: every package).
`mirror-all.sh` runs it for every line of `repos.conf`:

| repo | lab URL | wanted |
|---|---|---|
| omarchy | `http://192.168.2.2:1764/omarchy-mirror/$arch` | `wanted.txt`: installed on any lab machine, plus omarchy-iso's root image (58) |
| core | `http://192.168.2.2:1764/arch-mirror/$repo/os/$arch` | `core.wanted`: omarchy-iso's root image (201) |
| extra | `http://192.168.2.2:1764/arch-mirror/$repo/os/$arch` | `extra.wanted`: omarchy-iso's root image (627) |

The root image's lists are the dependency closure of omarchy-iso's x86_64 edge image
(`builder/archinstall.packages` without the per-machine firmware, `builder/image.packages`,
Omarchy's `omarchy-base.packages`, omarchy-dev, omarchy-settings-dev and omarchy-nvim), 878
packages, resolved with `pacman -Sp --print-format '%r %n'` against an empty database. Arch
packages are checked against `/usr/share/pacman/keyrings/archlinux.gpg`, Omarchy's against
`omarchy.gpg`. First run: core 201 packages in about 20 minutes, extra 627 in 18 minutes
(1.89 GB of chunks); each package converts on about one core, which is what limits it.

Each run:
1. Fetches upstream's `omarchy.db` through pacoloco (conditional GET). If it hasn't changed and the
   last run succeeded, the run stops there; that takes 0.4 s.
2. Downloads the `.pkg.tar.zst` and `.sig` of the wanted packages the mirror doesn't have yet.
3. Runs `btrpkg repo-update --db-in`. Every new file is checked against the database's SHA-256 and
   Omarchy's OpenPGP signature (`gpgv` with `/usr/share/pacman/keyrings/omarchy.gpg`, dearmored),
   then converted. Its chunks and the index (signed with the mirror's key) are published, and the
   served `omarchy.db` is replaced last by upstream's with three `btrpkg.*` lines in `%DATA%` per
   converted package.

The mirror signs an index only after Omarchy's own signature has checked out, so a client that
trusts the mirror's key gets nothing Omarchy didn't sign.

Packages that aren't converted keep their plain entry. pacman downloads those as usual: the
mirror doesn't have the file (404), so pacman tries the next `Server`. A client therefore lists
the mirror first and Omarchy's servers after it:

    [omarchy]
    Server = http://192.168.2.2:1764/omarchy-mirror/$arch
    Server = http://desktop.lab:9129/repo/omarchy-edge-$arch
    Server = https://pkgs.omarchy.org/edge/$arch

The stock pacman reads the same database and ignores the btrpkg lines. `pacman-btrpkg` installs the
converted packages as btrpkg packages.

`wanted.txt`: the Omarchy packages installed on any lab machine (51; 0.91 GB of `.pkg.tar.zst`,
895 MB of chunks). The whole repository would be 306 packages and 17.4 GB.

The mirror also learns from misses. Every run reads caddy's access log, and each package whose
`.pkg.tar.zst` a client requested from the mirror and got a 404 for goes into
`.wanted-learned`; the run converts it. After p14s's update installed fantasi, flowtunes and
omarchy-themes-loops (fetched from Omarchy's servers through the fallback), the next run
converted them in 4 s. A machine's next update of those packages then comes through btrpkg.

Old versions stay, so a client with a slightly older database can still fetch them. Nothing is
removed yet; deleting unused files and chunks belongs to the repo pipeline (PLAN step 7).

Services (systemd user units in `systemd/`, installed in `~/.config/systemd/user/`, with lingering
on so they run without a login):
- `btrpkg-repo.service`: caddy serving `~/Work/btrpkg-repo` (`:8790` local, `:1764` lab,
  `:1763` TLS). Dotfiles, `keys/`, logs and the Caddyfile are hidden.
- `btrpkg-mirror.timer`: runs the mirror 2 minutes after boot, then 10 minutes after each run
  ends.

Check: `systemctl --user list-timers btrpkg-mirror.timer`, `journalctl --user -u btrpkg-mirror`.
