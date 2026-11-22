#!/usr/bin/env python3
"""One-off: move each repository's chunks/ and raw/ into the shared btrparts directory, leave
chunks -> ../btrparts (clients from before the rename) and btrparts -> ../btrparts (new clients
with a database from before the move) symlinks, rewrite every database with
btrpkg.parts, then check that every btrpart every index names is there. (The symlinks were removed
again once every client had pacman-btrpkg -5 / btrpkg-git r55.)"""
import os, subprocess, sys
R = os.path.expanduser("~/Work/btrpkg-repo")
B = os.path.expanduser("~/Work/btrpkg/target/release/btrpkg")
KEY = f"{R}/keys/btrpkg-test.sec"
# repo dir, db name, parts (relative), mirror (db-in + keyring) or not
REPOS = [
    ("omarchy-mirror/x86_64", "omarchy", "../btrparts", True),
    ("arch-mirror/core/os/x86_64", "core", "../../../btrparts", True),
    ("arch-mirror/extra/os/x86_64", "extra", "../../../btrparts", True),
    ("omarchy/x86_64", "omarchy", "../btrparts", False),
    ("btrtest/x86_64", "btrtest", "../btrparts", False),
    ("btrtest2/x86_64", "btrtest2", "../btrparts", False),
]

def merge(src, dst):
    moved = dup = 0
    for sub in sorted(os.listdir(src)):
        sd, dd = os.path.join(src, sub), os.path.join(dst, sub)
        os.makedirs(dd, exist_ok=True)
        for f in os.listdir(sd):
            if os.path.exists(os.path.join(dd, f)):
                os.unlink(os.path.join(sd, f)); dup += 1
            else:
                os.rename(os.path.join(sd, f), os.path.join(dd, f)); moved += 1
        os.rmdir(sd)
    os.rmdir(src)
    return moved, dup

for d, name, parts, mirror in REPOS:
    repo = f"{R}/{d}"
    pdir = os.path.normpath(os.path.join(repo, parts))
    chunks, raw = f"{repo}/chunks", f"{repo}/raw"
    if os.path.isdir(chunks) and not os.path.islink(chunks):
        m, dup = merge(chunks, pdir)
        print(f"{d}: {m} btrparts moved to {os.path.relpath(pdir, R)}, {dup} already there")
        os.symlink(parts, chunks)
    # a new client with a database from before the move (no btrpkg.parts) looks in btrparts/
    if not os.path.lexists(f"{repo}/btrparts"):
        os.symlink(parts, f"{repo}/btrparts")
    if os.path.isdir(raw):
        m, dup = merge(raw, f"{pdir}/.raw")
        print(f"{d}: {m} raw map entries moved, {dup} already there")
    cmd = [B, "repo-update", "--key", KEY, "--jobs", "8", "--parts", parts]
    if mirror:
        cmd += ["--gpg-keyring", f"{repo}/.keyring.gpg", "--db-in", f"{repo}/.upstream.db"]
    r = subprocess.run(cmd + [repo, name], capture_output=True, text=True)
    print(f"{d}: {r.stderr.strip().splitlines()[-1] if r.stderr.strip() else ''} (exit {r.returncode})")

bad = 0
for d, name, parts, mirror in REPOS:
    repo = f"{R}/{d}"
    pdir = os.path.normpath(os.path.join(repo, parts))
    db = subprocess.run(["bsdtar", "-xOf", f"{repo}/{name}.db", "--include=*/desc"], capture_output=True, text=True).stdout
    n_idx = db.count("btrpkg.index="); n_parts = db.count(f"btrpkg.parts={parts}\n")
    missing = total = 0
    for f in os.listdir(f"{repo}/index"):
        if not f.endswith(".btridx"): continue
        for line in subprocess.run([B, "dump", f"{repo}/index/{f}"], capture_output=True, text=True).stdout.splitlines():
            p = line.split()
            if p[0] == "c":
                total += 1
                if not os.path.isfile(f"{pdir}/{p[5][:2]}/{p[5]}"): missing += 1
    bad += missing + (n_idx != n_parts)
    print(f"{d}: db {n_idx} btrpkg entries, {n_parts} with btrpkg.parts={parts}; index btrparts {total}, missing {missing}")
sys.exit(1 if bad else 0)
