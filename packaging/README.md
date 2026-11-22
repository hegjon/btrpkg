# Packages

Two Arch packages, built in a clean container by `packaging/build.sh [OUTDIR]` (default
`packaging/out/`; no root on the host, takes about 5 minutes):

- **`btrpkg-git`** (`btrpkg/`): `/usr/bin/btrpkg`, `/usr/bin/pacbtr` (the same binary, a
  hardlink), `btrpkg-cache-clean.{service,timer}` (weekly, keeps the newest 3 `.pkg.btr` of each
  package in every pacman CacheDir; the timer is not enabled by default, like `paccache.timer`),
  and empty key directories `/usr/share/btrpkg/keys` (packaged keys) and `/etc/btrpkg/keys`
  (local ones). It builds GitHub's HEAD; `BTRPKG_SRC=git+file:///src packaging/build.sh` builds
  this checkout instead. It links the system's libzstd: zstd-sys would otherwise compile its own
  copy with makepkg's `-flto` CFLAGS into GCC LTO objects that Rust's linker (lld) can't read.
- **`pacman-btrpkg`** (`pacman-btrpkg/`): Arch's pacman package (the same commit `54d94116`, the
  same four Arch patches, the same meson options) plus `libalpm/00*.patch` from this repository,
  installed **beside** the system pacman:
  - `/usr/bin/pacman-btrpkg` → `/usr/lib/pacman-btrpkg/pacman`, which finds its own
    `libalpm.so.16` in `/usr/lib/pacman-btrpkg/` through RUNPATH (the soname is the same as the
    system's, so nothing else may load it).
  - `/usr/bin/pacman-btrpkg-conf`: pacman-conf of this build.
  - `/etc/pacman-btrpkg.conf`, its default config:
    ```
    [options]
    BtrPkgHelper = /usr/bin/btrpkg
    Include = /etc/pacman.conf
    ```
    The system's pacman.conf stays the one place for repositories and options, and the system
    pacman never sees the `BtrPkgHelper` line it doesn't know.
  - It shares the local database, the lock, the cache, the hooks and the keyring with the system
    pacman, so the two can be used in turn. `pacman` itself is untouched; removing
    `pacman-btrpkg` leaves a normal system.
  - The patch symlinks in `pacman-btrpkg/` point at `../../libalpm/`; after changing a patch, run
    `makepkg -g` there and update `sha256sums`.

Install (on a lab machine):

    pacman -U btrpkg-git-*.pkg.tar.zst pacman-btrpkg-*.pkg.tar.zst
    pacman-btrpkg -Syu                    # like pacman -Syu; btrpkg packages where the repo has them
    systemctl enable --now btrpkg-cache-clean.timer     # optional

## pacstrap-btrpkg

`/usr/bin/pacstrap-btrpkg` (in `pacman-btrpkg`, needs `arch-install-scripts`) is Arch's pacstrap,
run unchanged, with pacman-btrpkg as its pacman. It takes the same options as pacstrap(8),
including the ones omarchy-iso uses:

    pacstrap-btrpkg -C pacman.conf -c -G -M /mnt/root PKGS...                # root image, live system
    pacstrap-btrpkg -C /etc/pacman.conf -K /mnt PKGS --noconfirm --needed   # archinstall

- A PATH entry ahead of `/usr/bin` makes pacstrap's `pacman` and `pacman-conf` the btrpkg ones.
- The config (`-C`, default `/etc/pacman.conf`) is copied with `BtrPkgHelper = /usr/bin/btrpkg`
  added unless it already has one.
- With `-P`, the target still gets the config as given, so its stock pacman never sees the
  BtrPkgHelper line.
- Packages already in a cache directory are installed from there and not downloaded again as
  btrpkg packages (`-c` with an offline mirror). That needs libalpm patch 4.
- pacstrap disables pacman's download sandbox, so btrpkg's network half runs as root there, like
  pacman's own downloads.
- With the cache on the target's filesystem, large files are reflinked from the cache files, so
  the new root's extents are the packages' compressed frames. pacstrap's default cache inside the
  new root does that. With `-c` and a cache on another filesystem, files are copied instead (the
  target recompresses them at its own level).

## Omarchy

On Omarchy, system upgrades go through `omarchy update`. Its pre-transaction hook refuses a direct
`pacman -Syu`, and the update flow (`omarchy-update-pacman`) runs `sudo ... pacman "$@"` with the
name hardcoded. To put pacman-btrpkg into that flow, a symlink earlier in sudo's `secure_path`
does it:

    ln -s /usr/bin/pacman-btrpkg /usr/local/bin/pacman    # undo: rm /usr/local/bin/pacman

Every `sudo pacman` then runs pacman-btrpkg: omarchy update, its migrations, and AUR helpers. A
cleaner way would be a variable for the pacman command in `omarchy-update-pacman`, which is
Omarchy's call to make.

Repositories without btrpkg data (`btrpkg.index`/`btrpkg.id` in `%DATA%`) are handled exactly as by
pacman. The repository's signify public key goes into `/etc/btrpkg/keys/NAME.pub`.

The container uses the host's mirrorlist (and `--network host`, so a local pacoloco works), which
keeps the libraries the packages are built against at the versions of the host and the lab.
