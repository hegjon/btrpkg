//! btrpkg spike: pacman packages whose file data is shipped as btrfs zstd extents.
//!
//!   btrpkg convert [-l LEVEL] IN.pkg.tar.zst OUT.btrpkg
//!   btrpkg fetch [--no-encoded] IN.btrpkg|- CACHEFILE      (needs CAP_SYS_ADMIN for encoded writes)
//!   btrpkg install [--copy] CACHEFILE ROOT
//!   btrpkg info FILE
//!
//! See README.md for the format.

use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"BTRPKG\0\0";
const VERSION: u32 = 0;
const FLAG_MATERIALIZED: u32 = 1;
const HDR: usize = 64;
const TABLE_ENT: usize = 24;
const BLOCK: u64 = 4096;
const MAX_EXTENT: usize = 128 * 1024;
const COMP_NONE: u8 = 0;
const COMP_ZSTD: u8 = 2; // BTRFS_ENCODED_IO_COMPRESSION_ZSTD

const K_FILE: u8 = 0;
const K_DIR: u8 = 1;
const K_SYMLINK: u8 = 2;
const K_HARDLINK: u8 = 3;

fn align(x: u64) -> u64 {
    (x + BLOCK - 1) & !(BLOCK - 1)
}

// ---------------------------------------------------------------- format

#[derive(Debug, Clone, Default)]
struct Entry {
    kind: u8,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    size: u64,
    /// image offset (large file) or offset into the small blob (small file)
    data_off: u64,
    small: bool,
    path: Vec<u8>,
    target: Vec<u8>,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    image_off: u64,
    unencoded_len: u32,
    encoded_len: u32,
    compression: u8,
}

#[derive(Debug, Default)]
struct Header {
    flags: u32,
    meta_len: u64,
    nchunks: u64,
    image_len: u64,
    image_base: u64,
    meta_raw_len: u64,
}

impl Header {
    fn encode(&self) -> [u8; HDR] {
        let mut b = [0u8; HDR];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.meta_len.to_le_bytes());
        b[24..32].copy_from_slice(&self.nchunks.to_le_bytes());
        b[32..40].copy_from_slice(&self.image_len.to_le_bytes());
        b[40..48].copy_from_slice(&self.image_base.to_le_bytes());
        b[48..56].copy_from_slice(&self.meta_raw_len.to_le_bytes());
        b
    }
    fn decode(b: &[u8]) -> Result<Header> {
        ensure!(b.len() >= HDR && &b[0..8] == MAGIC, "not a btrpkg file");
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        ensure!(u32_at(8) == VERSION, "unsupported btrpkg version {}", u32_at(8));
        let h = Header {
            flags: u32_at(12),
            meta_len: u64_at(16),
            nchunks: u64_at(24),
            image_len: u64_at(32),
            image_base: u64_at(40),
            meta_raw_len: u64_at(48),
        };
        ensure!(h.meta_len < 1 << 32 && h.meta_raw_len < 1 << 34 && h.nchunks < 1 << 32, "header sizes out of range");
        Ok(h)
    }
    fn table_len(&self) -> u64 {
        self.nchunks * TABLE_ENT as u64
    }
}

fn encode_table(chunks: &[Chunk]) -> Vec<u8> {
    let mut v = Vec::with_capacity(chunks.len() * TABLE_ENT);
    for c in chunks {
        v.extend_from_slice(&c.image_off.to_le_bytes());
        v.extend_from_slice(&c.unencoded_len.to_le_bytes());
        v.extend_from_slice(&c.encoded_len.to_le_bytes());
        v.push(c.compression);
        v.extend_from_slice(&[0u8; 7]);
    }
    v
}

fn decode_table(b: &[u8], image_len: u64) -> Result<Vec<Chunk>> {
    let mut out = Vec::with_capacity(b.len() / TABLE_ENT);
    for e in b.chunks_exact(TABLE_ENT) {
        let c = Chunk {
            image_off: u64::from_le_bytes(e[0..8].try_into().unwrap()),
            unencoded_len: u32::from_le_bytes(e[8..12].try_into().unwrap()),
            encoded_len: u32::from_le_bytes(e[12..16].try_into().unwrap()),
            compression: e[16],
        };
        ensure!(c.image_off % BLOCK == 0 && c.unencoded_len as u64 % BLOCK == 0, "unaligned chunk");
        ensure!(c.unencoded_len as usize <= MAX_EXTENT && c.unencoded_len > 0, "chunk too large");
        ensure!(c.image_off + c.unencoded_len as u64 <= image_len, "chunk past image end");
        ensure!(c.compression == COMP_NONE || c.compression == COMP_ZSTD, "unknown compression");
        ensure!(c.compression != COMP_NONE || c.encoded_len == c.unencoded_len, "raw chunk length mismatch");
        out.push(c);
    }
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
        ensure!(self.1 + n <= self.0.len(), "truncated manifest");
        let s = &self.0[self.1..self.1 + n];
        self.1 += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }
    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into()?)) }
    fn u64(&mut self) -> Result<u64> { Ok(u64::from_le_bytes(self.take(8)?.try_into()?)) }
    fn bytes(&mut self) -> Result<Vec<u8>> { let n = self.u32()? as usize; Ok(self.take(n)?.to_vec()) }
}

fn encode_meta(entries: &[Entry], small: &[u8]) -> Vec<u8> {
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
    w.0.extend_from_slice(small);
    w.0
}

fn decode_meta(b: &[u8]) -> Result<(Vec<Entry>, Vec<u8>)> {
    let mut r = R(b, 0);
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let mut e = Entry {
            kind: r.u8()?,
            mode: r.u32()?,
            uid: r.u32()?,
            gid: r.u32()?,
            mtime: r.u64()? as i64,
            size: r.u64()?,
            data_off: r.u64()?,
            small: r.u8()? != 0,
            path: r.bytes()?,
            target: r.bytes()?,
            xattrs: vec![],
        };
        let nx = r.u32()?;
        for _ in 0..nx {
            e.xattrs.push((r.bytes()?, r.bytes()?));
        }
        ensure!(!e.path.is_empty() && !e.path.starts_with(b"/") && !e.path.split(|&c| c == b'/').any(|c| c == b".."),
            "unsafe path {:?}", String::from_utf8_lossy(&e.path));
        v.push(e);
    }
    Ok((v, b[r.1..].to_vec()))
}

// ---------------------------------------------------------------- convert

fn convert(input: &Path, output: &Path, level: i32, small_max: u64) -> Result<()> {
    let f = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let dec = zstd::stream::read::Decoder::new(BufReader::new(f))?;
    let mut ar = tar::Archive::new(dec);
    let mut entries = Vec::new();
    let mut small = Vec::new();
    let mut jobs: Vec<(u64, Vec<u8>)> = Vec::new(); // image_off, padded chunk data
    let mut cursor = 0u64;
    let mut seen: HashMap<[u8; 32], (bool, u64)> = HashMap::new();
    #[derive(Default, Debug)]
    struct ConvStats { dedup_files: u64, dedup_bytes: u64 }
    let mut stats = ConvStats::default();

    for ent in ar.entries()? {
        let mut ent = ent?;
        let path = ent.path_bytes().into_owned();
        let mut path = path;
        while path.ends_with(b"/") { path.pop(); }
        while path.starts_with(b"./") { path.drain(..2); }
        if path.is_empty() || path == b"." { continue; }
        let mut xattrs = Vec::new();
        if let Some(exts) = ent.pax_extensions()? {
            for x in exts {
                let x = x?;
                if let Some(k) = x.key_bytes().strip_prefix(b"SCHILY.xattr.") {
                    xattrs.push((k.to_vec(), x.value_bytes().to_vec()));
                }
            }
        }
        let h = ent.header();
        let mut e = Entry {
            mode: h.mode()?,
            uid: h.uid()? as u32,
            gid: h.gid()? as u32,
            mtime: h.mtime()? as i64,
            path,
            xattrs,
            ..Default::default()
        };
        match h.entry_type() {
            tar::EntryType::Directory => e.kind = K_DIR,
            tar::EntryType::Symlink => {
                e.kind = K_SYMLINK;
                e.target = ent.link_name_bytes().context("symlink without target")?.into_owned();
            }
            tar::EntryType::Link => {
                e.kind = K_HARDLINK;
                let mut t = ent.link_name_bytes().context("hardlink without target")?.into_owned();
                while t.starts_with(b"./") { t.drain(..2); }
                e.target = t;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                e.kind = K_FILE;
                let mut data = Vec::with_capacity(ent.size() as usize);
                ent.read_to_end(&mut data)?;
                e.size = data.len() as u64;
                // Identical contents are stored once; installs reflink the same range again.
                let key: [u8; 32] = Sha256::digest(&data).into();
                if let Some(&(small, off)) = seen.get(&key) {
                    e.small = small;
                    e.data_off = off;
                    stats.dedup_files += 1;
                    stats.dedup_bytes += e.size;
                } else if e.size <= small_max {
                    e.small = true;
                    e.data_off = small.len() as u64;
                    small.extend_from_slice(&data);
                } else {
                    e.data_off = cursor;
                    for piece in data.chunks(MAX_EXTENT) {
                        let mut p = piece.to_vec();
                        p.resize(align(p.len() as u64) as usize, 0);
                        let len = p.len() as u64;
                        jobs.push((cursor, p));
                        cursor += len;
                    }
                }
                seen.insert(key, (e.small, e.data_off));
            }
            t => bail!("unsupported tar entry type {:?} for {}", t, String::from_utf8_lossy(&e.path)),
        }
        entries.push(e);
    }

    // Compress every extent independently: zstd frames btrfs can read back
    // (window <= 128 KiB, no checksum, no dictionary).
    let compressed: Vec<(Chunk, Vec<u8>)> = jobs
        .into_par_iter()
        .map(|(off, data)| -> Result<(Chunk, Vec<u8>)> {
            let mut c = zstd::bulk::Compressor::new(level)?;
            c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(17))?;
            c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(false))?;
            c.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
            c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
            let z = c.compress(&data)?;
            // Only worth an encoded extent if it saves at least one block on disk.
            if align(z.len() as u64) < data.len() as u64 {
                Ok((Chunk { image_off: off, unencoded_len: data.len() as u32, encoded_len: z.len() as u32, compression: COMP_ZSTD }, z))
            } else {
                Ok((Chunk { image_off: off, unencoded_len: data.len() as u32, encoded_len: data.len() as u32, compression: COMP_NONE }, data))
            }
        })
        .collect::<Result<_>>()?;

    let meta_raw = encode_meta(&entries, &small);
    let meta = zstd::bulk::Compressor::new(19)?.compress(&meta_raw)?;
    let chunks: Vec<Chunk> = compressed.iter().map(|(c, _)| *c).collect();
    let hdr = Header {
        flags: 0,
        meta_len: meta.len() as u64,
        nchunks: chunks.len() as u64,
        image_len: cursor,
        image_base: 0,
        meta_raw_len: meta_raw.len() as u64,
    };
    let tmp = output.with_extension("btrpkg.part");
    let mut out = io::BufWriter::new(File::create(&tmp)?);
    let mut hash = Sha256::new();
    let mut put = |b: &[u8], out: &mut io::BufWriter<File>| -> Result<()> { hash.update(b); out.write_all(b)?; Ok(()) };
    put(&hdr.encode(), &mut out)?;
    put(&meta, &mut out)?;
    put(&encode_table(&chunks), &mut out)?;
    for (_, d) in &compressed {
        put(d, &mut out)?;
    }
    let digest = hash.finalize();
    out.write_all(&digest)?;
    out.into_inner()?.sync_all()?;
    fs::rename(&tmp, output)?;
    if stats.dedup_files > 0 { eprintln!("convert: {:?}", stats); }
    Ok(())
}

// ---------------------------------------------------------------- btrfs ioctls

#[repr(C)]
struct EncodedIoArgs {
    iov: *const libc::iovec,
    iovcnt: libc::c_ulong,
    offset: i64,
    flags: u64,
    len: u64,
    unencoded_len: u64,
    unencoded_offset: u64,
    compression: u32,
    encryption: u32,
    reserved: [u8; 64],
}
const _: () = assert!(std::mem::size_of::<EncodedIoArgs>() == 128);
// _IOW(0x94, 64, struct btrfs_ioctl_encoded_io_args)
const BTRFS_IOC_ENCODED_WRITE: libc::c_ulong = 0x4080_9440;
// _IOW(0x94, 13, struct file_clone_range)
const FICLONERANGE: libc::c_ulong = 0x4020_940d;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_NOCOMP_FL: libc::c_long = 0x400;

fn encoded_write(f: &File, off: u64, unencoded_len: u64, data: &[u8]) -> io::Result<()> {
    let iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let args = EncodedIoArgs {
        iov: &iov,
        iovcnt: 1,
        offset: off as i64,
        flags: 0,
        len: unencoded_len,
        unencoded_len,
        unencoded_offset: 0,
        compression: COMP_ZSTD as u32,
        encryption: 0,
        reserved: [0; 64],
    };
    let r = unsafe { libc::ioctl(f.as_raw_fd(), BTRFS_IOC_ENCODED_WRITE, &args) };
    if r < 0 { return Err(io::Error::last_os_error()); }
    ensure_io(r as usize == data.len(), "short encoded write")
}

fn ensure_io(c: bool, m: &str) -> io::Result<()> {
    if c { Ok(()) } else { Err(io::Error::other(m.to_string())) }
}

#[repr(C)]
struct FileCloneRange { src_fd: i64, src_offset: u64, src_length: u64, dest_offset: u64 }

fn clone_range(src: &File, src_off: u64, len: u64, dst: &File) -> io::Result<()> {
    let a = FileCloneRange { src_fd: src.as_raw_fd() as i64, src_offset: src_off, src_length: len, dest_offset: 0 };
    let r = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONERANGE, &a) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn set_nocomp(f: &File) {
    let mut flags: libc::c_long = 0;
    unsafe {
        if libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS, &mut flags) == 0 {
            flags |= FS_NOCOMP_FL;
            libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS, &flags);
        }
    }
}

// ---------------------------------------------------------------- fetch

#[derive(Default, Debug)]
struct FetchStats { encoded_bytes: u64, encoded_chunks: u64, raw_bytes: u64, fallback_bytes: u64 }

/// Stream a transport file into the package cache. Header, manifest and chunk
/// table are written as normal data; every zstd chunk goes to disk unchanged as
/// a btrfs compressed extent at image_base + image_off.
fn fetch(input: &str, cache: &Path, use_encoded: bool) -> Result<()> {
    let mut rd: Box<dyn Read> = if input == "-" { Box::new(io::stdin().lock()) } else { Box::new(File::open(input)?) };
    let mut rd = BufReader::with_capacity(1 << 20, &mut rd);
    let mut hash = Sha256::new();
    let mut read = |n: usize, rd: &mut dyn Read| -> Result<Vec<u8>> {
        let mut b = vec![0u8; n];
        rd.read_exact(&mut b).context("truncated package")?;
        hash.update(&b);
        Ok(b)
    };
    let hb = read(HDR, &mut rd)?;
    let mut hdr = Header::decode(&hb)?;
    ensure!(hdr.flags == 0, "input is not a transport package");
    let meta = read(hdr.meta_len as usize, &mut rd)?;
    let table_b = read(hdr.table_len() as usize, &mut rd)?;
    let chunks = decode_table(&table_b, hdr.image_len)?;

    let tmp = cache.with_extension("part");
    let _ = fs::remove_file(&tmp);
    let out = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
    // Only raw (incompressible) chunks are written as plain data; don't let the
    // kernel try to compress them. Without encoded writes the kernel must compress.
    if use_encoded { set_nocomp(&out); }
    let prefix = (HDR + meta.len() + table_b.len() + 32) as u64;
    hdr.image_base = align(prefix);
    hdr.flags = FLAG_MATERIALIZED;

    let mut st = FetchStats::default();
    let mut encoded_ok = use_encoded;
    let mut dctx = zstd::bulk::Decompressor::new()?;
    for c in &chunks {
        let data = read(c.encoded_len as usize, &mut rd)?;
        let off = hdr.image_base + c.image_off;
        if c.compression == COMP_NONE {
            out.write_all_at(&data, off)?;
            st.raw_bytes += data.len() as u64;
            continue;
        }
        if encoded_ok {
            match encoded_write(&out, off, c.unencoded_len as u64, &data) {
                Ok(()) => { st.encoded_bytes += data.len() as u64; st.encoded_chunks += 1; continue; }
                Err(e) if st.encoded_chunks == 0 => {
                    eprintln!("btrpkg: encoded write unavailable ({e}); decompressing instead");
                    encoded_ok = false;
                }
                Err(e) => return Err(e).context("encoded write"),
            }
        }
        let plain = dctx.decompress(&data, c.unencoded_len as usize)?;
        ensure!(plain.len() == c.unencoded_len as usize, "chunk decompressed to wrong size");
        out.write_all_at(&plain, off)?;
        st.fallback_bytes += plain.len() as u64;
    }
    let mut want = [0u8; 32];
    rd.read_exact(&mut want).context("missing digest")?;
    let got = hash.finalize();
    if got.as_slice() != want {
        drop(out);
        let _ = fs::remove_file(&tmp);
        bail!("digest mismatch: package is corrupt");
    }
    // Header, manifest, table and digest go in front of the image.
    let mut pre = hdr.encode().to_vec();
    pre.extend_from_slice(&meta);
    pre.extend_from_slice(&table_b);
    pre.extend_from_slice(&want);
    out.write_all_at(&pre, 0)?;
    out.set_len(hdr.image_base + hdr.image_len)?;
    drop(out);
    fs::rename(&tmp, cache)?;
    eprintln!("fetch: {:?}", st);
    Ok(())
}

// ---------------------------------------------------------------- install

struct Pkg { hdr: Header, entries: Vec<Entry>, small: Vec<u8>, file: File }

fn open_pkg(p: &Path) -> Result<Pkg> {
    let file = File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut hb = [0u8; HDR];
    file.read_exact_at(&mut hb, 0)?;
    let hdr = Header::decode(&hb)?;
    let mut meta = vec![0u8; hdr.meta_len as usize];
    file.read_exact_at(&mut meta, HDR as u64)?;
    let raw = zstd::bulk::decompress(&meta, hdr.meta_raw_len as usize)?;
    let (entries, small) = decode_meta(&raw)?;
    Ok(Pkg { hdr, entries, small, file })
}

#[derive(Default, Debug)]
struct InstallStats { files: u64, cloned_files: u64, cloned_bytes: u64, small_files: u64, small_bytes: u64, copied_bytes: u64, dirs: u64, symlinks: u64, hardlinks: u64 }

fn cpath(p: &Path) -> CString { CString::new(p.as_os_str().as_bytes()).unwrap() }

fn set_attrs(path: &Path, e: &Entry, link: bool) -> Result<()> {
    let c = cpath(path);
    unsafe {
        if libc::lchown(c.as_ptr(), e.uid, e.gid) != 0 { return Err(io::Error::last_os_error()).context("chown"); }
        if !link && libc::chmod(c.as_ptr(), e.mode & 0o7777) != 0 { return Err(io::Error::last_os_error()).context("chmod"); }
        // xattrs (e.g. security.capability) after chown, which clears them
        for (k, v) in &e.xattrs {
            let kc = CString::new(k.clone())?;
            if libc::lsetxattr(c.as_ptr(), kc.as_ptr(), v.as_ptr() as *const _, v.len(), 0) != 0 {
                return Err(io::Error::last_os_error()).context("setxattr");
            }
        }
        let ts = [libc::timespec { tv_sec: e.mtime, tv_nsec: 0 }, libc::timespec { tv_sec: e.mtime, tv_nsec: 0 }];
        if libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) != 0 {
            return Err(io::Error::last_os_error()).context("utimensat");
        }
    }
    Ok(())
}

fn install(cache: &Path, root: &Path, force_copy: bool) -> Result<()> {
    let pkg = open_pkg(cache)?;
    ensure!(pkg.hdr.flags & FLAG_MATERIALIZED != 0, "not a fetched package (run btrpkg fetch first)");
    let mut st = InstallStats::default();
    let mut dirs = Vec::new();
    let mut can_clone = !force_copy;
    for e in &pkg.entries {
        // pacman's metadata files live in the package root and are not installed
        if !e.path.contains(&b'/') && e.path.starts_with(b".") { continue; }
        let dst = root.join(std::ffi::OsStr::from_bytes(&e.path));
        let tmp = { let mut t = dst.clone().into_os_string(); t.push(".btrpkg-new"); PathBuf::from(t) };
        if let Some(parent) = dst.parent() { fs::create_dir_all(parent)?; }
        match e.kind {
            K_DIR => {
                match fs::create_dir(&dst) { Ok(()) => {}, Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}, Err(err) => return Err(err.into()) }
                dirs.push((dst, e));
                st.dirs += 1;
            }
            K_SYMLINK => {
                let _ = fs::remove_file(&tmp);
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&e.target), &tmp)?;
                set_attrs(&tmp, e, true)?;
                fs::rename(&tmp, &dst)?;
                st.symlinks += 1;
            }
            K_HARDLINK => {
                let _ = fs::remove_file(&tmp);
                fs::hard_link(root.join(std::ffi::OsStr::from_bytes(&e.target)), &tmp)?;
                fs::rename(&tmp, &dst)?;
                st.hardlinks += 1;
            }
            K_FILE => {
                let _ = fs::remove_file(&tmp);
                let f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)
                    .with_context(|| format!("create {}", tmp.display()))?;
                if e.small {
                    let s = e.data_off as usize;
                    ensure!(s + e.size as usize <= pkg.small.len(), "small file out of range");
                    (&f).write_all(&pkg.small[s..s + e.size as usize])?;
                    st.small_files += 1;
                    st.small_bytes += e.size;
                } else {
                    let src = pkg.hdr.image_base + e.data_off;
                    let alen = align(e.size);
                    ensure!(e.data_off + alen <= pkg.hdr.image_len, "file out of image");
                    let mut done = false;
                    if can_clone {
                        match clone_range(&pkg.file, src, alen, &f) {
                            Ok(()) => { f.set_len(e.size)?; done = true; st.cloned_files += 1; st.cloned_bytes += e.size; }
                            Err(err) => { eprintln!("btrpkg: reflink unavailable ({err}); copying"); can_clone = false; }
                        }
                    }
                    if !done {
                        let mut off = 0u64;
                        while off < e.size {
                            let n = unsafe { libc::copy_file_range(pkg.file.as_raw_fd(), &mut ((src + off) as i64) as *mut i64,
                                f.as_raw_fd(), &mut (off as i64) as *mut i64, (e.size - off) as usize, 0) };
                            ensure!(n > 0, "copy_file_range: {}", io::Error::last_os_error());
                            off += n as u64;
                        }
                        st.copied_bytes += e.size;
                    }
                }
                drop(f);
                set_attrs(&tmp, e, false)?;
                fs::rename(&tmp, &dst)?;
                st.files += 1;
            }
            _ => bail!("bad entry kind"),
        }
    }
    // directory attributes last, deepest first, so file creation doesn't bump mtimes
    for (d, e) in dirs.iter().rev() { set_attrs(d, e, false)?; }
    eprintln!("install: {:?}", st);
    Ok(())
}

fn info(p: &Path) -> Result<()> {
    let pkg = open_pkg(p)?;
    let mut tb = vec![0u8; pkg.hdr.table_len() as usize];
    pkg.file.read_exact_at(&mut tb, HDR as u64 + pkg.hdr.meta_len)?;
    let chunks = decode_table(&tb, pkg.hdr.image_len)?;
    let enc: u64 = chunks.iter().map(|c| c.encoded_len as u64).sum();
    let unenc: u64 = chunks.iter().map(|c| c.unencoded_len as u64).sum();
    let mut kinds: HashMap<u8, u64> = HashMap::new();
    for e in &pkg.entries { *kinds.entry(e.kind).or_default() += 1; }
    println!("{:?}\nentries={} files={} dirs={} symlinks={} hardlinks={} small_bytes={}\nchunks={} zstd={} raw={} encoded_bytes={} image_bytes={}",
        pkg.hdr, pkg.entries.len(), kinds.get(&K_FILE).unwrap_or(&0), kinds.get(&K_DIR).unwrap_or(&0),
        kinds.get(&K_SYMLINK).unwrap_or(&0), kinds.get(&K_HARDLINK).unwrap_or(&0), pkg.small.len(),
        chunks.len(), chunks.iter().filter(|c| c.compression == COMP_ZSTD).count(),
        chunks.iter().filter(|c| c.compression == COMP_NONE).count(), enc, unenc);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut flags: Vec<&str> = vec![];
    let mut pos: Vec<&str> = vec![];
    let mut level = 19;
    let mut small_max = 4096u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-l" => level = it.next().context("-l needs a value")?.parse()?,
            "--small" => small_max = it.next().context("--small needs a value")?.parse()?,
            s if s.starts_with("--") => flags.push(s),
            s => pos.push(s),
        }
    }
    match pos.as_slice() {
        ["convert", i, o] => convert(Path::new(i), Path::new(o), level, small_max),
        ["fetch", i, c] => fetch(i, Path::new(c), !flags.contains(&"--no-encoded")),
        ["install", c, r] => install(Path::new(c), Path::new(r), flags.contains(&"--copy")),
        ["info", p] => info(Path::new(p)),
        _ => bail!("usage: btrpkg convert [-l N] [--small N] IN OUT | fetch [--no-encoded] IN CACHE | install [--copy] CACHE ROOT | info FILE"),
    }
}
