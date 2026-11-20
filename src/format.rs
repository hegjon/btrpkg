//! btrpkg format version 1.
//!
//! One index describes a package. The other two forms only append to it, so the header (and
//! later its signature) is the same in all three:
//!
//!   index      (.btridx)   header | chunk table | entries | small-file blob
//!   transport  (.pkg.btr)  index | chunk data, in table order
//!   cache file             index | zero padding up to image_base | image
//!
//! `image_base = align(index_len)`. In the cache file each chunk sits at `image_base + image_off`,
//! as a btrfs compressed extent when it is zstd, and every large file is a 4 KiB-aligned range of
//! the image that `install` reflinks.
//!
//! Header, 256 bytes, little endian:
//!
//! | offset | field |
//! |---|---|
//! | 0 | magic `BTRPKG\0\0` |
//! | 8 | format version, u32 = 1 |
//! | 12 | hash algorithm, u8: 1 = SHA-256; bytes 13..16 zero |
//! | 16 | number of chunks, u64 |
//! | 24 | image length, u64 |
//! | 32 / 40 | entries: stored (zstd) length / uncompressed length, u64 |
//! | 48 / 56 | small-file blob: stored (zstd) length / uncompressed length, u64 |
//! | 64 / 96 / 128 | SHA-256 of the chunk table / entries / blob, as stored |
//! | 160..224 | zero (reserved) |
//! | 224 | SHA-256 of bytes 0..224 (the header hash) |
//!
//! Chunk table entry, 88 bytes: image offset u64, unencoded length u32, stored length u32,
//! compression u8 (0 raw, 2 zstd), 7 zero bytes, SHA-256 of the stored bytes, SHA-256 of the
//! unencoded bytes (the file data zero-padded to 4 KiB). For a raw chunk both hashes are equal.
//! Chunks are in image order and cover the image without gaps.
//!
//! Readers check each section against the header before using it and before reading the next
//! one, so the chunk table can be used before the entries and blob have arrived.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;

pub const MAGIC: &[u8; 8] = b"BTRPKG\0\0";
pub const VERSION: u32 = 1;
pub const HASH_SHA256: u8 = 1;
pub const HDR: usize = 256;
pub const ENT: usize = 88;
pub const BLOCK: u64 = 4096;
pub const MAX_EXTENT: usize = 128 * 1024;
pub const COMP_NONE: u8 = 0;
pub const COMP_ZSTD: u8 = 2; // BTRFS_ENCODED_IO_COMPRESSION_ZSTD

pub const K_FILE: u8 = 0;
pub const K_DIR: u8 = 1;
pub const K_SYMLINK: u8 = 2;
pub const K_HARDLINK: u8 = 3;

/// Limits on lengths a header may announce (checked before anything is allocated). The largest
/// package seen (ansible) has 9.6 MB of entries and a 19.5 MB blob; caps leave room for much more
/// while keeping a decompression bomb small.
const MAX_SECTION: u64 = 1 << 32;
const MAX_ENTRIES_RAW: u64 = 256 << 20;
const MAX_BLOB_RAW: u64 = 1 << 30;
const META_LEVEL: i32 = 19;
/// btrfs reads back a zstd extent with a window of at most 128 KiB.
const CHUNK_WINDOW_LOG: u32 = 17;

pub type Hash = [u8; 32];

pub fn sha256(b: &[u8]) -> Hash {
    Sha256::digest(b).into()
}

/// Round up to a 4 KiB block. Only for lengths already known to be sane; use `checked_align` on
/// values read from a file.
pub fn align(x: u64) -> u64 {
    checked_align(x).expect("length too large to align")
}

pub fn checked_align(x: u64) -> Option<u64> {
    x.checked_add(BLOCK - 1).map(|v| v & !(BLOCK - 1))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub image_off: u64,
    pub unencoded_len: u32,
    pub encoded_len: u32,
    pub compression: u8,
    /// SHA-256 of the bytes as stored (the zstd frame, or the raw data)
    pub stored: Hash,
    /// SHA-256 of the unencoded bytes
    pub raw: Hash,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub kind: u8,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub size: u64,
    /// image offset (large file) or offset into the small-file blob (small file)
    pub data_off: u64,
    pub small: bool,
    pub path: Vec<u8>,
    pub target: Vec<u8>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub nchunks: u64,
    pub image_len: u64,
    pub entries_len: u64,
    pub entries_raw_len: u64,
    pub blob_len: u64,
    pub blob_raw_len: u64,
    pub table_hash: Hash,
    pub entries_hash: Hash,
    pub blob_hash: Hash,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Header {
    pub fn encode(&self) -> [u8; HDR] {
        let mut b = [0u8; HDR];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12] = HASH_SHA256;
        for (o, v) in [(16, self.nchunks), (24, self.image_len), (32, self.entries_len), (40, self.entries_raw_len),
                       (48, self.blob_len), (56, self.blob_raw_len)] {
            b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        b[64..96].copy_from_slice(&self.table_hash);
        b[96..128].copy_from_slice(&self.entries_hash);
        b[128..160].copy_from_slice(&self.blob_hash);
        let h = sha256(&b[..224]);
        b[224..].copy_from_slice(&h);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Header> {
        ensure!(b.len() >= HDR, "truncated header");
        ensure!(&b[0..8] == MAGIC, "not a btrpkg file");
        let v = u32_at(b, 8);
        ensure!(v == VERSION, "btrpkg format version {v}, this btrpkg reads version {VERSION}");
        ensure!(b[12] == HASH_SHA256, "unknown hash algorithm {}", b[12]);
        ensure!(sha256(&b[..224])[..] == b[224..HDR], "header checksum mismatch: corrupt or tampered header");
        ensure!(b[13..16] == [0; 3] && b[160..224].iter().all(|&x| x == 0), "reserved header bytes are set");
        let h = Header {
            nchunks: u64_at(b, 16),
            image_len: u64_at(b, 24),
            entries_len: u64_at(b, 32),
            entries_raw_len: u64_at(b, 40),
            blob_len: u64_at(b, 48),
            blob_raw_len: u64_at(b, 56),
            table_hash: b[64..96].try_into().unwrap(),
            entries_hash: b[96..128].try_into().unwrap(),
            blob_hash: b[128..160].try_into().unwrap(),
        };
        ensure!(h.nchunks <= MAX_SECTION / ENT as u64, "too many chunks");
        ensure!(h.image_len <= h.nchunks * MAX_EXTENT as u64, "image longer than its chunks");
        ensure!(h.entries_len < MAX_SECTION && h.blob_len < MAX_SECTION, "section too large");
        ensure!(h.entries_raw_len <= MAX_ENTRIES_RAW && h.blob_raw_len <= MAX_BLOB_RAW, "section too large");
        Ok(h)
    }

    /// The header hash: what identifies the package and what a signature covers.
    pub fn id(&self) -> Hash {
        self.encode()[224..].try_into().unwrap()
    }
    pub fn table_len(&self) -> u64 {
        self.nchunks * ENT as u64
    }
    pub fn index_len(&self) -> u64 {
        HDR as u64 + self.table_len() + self.entries_len + self.blob_len
    }
    pub fn image_base(&self) -> u64 {
        align(self.index_len())
    }
    /// Total length of the transport form (index plus chunk data).
    pub fn transport_len(&self, chunks: &[Chunk]) -> u64 {
        self.index_len() + chunks.iter().map(|c| c.encoded_len as u64).sum::<u64>()
    }
    pub fn cache_len(&self) -> u64 {
        self.image_base() + self.image_len
    }
}

pub fn encode_table(chunks: &[Chunk]) -> Vec<u8> {
    let mut v = Vec::with_capacity(chunks.len() * ENT);
    for c in chunks {
        v.extend_from_slice(&c.image_off.to_le_bytes());
        v.extend_from_slice(&c.unencoded_len.to_le_bytes());
        v.extend_from_slice(&c.encoded_len.to_le_bytes());
        v.push(c.compression);
        v.extend_from_slice(&[0u8; 7]);
        v.extend_from_slice(&c.stored);
        v.extend_from_slice(&c.raw);
    }
    v
}

pub fn decode_table(b: &[u8], image_len: u64) -> Result<Vec<Chunk>> {
    ensure!(b.len() % ENT == 0, "chunk table has the wrong length");
    let mut out = Vec::with_capacity(b.len() / ENT);
    let mut next = 0u64;
    for (i, e) in b.chunks_exact(ENT).enumerate() {
        let c = Chunk {
            image_off: u64_at(e, 0),
            unencoded_len: u32_at(e, 8),
            encoded_len: u32_at(e, 12),
            compression: e[16],
            stored: e[24..56].try_into().unwrap(),
            raw: e[56..88].try_into().unwrap(),
        };
        ensure!(e[17..24] == [0; 7], "chunk {i}: reserved bytes are set");
        ensure!(c.image_off == next, "chunk {i}: not where the previous chunk ends");
        ensure!(c.unencoded_len > 0 && c.unencoded_len as usize <= MAX_EXTENT && c.unencoded_len as u64 % BLOCK == 0,
            "chunk {i}: bad length");
        match c.compression {
            COMP_NONE => ensure!(c.encoded_len == c.unencoded_len && c.stored == c.raw, "chunk {i}: bad raw chunk"),
            // btrfs only takes a compressed extent that is smaller than its data
            COMP_ZSTD => ensure!(c.encoded_len > 0 && c.encoded_len < c.unencoded_len, "chunk {i}: bad zstd chunk"),
            x => anyhow::bail!("chunk {i}: unknown compression {x}"),
        }
        next += c.unencoded_len as u64;
        out.push(c);
    }
    ensure!(next == image_len, "chunks don't cover the image");
    Ok(out)
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn bytes(&mut self, v: &[u8]) { self.u32(v.len() as u32); self.0.extend_from_slice(v) }
}
struct R<'a>(&'a [u8], usize);
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.0.len() - self.1, "truncated entries");
        let s = &self.0[self.1..self.1 + n];
        self.1 += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }
    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into()?)) }
    fn u64(&mut self) -> Result<u64> { Ok(u64::from_le_bytes(self.take(8)?.try_into()?)) }
    fn bytes(&mut self) -> Result<Vec<u8>> { let n = self.u32()? as usize; Ok(self.take(n)?.to_vec()) }
}

pub fn encode_entries(entries: &[Entry]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(entries.len() as u32);
    for e in entries {
        w.u8(e.kind);
        w.u32(e.mode);
        w.u32(e.uid);
        w.u32(e.gid);
        w.u64(e.mtime as u64);
        w.u64(e.size);
        w.u64(e.data_off);
        w.u8(e.small as u8);
        w.bytes(&e.path);
        w.bytes(&e.target);
        w.u32(e.xattrs.len() as u32);
        for (k, v) in &e.xattrs {
            w.bytes(k);
            w.bytes(v);
        }
    }
    w.0
}

pub fn decode_entries(b: &[u8]) -> Result<Vec<Entry>> {
    let mut r = R(b, 0);
    let n = r.u32()? as usize;
    // every entry takes at least 46 bytes, so a bogus count can't make us allocate much
    let mut v = Vec::with_capacity(n.min(b.len() / 46));
    for _ in 0..n {
        let mut e = Entry {
            kind: r.u8()?,
            mode: r.u32()?,
            uid: r.u32()?,
            gid: r.u32()?,
            mtime: r.u64()? as i64,
            size: r.u64()?,
            data_off: r.u64()?,
            small: match r.u8()? { 0 => false, 1 => true, x => anyhow::bail!("bad small flag {x}") },
            path: r.bytes()?,
            target: r.bytes()?,
            xattrs: vec![],
        };
        let nx = r.u32()?;
        for _ in 0..nx {
            e.xattrs.push((r.bytes()?, r.bytes()?));
        }
        v.push(e);
    }
    ensure!(r.1 == b.len(), "trailing bytes after the entries");
    Ok(v)
}

fn safe_path(p: &[u8]) -> bool {
    !p.is_empty() && !p.starts_with(b"/") && !p.contains(&0)
        && p.split(|&c| c == b'/').all(|c| !c.is_empty() && c != b"." && c != b"..")
}

/// Everything `install` relies on: safe paths, known kinds, data inside the image or blob, no
/// path that leads through a symlink or file of the same package, and hardlinks only to files
/// the package placed before. (Symlinks already on disk are refused by `install` itself.)
pub fn check_entries(entries: &[Entry], image_len: u64, blob_len: u64) -> Result<()> {
    let mut files: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    let mut not_dirs: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for e in entries {
        let name = || String::from_utf8_lossy(&e.path).into_owned();
        ensure!(safe_path(&e.path), "unsafe path {:?}", name());
        for (i, _) in e.path.iter().enumerate().filter(|(_, &c)| c == b'/') {
            ensure!(!not_dirs.contains(&e.path[..i]), "{}: leads through a symlink or file of the package", name());
        }
        ensure!(!e.target.contains(&0), "{}: NUL in link target", name());
        ensure!(e.xattrs.iter().all(|(k, _)| !k.is_empty() && !k.contains(&0)), "{}: bad xattr name", name());
        match e.kind {
            K_DIR => {}
            K_SYMLINK => ensure!(!e.target.is_empty(), "{}: symlink without target", name()),
            K_HARDLINK => ensure!(safe_path(&e.target) && files.contains(e.target.as_slice()),
                "{}: hardlink to something other than an earlier file of the package", name()),
            K_FILE if e.small => ensure!(e.data_off.checked_add(e.size).is_some_and(|end| end <= blob_len),
                "{}: small file outside the blob", name()),
            K_FILE => ensure!(e.size > 0 && e.data_off % BLOCK == 0
                && checked_align(e.size).and_then(|a| e.data_off.checked_add(a)).is_some_and(|end| end <= image_len),
                "{}: file outside the image", name()),
            k => anyhow::bail!("{}: unknown entry kind {k}", name()),
        }
        match e.kind {
            K_FILE => { files.insert(&e.path); not_dirs.insert(&e.path); }
            K_SYMLINK | K_HARDLINK => { not_dirs.insert(&e.path); }
            _ => ensure!(!not_dirs.contains(e.path.as_slice()), "{}: a directory where the package has a file or symlink", name()),
        }
    }
    Ok(())
}

thread_local! {
    static DCTX: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { std::cell::RefCell::new(None) };
}

/// The unencoded data of a zstd chunk, decoded the way btrfs will read it back: one frame, window
/// at most 128 KiB, no dictionary, exactly `unencoded_len` bytes.
pub fn decode_chunk(c: &Chunk, d: &[u8]) -> Result<Vec<u8>> {
    ensure!(d.len() >= 5 && d[..4] == [0x28, 0xb5, 0x2f, 0xfd], "not a zstd frame");
    // frame header descriptor: no dictionary ID, no checksum
    ensure!(d[4] & 0x07 == 0, "zstd frame with a dictionary ID or checksum");
    ensure!(zstd::zstd_safe::find_frame_compressed_size(d).is_ok_and(|n| n == d.len()), "not exactly one zstd frame");
    DCTX.with(|cell| -> Result<Vec<u8>> {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            let mut z = zstd::bulk::Decompressor::new()?;
            z.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(CHUNK_WINDOW_LOG))?;
            *cell = Some(z);
        }
        let out = cell.as_mut().unwrap().decompress(d, c.unencoded_len as usize).context("zstd chunk")?;
        ensure!(out.len() == c.unencoded_len as usize, "zstd chunk decoded to the wrong length");
        Ok(out)
    })
}

/// Check a chunk's bytes before anything is written: the stored hash, and the hash of the data
/// it decodes to, which reuse across packages relies on. The kernel stores encoded frames without
/// looking at them, so this is the only check before the data is on disk.
pub fn check_chunk(i: usize, c: &Chunk, d: &[u8]) -> Result<()> {
    ensure!(d.len() == c.encoded_len as usize && sha256(d) == c.stored, "chunk {i} is corrupt");
    match c.compression {
        COMP_ZSTD => {
            let plain = decode_chunk(c, d).with_context(|| format!("chunk {i}"))?;
            ensure!(sha256(&plain) == c.raw, "chunk {i}: its data doesn't match the unencoded hash");
        }
        COMP_NONE => ensure!(c.stored == c.raw && d.len() == c.unencoded_len as usize, "chunk {i}: its data doesn't match the unencoded hash"),
        x => anyhow::bail!("chunk {i}: unknown compression {x}"),
    }
    Ok(())
}

pub struct Index {
    pub hdr: Header,
    pub chunks: Vec<Chunk>,
    pub entries: Vec<Entry>,
    pub small: Vec<u8>,
    /// the index exactly as stored
    pub bytes: Vec<u8>,
}

/// Index bytes from already compressed entries and blob.
pub fn assemble(chunks: &[Chunk], image_len: u64, entries_z: &[u8], entries_raw_len: u64, blob_z: &[u8], blob_raw_len: u64) -> Vec<u8> {
    let table = encode_table(chunks);
    let hdr = Header {
        nchunks: chunks.len() as u64,
        image_len,
        entries_len: entries_z.len() as u64,
        entries_raw_len,
        blob_len: blob_z.len() as u64,
        blob_raw_len,
        table_hash: sha256(&table),
        entries_hash: sha256(entries_z),
        blob_hash: sha256(blob_z),
    };
    let mut b = Vec::with_capacity(HDR + table.len() + entries_z.len() + blob_z.len());
    b.extend_from_slice(&hdr.encode());
    b.extend_from_slice(&table);
    b.extend_from_slice(entries_z);
    b.extend_from_slice(blob_z);
    b
}

pub fn build_index(entries: &[Entry], small: &[u8], chunks: &[Chunk], image_len: u64) -> Result<Vec<u8>> {
    check_entries(entries, image_len, small.len() as u64)?;
    let er = encode_entries(entries);
    let ez = zstd::bulk::Compressor::new(META_LEVEL)?.compress(&er)?;
    let bz = zstd::bulk::Compressor::new(META_LEVEL)?.compress(small)?;
    Ok(assemble(chunks, image_len, &ez, er.len() as u64, &bz, small.len() as u64))
}

impl Index {
    /// The same index with a different chunk table (a publisher reusing stored chunks).
    pub fn with_chunks(&self, chunks: &[Chunk]) -> Vec<u8> {
        let e0 = HDR + self.hdr.table_len() as usize;
        let b0 = e0 + self.hdr.entries_len as usize;
        assemble(chunks, self.hdr.image_len, &self.bytes[e0..b0], self.hdr.entries_raw_len,
            &self.bytes[b0..], self.hdr.blob_raw_len)
    }
}

/// Read exactly `n` bytes without trusting `n` for the allocation.
pub fn read_n(r: &mut impl Read, n: u64, what: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    r.take(n).read_to_end(&mut v).with_context(|| format!("reading {what}"))?;
    ensure!(v.len() as u64 == n, "truncated package ({what})");
    Ok(v)
}

fn decompress_exact(z: &[u8], raw_len: u64, what: &str) -> Result<Vec<u8>> {
    let mut d = zstd::stream::read::Decoder::new(z)?.single_frame();
    let mut out = Vec::new();
    (&mut d).take(raw_len + 1).read_to_end(&mut out).with_context(|| format!("decompressing {what}"))?;
    ensure!(out.len() as u64 == raw_len, "{what} decompressed to the wrong length");
    Ok(out)
}

/// Header and chunk table only, checked: enough to start fetching chunks.
pub fn read_head(r: &mut impl Read) -> Result<(Header, Vec<u8>, Vec<Chunk>, Vec<u8>)> {
    read_head_checked(r, &|_| Ok(()))
}

/// Like `read_head`, but `trust` sees the header (whose hash covers every section) before
/// anything else is read: an expected ID or a signature.
pub fn read_head_checked(r: &mut impl Read, trust: &dyn Fn(&[u8]) -> Result<()>) -> Result<(Header, Vec<u8>, Vec<Chunk>, Vec<u8>)> {
    let hb = read_n(r, HDR as u64, "header")?;
    let hdr = Header::decode(&hb)?;
    trust(&hb)?;
    let tb = read_n(r, hdr.table_len(), "chunk table")?;
    ensure!(sha256(&tb) == hdr.table_hash, "chunk table checksum mismatch");
    let chunks = decode_table(&tb, hdr.image_len)?;
    Ok((hdr, hb, chunks, tb))
}

/// Read and check a whole index, section by section.
pub fn read_index(r: &mut impl Read) -> Result<Index> {
    read_index_checked(r, &|_| Ok(()))
}

pub fn read_index_checked(r: &mut impl Read, trust: &dyn Fn(&[u8]) -> Result<()>) -> Result<Index> {
    let (hdr, hb, chunks, tb) = read_head_checked(r, trust)?;
    let ez = read_n(r, hdr.entries_len, "entries")?;
    ensure!(sha256(&ez) == hdr.entries_hash, "entries checksum mismatch");
    let entries = decode_entries(&decompress_exact(&ez, hdr.entries_raw_len, "entries")?)?;
    let bz = read_n(r, hdr.blob_len, "small-file blob")?;
    ensure!(sha256(&bz) == hdr.blob_hash, "small-file blob checksum mismatch");
    let small = decompress_exact(&bz, hdr.blob_raw_len, "small-file blob")?;
    check_entries(&entries, hdr.image_len, small.len() as u64)?;
    let mut bytes = hb;
    bytes.extend_from_slice(&tb);
    bytes.extend_from_slice(&ez);
    bytes.extend_from_slice(&bz);
    Ok(Index { hdr, chunks, entries, small, bytes })
}

pub fn parse_index(mut b: &[u8]) -> Result<Index> {
    read_index(&mut b)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> (Vec<Entry>, Vec<u8>, Vec<Chunk>, u64) {
        let small = b"hello\nworld\n".to_vec();
        let data = vec![7u8; 8192];
        let z = zstd::bulk::compress(&data, 3).unwrap();
        let c0 = Chunk { image_off: 0, unencoded_len: 8192, encoded_len: z.len() as u32, compression: COMP_ZSTD,
            stored: sha256(&z), raw: sha256(&data) };
        let r = vec![1u8; 4096];
        let c1 = Chunk { image_off: 8192, unencoded_len: 4096, encoded_len: 4096, compression: COMP_NONE,
            stored: sha256(&r), raw: sha256(&r) };
        let e = |kind, path: &str, size, off, small| Entry { kind, mode: 0o644, uid: 0, gid: 0, mtime: 1, size, data_off: off,
            small, path: path.into(), target: vec![], xattrs: vec![] };
        let mut ents = vec![e(K_DIR, "usr", 0, 0, false), e(K_FILE, "usr/a", 6, 0, true), e(K_FILE, "usr/b", 6, 6, true),
            e(K_FILE, "usr/big", 8000, 0, false), e(K_FILE, "usr/raw", 4096, 8192, false)];
        ents.push(Entry { kind: K_SYMLINK, path: b"usr/l".to_vec(), target: b"big".to_vec(), ..ents[0].clone() });
        ents[3].xattrs.push((b"user.x".to_vec(), b"1".to_vec()));
        (ents, small, vec![c0, c1], 12288)
    }

    #[test]
    fn round_trip() {
        let (ents, small, chunks, image_len) = sample();
        let b = build_index(&ents, &small, &chunks, image_len).unwrap();
        let idx = parse_index(&b).unwrap();
        assert_eq!(idx.entries, ents);
        assert_eq!(idx.small, small);
        assert_eq!(idx.chunks, chunks);
        assert_eq!(idx.bytes, b);
        assert_eq!(idx.hdr.index_len(), b.len() as u64);
        assert_eq!(idx.hdr.image_base() % BLOCK, 0);
        assert_eq!(idx.with_chunks(&chunks), b);
        // the header hash identifies the index: same input, same bytes
        assert_eq!(build_index(&ents, &small, &chunks, image_len).unwrap(), b);
    }

    #[test]
    fn every_flipped_byte_is_rejected() {
        let (ents, small, chunks, image_len) = sample();
        let b = build_index(&ents, &small, &chunks, image_len).unwrap();
        for i in 0..b.len() {
            for bit in [0x01u8, 0x80] {
                let mut c = b.clone();
                c[i] ^= bit;
                assert!(parse_index(&c).is_err(), "flipping bit {bit:#x} of byte {i} was accepted");
            }
        }
    }

    #[test]
    fn truncated_index_is_rejected() {
        let (ents, small, chunks, image_len) = sample();
        let b = build_index(&ents, &small, &chunks, image_len).unwrap();
        for n in 0..b.len() {
            assert!(parse_index(&b[..n]).is_err(), "index cut at {n} was accepted");
        }
    }

    #[test]
    fn huge_lengths_fail_without_allocating() {
        // a header with a valid checksum that announces 4 GiB sections and is followed by nothing
        let h = Header { nchunks: (MAX_SECTION / ENT as u64) - 1, image_len: 4096, entries_len: MAX_SECTION - 1,
            entries_raw_len: MAX_ENTRIES_RAW, blob_len: 1, blob_raw_len: 1, ..Default::default() };
        let e = parse_index(&h.encode()).err().unwrap().to_string();
        assert!(e.contains("truncated"), "{e}");
        let too_big = Header { entries_len: MAX_SECTION, ..h.clone() };
        assert!(Header::decode(&too_big.encode()).is_err());
        let bomb = Header { entries_raw_len: MAX_ENTRIES_RAW + 1, ..h.clone() };
        assert!(Header::decode(&bomb.encode()).is_err());
        let bomb = Header { blob_raw_len: MAX_BLOB_RAW + 1, ..h.clone() };
        assert!(Header::decode(&bomb.encode()).is_err());
    }

    #[test]
    fn unsafe_entries_are_rejected() {
        let (ents, small, chunks, image_len) = sample();
        let bad: Vec<Box<dyn Fn(&mut Entry)>> = vec![
            Box::new(|e| e.path = b"../etc/passwd".to_vec()),
            Box::new(|e| e.path = b"/etc/passwd".to_vec()),
            Box::new(|e| e.path = b"usr/./x".to_vec()),
            Box::new(|e| e.path = b"usr//x".to_vec()),
            Box::new(|e| e.path = b"usr/x\0y".to_vec()),
            Box::new(|e| e.path = vec![]),
            Box::new(|e| { e.kind = K_HARDLINK; e.target = b"../../x".to_vec() }),
            Box::new(|e| { e.kind = K_SYMLINK; e.target = vec![] }),
            Box::new(|e| e.kind = 9),
            Box::new(|e| { e.kind = K_FILE; e.small = true; e.data_off = 10; e.size = 100 }),
            Box::new(|e| { e.kind = K_FILE; e.small = false; e.data_off = 8192; e.size = 8192 }),
            Box::new(|e| { e.kind = K_FILE; e.small = false; e.data_off = 100; e.size = 100 }),
            Box::new(|e| { e.kind = K_FILE; e.small = false; e.data_off = u64::MAX - 10; e.size = 100 }),
        ];
        for (i, f) in bad.iter().enumerate() {
            let mut e2 = ents.clone();
            f(&mut e2[1]);
            assert!(build_index(&e2, &small, &chunks, image_len).is_err(), "case {i} built");
            // and a reader rejects it even if a writer skipped the check
            let er = encode_entries(&e2);
            let ez = zstd::bulk::compress(&er, 3).unwrap();
            let bz = zstd::bulk::compress(&small, 3).unwrap();
            let b = assemble(&chunks, image_len, &ez, er.len() as u64, &bz, small.len() as u64);
            assert!(parse_index(&b).is_err(), "case {i} parsed");
        }
    }

    #[test]
    fn bad_tables_are_rejected() {
        let (ents, small, chunks, image_len) = sample();
        let bad: Vec<Box<dyn Fn(&mut Vec<Chunk>)>> = vec![
            Box::new(|c| c[1].image_off = 4096),                       // overlap
            Box::new(|c| c[1].image_off = 16384),                      // gap
            Box::new(|c| c[0].unencoded_len = 8000),                   // unaligned
            Box::new(|c| c[0].unencoded_len = 256 * 1024),             // too large
            Box::new(|c| c[0].encoded_len = c[0].unencoded_len),       // compressed but not smaller
            Box::new(|c| c[1].stored[0] ^= 1),                         // raw chunk with two hashes
            Box::new(|c| c[1].encoded_len = 100),                      // raw chunk with a stored length
            Box::new(|c| c[0].compression = 1),                        // zlib
            Box::new(|c| { c.pop(); }),                                // image not covered
        ];
        for (i, f) in bad.iter().enumerate() {
            let mut c2 = chunks.clone();
            f(&mut c2);
            let b = build_index(&ents, &small, &c2, image_len).unwrap();
            assert!(parse_index(&b).is_err(), "case {i} parsed");
        }
    }
}
