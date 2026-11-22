#!/usr/bin/python3
"""Move a btrpkg repository's indexes next to the package files.

    migrate-sibling-index.py REPODIR...

index/NAME.btridx (+ .sig) and index/NAME.src become NAME.pkg.btr (+ .sig) and NAME.pkg.btr.src
(hard links first), the database's btrpkg.index=index/NAME.btridx lines become
btrpkg.index=NAME.pkg.btr (the database is replaced atomically), then index/ goes. The index bytes
and signatures don't change. Holds the repository's .lock (the mirror's and gc's) throughout.
Clients before pacman-btrpkg 8 / btrpkg r70 don't take the new field and download the package
files instead.
"""
import fcntl, gzip, io, os, re, subprocess, sys, tarfile

def die(m):
    sys.exit(f"migrate-sibling-index: {m}")

def migrate(repo):
    idir = os.path.join(repo, "index")
    if not os.path.isdir(idir):
        print(f"{repo}: no index/, nothing to do"); return
    dbs = [f for f in os.listdir(repo) if f.endswith(".db") and not f.startswith(".")]
    if len(dbs) != 1: die(f"{repo}: {len(dbs)} databases")
    db = os.path.realpath(os.path.join(repo, dbs[0]))
    lock = open(os.path.join(repo, ".lock"), "a")
    try: fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError: die(f"{repo}: busy (.lock)")

    # 1. the new names, as hard links (nothing is gone until the database says so)
    moves = []
    for f in sorted(os.listdir(idir)):
        for old, new in ((".btridx.sig", ".pkg.btr.sig"), (".btridx", ".pkg.btr"), (".src", ".pkg.btr.src")):
            if f.endswith(old):
                moves.append((os.path.join(idir, f), os.path.join(repo, f[: -len(old)] + new))); break
        else:
            die(f"{repo}: unexpected index/{f}")
    for a, b in moves:
        if os.path.lexists(b):
            if os.path.samefile(a, b): continue
            if open(a, "rb").read() != open(b, "rb").read(): die(f"{b} exists and differs from {a}")
            continue
        os.link(a, b)

    # 2. the database
    raw = open(db, "rb").read()
    zst = raw.startswith(b"\x28\xb5\x2f\xfd")
    if zst: tarb = subprocess.run(["zstd", "-dc"], input=raw, capture_output=True, check=True).stdout
    elif raw.startswith(b"\x1f\x8b"): tarb = gzip.decompress(raw)
    else: die(f"{db}: neither zstd nor gzip")
    pat = re.compile(r"^btrpkg\.index=index/([^/\n]+)\.btridx$", re.M)
    out = io.BytesIO(); n = 0; have = 0
    with tarfile.open(fileobj=io.BytesIO(tarb)) as tin, tarfile.open(fileobj=out, mode="w", format=tarfile.PAX_FORMAT) as tout:
        for m in tin.getmembers():
            if not m.isfile():
                tout.addfile(m); continue
            data = tin.extractfile(m).read()  # type: ignore[union-attr]
            if m.name.endswith("/desc"):
                text = data.decode()
                have += text.count("btrpkg.index=")
                text, k = pat.subn(lambda x: f"btrpkg.index={x.group(1)}.pkg.btr", text)
                for name in pat.findall(data.decode()):
                    if not os.path.isfile(os.path.join(repo, name + ".pkg.btr")): die(f"{repo}: {name}.pkg.btr missing")
                n += k
                data = text.encode()
            m.size = len(data)
            tout.addfile(m, io.BytesIO(data))
    if n != have: die(f"{db}: {have} btrpkg.index lines, {n} in the old form")
    comp = (subprocess.run(["zstd", "-19", "-q", "-c"], input=out.getvalue(), capture_output=True, check=True).stdout
            if zst else gzip.compress(out.getvalue(), 9, mtime=0))
    tmp = db + ".migrate"
    with open(tmp, "wb") as f:
        f.write(comp); f.flush(); os.fsync(f.fileno())
    os.rename(tmp, db)

    # 3. the old names
    for a, _ in moves: os.unlink(a)
    os.rmdir(idir)
    print(f"{repo}: {len(moves)} files moved, {n} database entries rewritten")

for r in sys.argv[1:]:
    migrate(r)
