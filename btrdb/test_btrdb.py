#!/usr/bin/env python3
"""Tests for btrdb.py: index round trip and rejection, blob checks, and a publish + sync round
through a local HTTP server, including a tampered blob and a tampered index (signature).
  python3 btrdb/test_btrdb.py      (BTRPKG=path/to/btrpkg; default target/release/btrpkg)
"""
import contextlib, functools, http.server, io, os, shutil, subprocess, sys, tarfile, tempfile, threading, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import btrdb
from compression import zstd

HERE = os.path.dirname(os.path.abspath(__file__))
BTRPKG = os.environ.get("BTRPKG", os.path.join(HERE, "..", "target", "release", "btrpkg"))


def desc(name, ver, arch="x86_64", extra=""):
    return (f"%FILENAME%\n{name}-{ver}-{arch}.pkg.tar.zst\n\n%NAME%\n{name}\n\n%VERSION%\n{ver}\n\n"
            f"%ARCH%\n{arch}\n\n{extra}").encode()


def make_db(path, pkgs, mtime):
    btrdb.write_db(path, {f"{n}-{v}/desc": desc(n, v, extra=x) for n, v, x in pkgs}, mtime)


class Index(unittest.TestCase):
    E = [("a", "1-1", "any", b"\1" * 32, btrdb.ZERO), ("b", "2:3.4-5", "x86_64", b"\2" * 32, b"\3" * 32)]

    def test_round_trip(self):
        ts, es = btrdb.read_index(zstd.compress(btrdb.encode(self.E, 42)))
        self.assertEqual((ts, es), (42, self.E))

    def test_every_truncation_rejected(self):
        raw = btrdb.encode(self.E, 42)
        for n in range(len(raw)):
            with self.assertRaises(ValueError, msg=n):
                btrdb.decode(raw[:n])

    def test_bad_indexes_rejected(self):
        raw = btrdb.encode(self.E, 42)
        bad = {
            "trailing": raw + b"\0",
            "version": raw[:8] + b"\1\0" + raw[10:],
            "flags": raw[:10] + b"\1\0" + raw[12:],
            "magic": b"BTRPKG\0\0" + raw[8:],
            "unsorted": btrdb.encode(self.E[::-1], 42),
            "duplicate": btrdb.encode([self.E[0], self.E[0]], 42),
            "count_high": raw[:12] + (3).to_bytes(4, "little") + raw[16:],
            "slash": btrdb.encode([("a/b", "1", "any", b"\1" * 32, btrdb.ZERO)], 42),
            "dot": btrdb.encode([("..", "1", "any", b"\1" * 32, btrdb.ZERO)], 42),
            "no_desc": btrdb.encode([("a", "1", "any", btrdb.ZERO, btrdb.ZERO)], 42),
        }
        for k, b in bad.items():
            with self.assertRaises(ValueError, msg=k):
                btrdb.decode(b)
        with self.assertRaises(ValueError):
            btrdb.read_index(zstd.compress(raw) + b"junk")
        with self.assertRaises(ValueError):
            btrdb.read_index(zstd.compress(raw)[:-3])


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a): pass


@unittest.skipUnless(os.access(BTRPKG, os.X_OK), f"needs btrpkg at {BTRPKG}")
class PublishSync(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.mkdtemp()
        self.srv = os.path.join(self.t, "srv")
        os.makedirs(self.srv)
        subprocess.run([BTRPKG, "keygen", os.path.join(self.t, "k")], check=True, stdout=subprocess.DEVNULL)
        self.keys = os.path.join(self.t, "keys")
        os.makedirs(self.keys)
        shutil.copy(os.path.join(self.t, "k.pub"), self.keys)
        h = functools.partial(Handler, directory=self.srv)
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), h)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.httpd.server_port}/x86_64"
        self.cache = os.path.join(self.t, "c")

    def tearDown(self):
        self.httpd.shutdown()
        shutil.rmtree(self.t)

    def publish(self, pkgs, mtime):
        db = os.path.join(self.t, f"in-{mtime}.db")
        make_db(db, pkgs, mtime)
        btrdb.publish(self.srv, "x86_64", "r", db, key=os.path.join(self.t, "k.sec"), btrpkg=BTRPKG)
        return db

    def sync(self):
        btrdb.sync(self.url, self.cache, "r", keys=self.keys, btrpkg=BTRPKG)

    def test_publish_sync_update(self):
        db1 = self.publish([("a", "1-1", ""), ("b", "1-1", "%DESC%\nbee\n")], 1000)
        self.sync()
        self.assertEqual(btrdb.db_members(os.path.join(self.cache, "r.db")), btrdb.db_members(db1))
        self.assertEqual(os.stat(os.path.join(self.cache, "r.db")).st_mtime, 1000)
        db2 = self.publish([("a", "1-1", ""), ("b", "2-1", "%DESC%\nbee\n"), ("c", "1-1", "")], 2000)
        self.sync()
        self.assertEqual(btrdb.db_members(os.path.join(self.cache, "r.db")), btrdb.db_members(db2))
        # the old b blob is no longer kept on the client
        import sqlite3
        n = sqlite3.connect(os.path.join(self.cache, "btrdb.sqlite")).execute("select count(*) from blobs").fetchone()[0]
        self.assertEqual(n, 3)

    def test_tampered_blob_and_index_refused(self):
        self.publish([("a", "1-1", "")], 1000)
        es = btrdb.read_index(open(os.path.join(self.srv, "x86_64", "r.btrdb"), "rb").read())[1]
        h = es[0][3].hex()
        p = os.path.join(self.srv, "db", h[:2], h)
        good = open(p, "rb").read()
        open(p, "wb").write(good[:-1] + bytes([good[-1] ^ 1]))
        with self.assertRaises(SystemExit):
            self.sync()
        self.assertFalse(os.path.exists(os.path.join(self.cache, "r.db")))
        open(p, "wb").write(good)
        ip = os.path.join(self.srv, "x86_64", "r.btrdb")
        raw = bytearray(zstd.decompress(open(ip, "rb").read()))
        # re-encoded, the same index still verifies (the signature covers the decompressed bytes)
        open(ip, "wb").write(zstd.compress(bytes(raw), level=1))
        self.sync()
        os.remove(os.path.join(self.cache, "r.db"))
        # a changed entry doesn't
        raw[-1] ^= 1
        open(ip, "wb").write(zstd.compress(bytes(raw), level=1))
        with self.assertRaises(SystemExit):
            self.sync()
        self.assertFalse(os.path.exists(os.path.join(self.cache, "r.db")))

    def out(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.sync()
        return buf.getvalue().split("\t")

    def test_patch(self):
        self.publish([("a", "1-1", ""), ("b", "1-1", "")], 1000)
        self.assertEqual(self.out()[4], "full")
        db2 = self.publish([("a", "1-1", ""), ("b", "2-1", ""), ("c", "1-1", "")], 2000)
        self.assertEqual(self.out()[4], "patch")
        self.assertEqual(btrdb.db_members(os.path.join(self.cache, "r.db")), btrdb.db_members(db2))
        self.assertEqual(self.out()[4], "unchanged")
        # a client two versions behind gets a direct patch too
        old_cache = self.cache + "-old"
        shutil.copytree(self.cache, old_cache)
        db3 = self.publish([("a", "1-1", ""), ("b", "3-1", ""), ("c", "1-1", "")], 3000)
        db4 = self.publish([("a", "1-1", ""), ("b", "3-1", ""), ("c", "2-1", "")], 4000)
        self.cache, cur = old_cache, self.cache
        self.assertEqual(self.out()[4], "patch")
        self.assertEqual(btrdb.db_members(os.path.join(self.cache, "r.db")), btrdb.db_members(db4))
        # a damaged patch: falls back to the whole index
        self.cache = cur
        h = btrdb.sha(btrdb.index_raw(open(os.path.join(cur, ".r.btrdb"), "rb").read())).hex()
        db5 = self.publish([("a", "2-1", ""), ("b", "3-1", ""), ("c", "2-1", "")], 5000)
        pp = os.path.join(self.srv, "x86_64", "r.btrdb.patch", h)
        pb = bytearray(open(pp, "rb").read()); pb[-2] ^= 1
        open(pp, "wb").write(pb)
        self.assertEqual(self.out()[4], "full")
        self.assertEqual(btrdb.db_members(os.path.join(self.cache, "r.db")), btrdb.db_members(db5))


if __name__ == "__main__":
    unittest.main()
