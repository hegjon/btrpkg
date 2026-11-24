#!/usr/bin/env python3
"""btrdb: thin pacman sync databases (prototype, format version 0).

A repository database is published as one small index, REPO.btrdb, that names each package's
`desc` (and `files`) entry by the SHA-256 of a separately stored blob, db/xx/HASH. A client
downloads the index whole on every sync and only the blobs it doesn't have yet, then rebuilds
the REPO.db tar pacman reads.

Layout (beside the btrpkg layout: architecture directories, shared content-addressed stores):
  ROOT/version                  "0\\n"
  ROOT/ARCH/REPO.btrdb          the index, zstd-compressed whole (no dictionary)
  ROOT/ARCH/REPO.btrdb.sig      signify signature over the decompressed index (btrpkg sign)
  ROOT/ARCH/REPO.btrdb.patch/H  the current index as a zstd patch (--patch-from) against an
                                older one, H = SHA-256 of that older decompressed index; for
                                every index of the last 7 days (at least 3), zstd level 12
  ROOT/db/xx/HASH               a desc or files blob: zstd with the version's dictionary;
                                HASH = SHA-256 of the stored (compressed) bytes. NOCOMPRESS.

REPO.btrdb, decompressed, little endian:
  header  magic "BTRDB\\0\\0\\0" (8) | version u16 = 0 | flags u16 = 0 | package_count u32 |
          timestamp u64 (unix seconds: the source database's Last-Modified)
  entry   name_len u8 | name | version_len u8 | version | arch_len u8 | arch |
          desc_hash (32) | files_hash (32, zeros when there is no files entry)
Entries are sorted by name, names unique. The tar member of an entry is NAME-VERSION/desc.

Dictionaries are versioned with the format: dicts/desc-dict-v0.zst and files-dict-v0.zst (raw
zstd dictionaries; the client ships them). A new dictionary needs a new format version, since
it changes every blob and so every hash.

usage:
  btrdb.py train DICTDIR DB [DB...] [--files FILES...]
  btrdb.py publish [--key SEC] [--btrpkg BIN] [--files REPO.files] [--keep N] [--keep-hours H] ROOT ARCH REPO REPO.db
  btrdb.py sync [--keys DIR] [--btrpkg BIN] [--files] [--measure-full] URL SYNCDIR REPO
      URL is the architecture directory (ROOT/ARCH); blobs come from URL/../db. A client with
      an older index asks for REPO.btrdb.patch/HASH first and falls back to the whole index;
      the signature is checked on the result either way. Rebuilds
      SYNCDIR/REPO.db (and REPO.files, desc + files, with --files) as plain tar stamped with the index's
      timestamp; blobs are kept in SYNCDIR/btrdb.sqlite. Prints: REPO BYTES REQUESTS SECONDS
      STATE (patch|full|unchanged) FULL_BYTES; BYTES = response bodies received, FULL_BYTES =
      what the sync takes with the whole index instead of a patch (--measure-full: a HEAD
      request for its length, not counted in BYTES/REQUESTS; else blank).
  btrdb.py compare A.db B.db          exit 0 if both have the same members and contents
  btrdb.py dump REPO.btrdb
"""
import hashlib, http.client, io, os, shutil, sqlite3, struct, subprocess, sys, tarfile, threading, time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from compression import zstd

VERSION = 0
MAGIC = b"BTRDB\0\0\0"
HDR = struct.Struct("<8sHHIQ")
HERE = os.path.dirname(os.path.abspath(__file__))
DICTS = os.environ.get("BTRDB_DICTS", os.path.join(HERE, "dicts"))
MAX_INDEX = 64 << 20      # decompressed .btrdb (extra: 1.4 MB)
MAX_BLOB = 64 << 20       # a decompressed desc/files entry
MAX_COUNT = 1 << 20
ZERO = bytes(32)


def sha(b): return hashlib.sha256(b).digest()


def load_dict(kind):
    with open(os.path.join(DICTS, f"{kind}-dict-v{VERSION}.zst"), "rb") as f:
        return zstd.ZstdDict(f.read())


def db_members(path):
    """{member name: bytes} of the regular files in a pacman database (any compression)."""
    with tarfile.open(path) as t:
        return {m.name: t.extractfile(m).read() for m in t.getmembers() if m.isfile()}


def desc_field(desc, key):
    lines = desc.decode().split("\n")
    i = lines.index(f"%{key}%")
    return lines[i + 1]


# ------------------------------------------------------------------ index encode/decode

def encode(entries, timestamp):
    out = [HDR.pack(MAGIC, VERSION, 0, len(entries), timestamp)]
    for name, ver, arch, dh, fh in entries:
        for s in (name, ver, arch):
            b = s.encode()
            assert 0 < len(b) < 256, s
            out.append(bytes([len(b)]) + b)
        out.append(dh + fh)
    return b"".join(out)


def safe(s):
    return s and not s.startswith(".") and "/" not in s and "\0" not in s and "\n" not in s


def decode(raw):
    if len(raw) < HDR.size:
        raise ValueError("truncated header")
    magic, ver, flags, n, ts = HDR.unpack_from(raw)
    if magic != MAGIC:
        raise ValueError("not a btrdb index")
    if ver != VERSION:
        raise ValueError(f"btrdb version {ver}, this reader reads {VERSION}")
    if flags != 0:
        raise ValueError(f"unknown flags {flags:#x}")
    if n > MAX_COUNT:
        raise ValueError(f"{n} packages")
    o, out, prev = HDR.size, [], None
    for _ in range(n):
        f = []
        for _ in range(3):
            if o >= len(raw):
                raise ValueError("truncated entry")
            ln = raw[o]
            s = raw[o + 1:o + 1 + ln]
            if len(s) != ln:
                raise ValueError("truncated entry")
            f.append(s.decode())
            o += 1 + ln
        hs = raw[o:o + 64]
        if len(hs) != 64:
            raise ValueError("truncated entry")
        o += 64
        name, ver_, arch = f
        if not (safe(name) and safe(ver_) and safe(arch)):
            raise ValueError(f"unsafe entry {f!r}")
        if prev is not None and name <= prev:
            raise ValueError("entries not sorted by name")
        prev = name
        if hs[:32] == ZERO:
            raise ValueError(f"{name}: no desc")
        out.append((name, ver_, arch, hs[:32], hs[32:]))
    if o != len(raw):
        raise ValueError("trailing data")
    return ts, out


def index_raw(stored):
    """The decompressed index of a stored (zstd) or already decompressed .btrdb."""
    if stored.startswith(MAGIC):
        if len(stored) > MAX_INDEX:
            raise ValueError("index too large")
        return stored
    d = zstd.ZstdDecompressor()
    raw = d.decompress(stored, max_length=MAX_INDEX + 1)
    if len(raw) > MAX_INDEX or not d.eof or d.unused_data:
        raise ValueError("index: bad zstd stream (too large, truncated or trailing data)")
    return raw


PATCH_LEVEL = 12   # 1-2% bigger patches than 19 at a ninth of the time (~9 ms each for extra)


def make_patch(old_raw, new_raw, level=PATCH_LEVEL):
    """new_raw as one zstd frame with old_raw as its prefix (zstd --patch-from)."""
    wl = max(21, (len(old_raw) + len(new_raw)).bit_length())
    z = zstd.ZstdCompressor(options={zstd.CompressionParameter.compression_level: level,
                                     zstd.CompressionParameter.window_log: wl},
                            zstd_dict=zstd.ZstdDict(old_raw, is_raw=True).as_prefix)
    return z.compress(new_raw, mode=zstd.ZstdCompressor.FLUSH_FRAME)


def apply_patch(old_raw, patch):
    d = zstd.ZstdDecompressor(zstd_dict=zstd.ZstdDict(old_raw, is_raw=True).as_prefix)
    raw = d.decompress(patch, max_length=MAX_INDEX + 1)
    if len(raw) > MAX_INDEX or not d.eof or d.unused_data:
        raise ValueError("patch: bad zstd stream")
    return raw


def read_index(stored):
    return decode(index_raw(stored))


def unblob(stored, zd):
    d = zstd.ZstdDecompressor(zstd_dict=zd)
    raw = d.decompress(stored, max_length=MAX_BLOB + 1)
    if len(raw) > MAX_BLOB or not d.eof or d.unused_data:
        raise ValueError("blob: bad zstd stream")
    return raw


# ------------------------------------------------------------------ server side

def write_atomic(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.tmp{os.getpid()}"
    with open(tmp, "wb") as f:
        f.write(data)
    os.rename(tmp, path)


def train(dictdir, dbs, files):
    os.makedirs(dictdir, exist_ok=True)
    for kind, srcs, want in (("desc", dbs, "/desc"), ("files", files, "/files")):
        if not srcs:
            continue
        samples = [v for p in srcs for k, v in db_members(p).items() if k.endswith(want)]
        zd = zstd.train_dict(samples, 112640)
        write_atomic(os.path.join(dictdir, f"{kind}-dict-v{VERSION}.zst"), zd.dict_content)
        print(f"{kind}: {len(samples)} samples -> {len(zd.dict_content)} B dictionary")


KEEP_HOURS = 168   # patches from every index of the last 7 days


def publish(root, arch, repo, dbpath, filespath=None, key=None, btrpkg="btrpkg", keep=3, keep_hours=KEEP_HOURS):
    st = os.stat(dbpath)
    ts = int(st.st_mtime)
    idxp = os.path.join(root, arch, f"{repo}.btrdb")
    if os.path.exists(idxp):
        with open(idxp, "rb") as f:
            if read_index(f.read())[0] == ts:
                print(f"{repo}: unchanged")
                return
    dbdir = os.path.join(root, "db")
    if not os.path.isdir(dbdir):
        os.makedirs(dbdir)
        subprocess.run(["chattr", "+m", dbdir], check=False)
    if not os.path.exists(os.path.join(root, "version")):
        write_atomic(os.path.join(root, "version"), f"{VERSION}\n".encode())
    descs = db_members(dbpath)
    files = db_members(filespath) if filespath else {}
    dz = zstd.ZstdCompressor(level=19, zstd_dict=load_dict("desc"))
    fz = zstd.ZstdCompressor(level=19, zstd_dict=load_dict("files")) if files else None
    new = new_bytes = 0

    def put(data, z):
        nonlocal new, new_bytes
        b = z.compress(data, mode=zstd.ZstdCompressor.FLUSH_FRAME)
        h = sha(b)
        p = os.path.join(dbdir, h.hex()[:2], h.hex())
        if not os.path.exists(p):
            write_atomic(p, b)
            new += 1
            new_bytes += len(b)
        return h

    entries = []
    for k in sorted(descs):
        if not k.endswith("/desc"):
            continue
        d = descs[k]
        name, ver, arch_ = desc_field(d, "NAME"), desc_field(d, "VERSION"), desc_field(d, "ARCH")
        assert k == f"{name}-{ver}/desc", k
        fh = put(files[f"{name}-{ver}/files"], fz) if f"{name}-{ver}/files" in files else ZERO
        entries.append((name, ver, arch_, put(d, dz), fh))
    entries.sort(key=lambda e: e[0])
    raw = encode(entries, ts)
    stored = zstd.compress(raw, level=19)
    read_index(stored)  # what clients will check
    # blobs first; then patches, signature and index; the previous index stays as REPO.btrdb.<ts>
    if os.path.exists(idxp):
        with open(idxp, "rb") as f:
            old = f.read()
        os.replace(idxp, f"{idxp}.{read_index(old)[0]}")
    ad = os.path.join(root, arch)
    os.makedirs(ad, exist_ok=True)
    prune(ad, repo, keep, keep_hours, ts)
    pdir, pnew = f"{idxp}.patch", os.path.join(ad, f".{repo}.btrdb.patch.new")
    shutil.rmtree(pnew, ignore_errors=True)
    os.makedirs(pnew)
    psizes = []
    for f in os.listdir(ad):
        if f.startswith(f"{repo}.btrdb.") and f.rsplit(".", 1)[1].isdigit():
            with open(os.path.join(ad, f), "rb") as fh:
                o = index_raw(fh.read())
            pb = make_patch(o, raw)
            with open(os.path.join(pnew, sha(o).hex()), "wb") as fh:
                fh.write(pb)
            psizes.append(len(pb))
    tmp = os.path.join(ad, f".{repo}.btrdb.new.btrdb")
    write_atomic(tmp, stored)
    if key:
        subprocess.run([btrpkg, "sign", "--key", key, tmp], check=True, stdout=subprocess.DEVNULL)
        os.utime(tmp + ".sig", (ts, ts))
        os.replace(tmp + ".sig", idxp + ".sig")
    os.utime(tmp, (ts, ts))  # Last-Modified = the source database's, as zdb does
    os.replace(tmp, idxp)
    pold = os.path.join(ad, f".{repo}.btrdb.patch.old")
    shutil.rmtree(pold, ignore_errors=True)
    if os.path.exists(pdir):
        os.rename(pdir, pold)
    os.rename(pnew, pdir)
    shutil.rmtree(pold, ignore_errors=True)
    gc_blobs(root)
    print(f"{repo}: {len(entries)} packages, index {len(stored)} B, {new} new blobs ({new_bytes} B), "
          f"{len(psizes)} patches ({min(psizes, default=0)}-{max(psizes, default=0)} B)")


def prune(d, repo, keep, keep_hours, now_ts):
    """Keep the old indexes (REPO.btrdb.<ts>) of the last keep_hours, at least the newest keep."""
    olds = sorted((f for f in os.listdir(d) if f.startswith(f"{repo}.btrdb.") and f.rsplit(".", 1)[1].isdigit()),
                  key=lambda f: int(f.rsplit(".", 1)[1]))
    for f in olds[:-keep] if keep else olds:
        if int(f.rsplit(".", 1)[1]) < now_ts - keep_hours * 3600:
            os.remove(os.path.join(d, f))


def gc_blobs(root):
    """Remove blobs no index (current or kept) of any repository under root names; only those
    older than an hour (a publish writes its blobs before its index)."""
    live = set()
    for a in os.listdir(root):
        ad = os.path.join(root, a)
        if a in ("db",) or not os.path.isdir(ad):
            continue
        for f in os.listdir(ad):
            if ".btrdb" in f and not f.endswith(".sig") and not f.startswith(".") and os.path.isfile(os.path.join(ad, f)):
                with open(os.path.join(ad, f), "rb") as fh:
                    for e in read_index(fh.read())[1]:
                        live.add(e[3]); live.add(e[4])
    now = time.time()
    dbdir = os.path.join(root, "db")
    for sub in os.listdir(dbdir):
        for f in os.listdir(os.path.join(dbdir, sub)):
            p = os.path.join(dbdir, sub, f)
            if bytes.fromhex(f) not in live and now - os.stat(p).st_mtime > 3600:
                os.remove(p)


# ------------------------------------------------------------------ client side

class Fetcher:
    """Persistent HTTP/1.1 connections, one per worker thread; counts body bytes and requests."""

    def __init__(self, base):
        u = urllib.parse.urlsplit(base)
        self.scheme, self.host, self.path = u.scheme, u.netloc, u.path.rstrip("/")
        self.local = threading.local()
        self.bytes = self.requests = 0
        self.lock = threading.Lock()

    def conn(self):
        c = getattr(self.local, "c", None)
        if c is None:
            cls = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
            c = self.local.c = cls(self.host, timeout=30)
        return c

    def get(self, rel, headers=None, limit=MAX_INDEX):
        path = urllib.parse.urljoin(self.path + "/", rel)
        for attempt in range(3):
            c = self.conn()
            try:
                c.request("GET", path, headers=headers or {})
                r = c.getresponse()
                body = r.read(limit + 1)
                with self.lock:
                    self.bytes += len(body); self.requests += 1
                if len(body) > limit:
                    raise ValueError(f"{path}: too large")
                return r.status, r.getheader("Last-Modified"), body
            except (http.client.HTTPException, OSError):
                c.close(); self.local.c = None
                if attempt == 2:
                    raise


def head_length(f, rel):
    """Content-Length of rel by HEAD (not counted in f's bytes and requests)."""
    c = (http.client.HTTPSConnection if f.scheme == "https" else http.client.HTTPConnection)(f.host, timeout=30)
    try:
        c.request("HEAD", urllib.parse.urljoin(f.path + "/", rel))
        r = c.getresponse(); r.read()
        return int(r.getheader("Content-Length") or 0) if r.status == 200 else None
    finally:
        c.close()


def write_db(path, members, ts):
    """A pacman database tar: NAME-VERSION/ directories and their entries, owned by root."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as t:
        dirs = set()
        for name in sorted(members):
            d = name.split("/")[0]
            if d not in dirs:
                ti = tarfile.TarInfo(d); ti.type = tarfile.DIRTYPE; ti.mode = 0o755; ti.mtime = ts
                t.addfile(ti); dirs.add(d)
            ti = tarfile.TarInfo(name); ti.size = len(members[name]); ti.mode = 0o644; ti.mtime = ts
            t.addfile(ti, io.BytesIO(members[name]))
    write_atomic(path, buf.getvalue())
    os.utime(path, (ts, ts))


def sync(url, syncdir, repo, keys=None, btrpkg="btrpkg", with_files=False, db_rel="../db", measure_full=False):
    t0 = time.monotonic()
    os.makedirs(syncdir, exist_ok=True)
    f = Fetcher(url)
    sq = sqlite3.connect(os.path.join(syncdir, "btrdb.sqlite"))
    sq.execute("create table if not exists blobs (hash blob primary key, data blob not null)")
    sq.execute("create table if not exists repos (repo text primary key, lastmod text, ts integer)")
    row = sq.execute("select lastmod from repos where repo = ?", (repo,)).fetchone()
    cur = os.path.join(syncdir, f".{repo}.btrdb")   # the last verified index, decompressed
    have = None
    if os.path.exists(cur) and os.path.exists(os.path.join(syncdir, f"{repo}.db")):
        with open(cur, "rb") as fh:
            have = index_raw(fh.read())
    # the signature is the change marker (its Last-Modified is the source database's)
    hdrs = {"If-Modified-Since": row[0]} if row and row[0] and have else {}
    status, lastmod, sig = f.get(f"{repo}.btrdb.sig", hdrs, limit=4096)
    if status == 304:
        print(f"{repo}\t{f.bytes}\t{f.requests}\t{time.monotonic() - t0:.2f}\tunchanged\t")
        return
    if status != 200:
        raise SystemExit(f"{repo}.btrdb.sig: HTTP {status}")
    tmpi = os.path.join(syncdir, f".{repo}.btrdb.new.btrdb")
    write_atomic(tmpi + ".sig", sig)
    verify = lambda: subprocess.run([btrpkg, "verify"] + (["--keys", keys] if keys else []) + [tmpi],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
    raw, how = None, "full"
    if have:
        s2, _, pb = f.get(f"{repo}.btrdb.patch/{sha(have).hex()}")
        if s2 == 200:
            try:
                raw = apply_patch(have, pb)
                write_atomic(tmpi, raw)
                how = "patch" if verify() else None
            except (ValueError, zstd.ZstdError):
                how = None
            if how is None:  # the index moved on between the signature and the patch: whole index
                raw, how = None, "full"
    if raw is None:
        s3, _, stored = f.get(f"{repo}.btrdb")
        if s3 != 200:
            raise SystemExit(f"{repo}.btrdb: HTTP {s3}")
        raw = index_raw(stored)
        write_atomic(tmpi, raw)
        if not verify():
            # maybe a publish between the two requests: once more, signature first
            _, lastmod, sig = f.get(f"{repo}.btrdb.sig", limit=4096)
            write_atomic(tmpi + ".sig", sig)
            if not verify():
                raise SystemExit(f"{repo}.btrdb: bad signature")
    full_len = head_length(f, f"{repo}.btrdb") if measure_full and how == "patch" else None
    ts, entries = decode(raw)
    want = {e[3] for e in entries} | ({e[4] for e in entries if e[4] != ZERO} if with_files else set())
    have = {h for (h,) in sq.execute("select hash from blobs")}
    missing = sorted(want - have)
    dbrel = db_rel.rstrip("/") + "/"

    def fetch(h):
        s, _, b = f.get(dbrel + h.hex()[:2] + "/" + h.hex(), limit=MAX_BLOB)
        if s != 200 or sha(b) != h:
            raise SystemExit(f"blob {h.hex()}: HTTP {s} or wrong hash")
        return h, b

    with ThreadPoolExecutor(8) as ex:
        got = list(ex.map(fetch, missing))
    with sq:
        sq.executemany("insert or replace into blobs values (?, ?)", got)
    dd = load_dict("desc")
    fd = load_dict("files") if with_files else None
    blob = lambda h: sq.execute("select data from blobs where hash = ?", (h,)).fetchone()[0]
    db = {f"{n}-{v}/desc": unblob(blob(dh), dd) for n, v, a, dh, fh in entries}
    write_db(os.path.join(syncdir, f"{repo}.db"), db, ts)
    if with_files:
        # REPO.files holds each package's desc as well as its files list
        fl = dict(db, **{f"{n}-{v}/files": unblob(blob(fh), fd) for n, v, a, dh, fh in entries if fh != ZERO})
        write_db(os.path.join(syncdir, f"{repo}.files"), fl, ts)
    # drop blobs no repository synced into this directory names any more
    with sq:
        sq.execute("insert or replace into repos values (?, ?, ?)", (repo, lastmod, ts))
        sq.execute("create temp table keep (hash blob primary key)")
        sq.executemany("insert or ignore into keep values (?)", [(h,) for h in want])
        for (r,) in sq.execute("select repo from repos where repo != ?", (repo,)).fetchall():
            p = os.path.join(syncdir, f".{r}.btrdb")
            if os.path.exists(p):
                with open(p, "rb") as fh:
                    sq.executemany("insert or ignore into keep values (?)",
                                   [(h,) for e in read_index(fh.read())[1] for h in (e[3], e[4])])
        sq.execute("delete from blobs where hash not in (select hash from keep)")
    sq.execute("vacuum")
    os.replace(tmpi + ".sig", cur + ".sig")
    os.replace(tmpi, cur)
    full = ""
    if measure_full:
        full = f.bytes if how == "full" else f.bytes - len(pb) + full_len if full_len is not None else ""
    print(f"{repo}\t{f.bytes}\t{f.requests}\t{time.monotonic() - t0:.2f}\t{how}\t{full}")


def compare(a, b):
    ma, mb = db_members(a), db_members(b)
    if ma == mb:
        print(f"same: {len(ma)} entries")
        return 0
    print(f"differ: {len(set(ma) ^ set(mb))} names only in one, "
          f"{sum(1 for k in set(ma) & set(mb) if ma[k] != mb[k])} with other contents")
    return 1


def dump(path):
    with open(path, "rb") as f:
        stored = f.read()
    ts, es = read_index(stored)
    print(f"version {VERSION} timestamp {ts} packages {len(es)} stored {len(stored)}")
    for n, v, a, dh, fh in es:
        print(n, v, a, dh.hex(), fh.hex() if fh != ZERO else "-")


def main(argv):
    opts, args, i = {}, [], 0
    while i < len(argv):
        a = argv[i]
        if a in ("--key", "--keys", "--btrpkg", "--keep", "--keep-hours", "--db"):
            opts[a] = argv[i + 1]; i += 2
        elif a == "--files" and argv[:1] in (["train"], ["publish"]):
            if argv[0] == "train":
                opts["--files"] = argv[i + 1:]; break
            opts[a] = argv[i + 1]; i += 2
        elif a in ("--files", "--measure-full"):
            opts[a] = True; i += 1
        elif a.startswith("--"):
            raise SystemExit(f"unknown option {a}\n{__doc__}")
        else:
            args.append(a); i += 1
    bt = opts.get("--btrpkg", "btrpkg")
    match args:
        case ["train", d, *dbs] if dbs:
            train(d, dbs, opts.get("--files", []))
        case ["publish", root, arch, repo, db]:
            publish(root, arch, repo, db, opts.get("--files"), opts.get("--key"), bt, int(opts.get("--keep", 3)),
                    float(opts.get("--keep-hours", KEEP_HOURS)))
        case ["sync", url, sd, repo]:
            sync(url, sd, repo, opts.get("--keys"), bt, bool(opts.get("--files")), opts.get("--db", "../db"),
                 bool(opts.get("--measure-full")))
        case ["compare", a, b]:
            return compare(a, b)
        case ["dump", p]:
            dump(p)
        case _:
            raise SystemExit(__doc__)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
