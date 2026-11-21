//! btrpkg spike: pacman packages whose file data is shipped as btrfs zstd extents.
//!
//!   btrpkg convert [-l LEVEL] IN.pkg.tar.zst OUT.pkg.btr
//!   btrpkg fetch [--no-encoded] IN.pkg.btr|- CACHEFILE     (needs CAP_SYS_ADMIN for encoded writes)
//!   btrpkg install [--copy] CACHEFILE ROOT
//!   btrpkg info FILE
//!   btrpkg publish IN.pkg.btr STORE            (content-addressed chunk store, see store.rs)
//!   btrpkg pull [--no-encoded] [--sha256 HEX] STORE NAME CACHEFILE
//!
//! The format is described in format.rs (version 1) and README.md.

use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

mod format;
mod cache;
mod fetchids;
mod fsx;
mod net;
mod repo;
mod sign;
mod store;
mod upgrade;

use format::*;

// ---------------------------------------------------------------- convert

/// Writes the .pkg.btr stream as `n` independent zstd frames of about
/// total/n bytes each (at least `min_frame`), so fetch can decode them in
/// parallel. Each frame's window is sized to the frame (capped at 2^27, as
/// --long=27), and its content size is in the header.
struct FrameWriter {
    out: Option<io::BufWriter<File>>,
    enc: Option<zstd::stream::write::Encoder<'static, io::BufWriter<File>>>,
    frame_size: u64,
    remaining: u64,
    left_in_frame: u64,
    level: i32,
    frames: u32,
}
impl FrameWriter {
    fn new(out: io::BufWriter<File>, total: u64, n: u64, min_frame: u64, level: i32) -> FrameWriter {
        let frame_size = total.div_ceil(n.max(1)).max(min_frame).min(total.max(1));
        FrameWriter { out: Some(out), enc: None, frame_size, remaining: total, left_in_frame: 0, level, frames: 0 }
    }
    fn start(&mut self) -> io::Result<()> {
        let len = self.frame_size.min(self.remaining);
        let wl = (64 - (len.max(1) - 1).leading_zeros()).clamp(10, 27);
        let mut z = zstd::stream::write::Encoder::new(self.out.take().unwrap(), self.level)?;
        z.set_pledged_src_size(Some(len))?;
        z.include_contentsize(true)?;
        z.include_checksum(true)?;
        z.window_log(wl)?;
        if wl >= 23 { z.long_distance_matching(true)?; }
        z.multithread(std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1))?;
        self.enc = Some(z);
        self.left_in_frame = len;
        self.frames += 1;
        Ok(())
    }
    fn finish(mut self) -> Result<u32> {
        ensure!(self.remaining == 0 && self.enc.is_none(), "frame writer: {} bytes short", self.remaining);
        self.out.take().unwrap().into_inner()?;
        Ok(self.frames)
    }
}
impl Write for FrameWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.is_empty() { return Ok(0); }
        if self.enc.is_none() {
            if self.remaining == 0 { return Err(io::Error::other("write past announced size")); }
            self.start()?;
        }
        let n = (b.len() as u64).min(self.left_in_frame) as usize;
        self.enc.as_mut().unwrap().write_all(&b[..n])?;
        self.left_in_frame -= n as u64;
        self.remaining -= n as u64;
        if self.left_in_frame == 0 {
            self.out = Some(self.enc.take().unwrap().finish()?);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

enum Out { Plain(io::BufWriter<File>), Zst(FrameWriter) }
impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self { Out::Plain(w) => w.write(b), Out::Zst(w) => w.write(b) }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self { Out::Plain(w) => w.flush(), Out::Zst(w) => w.flush() }
    }
}

/// One btrfs-readable zstd frame for a 4 KiB-aligned chunk of at most 128 KiB
/// (window <= 128 KiB, no checksum, no dictionary ID). Stays raw unless the
/// frame saves at least one 4 KiB block on disk.
fn frame(level: i32, data: Vec<u8>) -> Result<(u8, Vec<u8>)> {
    let mut c = zstd::bulk::Compressor::new(level)?;
    c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(17))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(false))?;
    c.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
    let z = c.compress(&data)?;
    if align(z.len() as u64) < data.len() as u64 { Ok((COMP_ZSTD, z)) } else { Ok((COMP_NONE, data)) }
}

fn convert(input: &Path, output: &Path, level: i32, outer_level: i32, small_max: u64, nframes: u64, min_frame: u64) -> Result<()> {
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
                let key = sha256(&data);
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
    // (window <= 128 KiB, no checksum, no dictionary). Level 0 keeps the chunks
    // raw, for .pkg.btr.zst where the outer zstd compresses and fetch frames.
    let compressed: Vec<(Chunk, Vec<u8>)> = jobs
        .into_par_iter()
        .map(|(off, data)| -> Result<(Chunk, Vec<u8>)> {
            let unencoded_len = data.len() as u32;
            let raw = sha256(&data);
            let (comp, bytes) = if level == 0 { (COMP_NONE, data) } else { frame(level, data)? };
            let stored = if comp == COMP_NONE { raw } else { sha256(&bytes) };
            Ok((Chunk { image_off: off, unencoded_len, encoded_len: bytes.len() as u32, compression: comp, stored, raw }, bytes))
        })
        .collect::<Result<_>>()?;

    let chunks: Vec<Chunk> = compressed.iter().map(|(c, _)| *c).collect();
    let index = build_index(&entries, &small, &chunks, cursor)?;
    let tmp = { let mut t = output.as_os_str().to_owned(); t.push(".part"); PathBuf::from(t) };
    // .pkg.btr.zst: the whole .pkg.btr wrapped in one solid zstd stream (long window).
    let outer = output.extension().is_some_and(|e| e == "zst");
    let file = File::create(&tmp)?;
    let total = index.len() as u64 + compressed.iter().map(|(_, d)| d.len() as u64).sum::<u64>();
    let mut out: Out = if outer {
        Out::Zst(FrameWriter::new(io::BufWriter::new(file.try_clone()?), total, nframes, min_frame, outer_level))
    } else {
        Out::Plain(io::BufWriter::new(file.try_clone()?))
    };
    out.write_all(&index)?;
    for (_, d) in &compressed {
        out.write_all(d)?;
    }
    match out {
        Out::Plain(w) => { w.into_inner()?; }
        Out::Zst(z) => { z.finish()?; }
    }
    file.sync_all()?;
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
    clone_range_at(src, src_off, len, dst, 0)
}

fn clone_range_at(src: &File, src_off: u64, len: u64, dst: &File, dst_off: u64) -> io::Result<()> {
    let a = FileCloneRange { src_fd: src.as_raw_fd() as i64, src_offset: src_off, src_length: len, dest_offset: dst_off };
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
#[allow(dead_code)]
pub(crate) struct FetchStats { outer: bool, framed_here: bool, file_sha256: String, encoded_bytes: u64, encoded_chunks: u64, raw_bytes: u64, fallback_bytes: u64 }

/// Hashes everything read through it: the bytes of the file as downloaded.
struct HashReader { inner: Box<dyn Read + Send>, h: std::sync::Arc<std::sync::Mutex<sha2::Sha256>> }
impl Read for HashReader {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(b)?;
        sha2::Digest::update(&mut *self.h.lock().unwrap(), &b[..n]);
        Ok(n)
    }
}

/// The outer stream of a .pkg.btr.zst is written with --long=27; nothing larger is accepted.
const OUTER_WINDOW_LOG: u32 = 27;
/// Largest outer frame decoded into memory (the spike holds frames in memory).
const MAX_OUTER_FRAME: u64 = 8 << 30;

/// Decode a .pkg.btr.zst: find the frame boundaries (block headers only, no
/// decoding), then decode all frames in parallel. Spike limitation: the whole
/// download and the whole inner stream are held in memory, and decoding starts
/// once the download is complete.
fn decode_frames(mut src: impl Read) -> Result<Box<dyn Read + Send>> {
    let mut all = Vec::new();
    src.read_to_end(&mut all)?;
    // One frame: stream it, so decoding overlaps with framing and writing.
    if zstd::zstd_safe::find_frame_compressed_size(&all).is_ok_and(|n| n == all.len()) {
        let mut d = zstd::stream::read::Decoder::with_buffer(io::Cursor::new(all))?;
        d.window_log_max(OUTER_WINDOW_LOG)?;
        return Ok(Box::new(d));
    }
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos < all.len() {
        let n = zstd::zstd_safe::find_frame_compressed_size(&all[pos..])
            .map_err(|e| anyhow::anyhow!("truncated package: bad zstd frame at {pos}: {}", zstd::zstd_safe::get_error_name(e)))?;
        frames.push(pos..pos + n);
        pos += n;
    }
    let bufs = frames
        .par_iter()
        .map(|r| -> Result<Vec<u8>> {
            let f = &all[r.clone()];
            match zstd::zstd_safe::get_frame_content_size(f) {
                Ok(Some(size)) => {
                    ensure!(size <= MAX_OUTER_FRAME, "zstd frame too large");
                    let mut d = zstd::bulk::Decompressor::new()?;
                    d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(OUTER_WINDOW_LOG))?;
                    let out = d.decompress(f, size as usize).context("truncated package: zstd frame")?;
                    ensure!(out.len() as u64 == size, "zstd frame decoded to the wrong size");
                    Ok(out)
                }
                _ => {
                    // no content size (older single-frame files): stream it
                    let mut d = zstd::stream::read::Decoder::new(f)?;
                    d.window_log_max(OUTER_WINDOW_LOG)?;
                    let mut out = Vec::new();
                    (&mut d).take(MAX_OUTER_FRAME + 1).read_to_end(&mut out).context("truncated package: zstd frame")?;
                    ensure!(out.len() as u64 <= MAX_OUTER_FRAME, "zstd frame too large");
                    Ok(out)
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(BufList { bufs: bufs.into(), pos: 0 }))
}

struct BufList { bufs: std::collections::VecDeque<Vec<u8>>, pos: usize }
impl Read for BufList {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while let Some(b) = self.bufs.front() {
            if self.pos < b.len() {
                let n = out.len().min(b.len() - self.pos);
                out[..n].copy_from_slice(&b[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            self.bufs.pop_front();
            self.pos = 0;
        }
        Ok(0)
    }
}

/// Writes one checked chunk into a cache file at `off`: zstd frames as encoded extents (or
/// decompressed when encoded writes are unavailable), raw chunks as plain data.
struct ChunkWriter { encoded_ok: bool, encoded_chunks: u64, encoded_bytes: u64, raw_chunks: u64, raw_bytes: u64, fallback_chunks: u64, fallback_bytes: u64, dctx: zstd::bulk::Decompressor<'static> }
impl ChunkWriter {
    fn new(use_encoded: bool) -> Result<ChunkWriter> {
        Ok(ChunkWriter { encoded_ok: use_encoded, encoded_chunks: 0, encoded_bytes: 0, raw_chunks: 0, raw_bytes: 0, fallback_chunks: 0, fallback_bytes: 0, dctx: zstd::bulk::Decompressor::new()? })
    }
    fn write(&mut self, out: &File, off: u64, c: &Chunk, data: &[u8]) -> Result<()> {
        if c.compression == COMP_NONE {
            out.write_all_at(data, off)?;
            self.raw_chunks += 1;
            self.raw_bytes += data.len() as u64;
            return Ok(());
        }
        if self.encoded_ok {
            match encoded_write(out, off, c.unencoded_len as u64, data) {
                Ok(()) => { self.encoded_bytes += data.len() as u64; self.encoded_chunks += 1; return Ok(()); }
                Err(e) if self.encoded_chunks == 0 => {
                    eprintln!("btrpkg: encoded write unavailable ({e}); decompressing instead");
                    self.encoded_ok = false;
                }
                Err(e) => return Err(e).context("encoded write"),
            }
        }
        let plain = self.dctx.decompress(data, c.unencoded_len as usize)?;
        ensure!(plain.len() == c.unencoded_len as usize, "chunk decompressed to the wrong size");
        out.write_all_at(&plain, off)?;
        self.fallback_chunks += 1;
        self.fallback_bytes += plain.len() as u64;
        Ok(())
    }
}

/// Stream a transport file (.pkg.btr, or .pkg.btr.zst) into the package cache.
/// The index (header, chunk table, entries, blob) is read and checked first; then every chunk
/// is checked against its hash before it is written. The cache file is the index, padding,
/// then the image with each zstd chunk as a btrfs compressed extent. Raw chunks of a
/// .pkg.btr.zst are framed here (in parallel, at `level`) so they land compressed too.
/// `file_sha256`: expected sha256 of the file as downloaded (what pacman keeps in the repo
/// database); optional, the index and chunk hashes already cover the contents.
pub(crate) struct FetchOpts { pub use_encoded: bool, pub level: i32, pub verify_data: bool, pub fsync: bool }

#[allow(dead_code)]
pub(crate) fn fetch(input: &str, cache: &Path, use_encoded: bool, level: i32, file_sha256: Option<&str>, trust: &sign::Trust) -> Result<FetchStats> {
    fetch_with(input, cache, file_sha256, trust, &FetchOpts { use_encoded, level, verify_data: false, fsync: true })
}

pub(crate) fn fetch_with(input: &str, cache: &Path, file_sha256: Option<&str>, trust: &sign::Trust, o: &FetchOpts) -> Result<FetchStats> {
    let (use_encoded, level) = (o.use_encoded, o.level);
    let mut src: Box<dyn Read + Send> = if input == "-" { Box::new(io::stdin()) } else { Box::new(File::open(input)?) };
    let mut magic = [0u8; 4];
    src.read_exact(&mut magic).context("empty package")?;
    let file_hash = std::sync::Arc::new(std::sync::Mutex::new(<sha2::Sha256 as sha2::Digest>::new()));
    sha2::Digest::update(&mut *file_hash.lock().unwrap(), magic);
    let src: Box<dyn Read + Send> = Box::new(HashReader { inner: src, h: file_hash.clone() });
    let chained = io::Cursor::new(magic.to_vec()).chain(src);
    let outer = magic == [0x28, 0xb5, 0x2f, 0xfd];
    let src: Box<dyn Read + Send> = if outer { decode_frames(chained)? } else { Box::new(chained) };
    let mut rd = BufReader::with_capacity(1 << 20, src);
    // The header is checked (expected ID or signature) before anything else is read or written.
    let idx = read_index_checked(&mut rd, &|h| trust.check(h))?;
    let image_base = idx.hdr.image_base();

    let tmp = part_path(cache);
    let _ = fs::remove_file(&tmp);
    let out = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
    // Only raw (incompressible) chunks are written as plain data; don't let the
    // kernel try to compress them. Without encoded writes the kernel must compress.
    if use_encoded { set_nocomp(&out); }

    let mut st = FetchStats { outer, ..Default::default() };
    let mut w = ChunkWriter::new(use_encoded)?;
    const BATCH: usize = 16 << 20;
    type Batch = Vec<(usize, Chunk, Vec<u8>)>;
    let chunks = idx.chunks.clone();
    // Pipeline: a reader thread decompresses the outer zstd and reads chunks, this
    // thread checks them (and frames raw ones) in parallel and writes them.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Batch>(2);
    let result: Result<()> = std::thread::scope(|s| {
        let reader = s.spawn(move || -> Result<()> {
            let mut i = 0;
            while i < chunks.len() {
                let mut batch = Vec::new();
                let mut sz = 0;
                while i < chunks.len() && sz < BATCH {
                    let c = chunks[i];
                    let d = read_n(&mut rd, c.encoded_len as u64, "chunk data")?;
                    sz += d.len();
                    batch.push((i, c, d));
                    i += 1;
                }
                // Checked here, while the writer is busy with the previous batch.
                batch.par_iter().try_for_each(|(i, c, d)| if o.verify_data { check_chunk_full(*i, c, d) } else { check_chunk(*i, c, d) })?;
                if tx.send(batch).is_err() { bail!("writer stopped"); }
            }
            drop(tx);
            // Read to the end: finishes the outer frame (and checks its checksum),
            // and refuses trailing data.
            let mut rest = [0u8; 1];
            ensure!(rd.read(&mut rest).context("reading package end")? == 0, "trailing data after package");
            Ok(())
        });
        let mut write_all = || -> Result<()> {
            for batch in rx.iter() {
                // Raw chunks (from a .pkg.btr.zst) are framed here, in parallel, so they land
                // as compressed extents too. Already-framed .pkg.btr chunks go straight through.
                let frame_here = use_encoded && w.encoded_ok && level > 0;
                let batch: Vec<(Chunk, Vec<u8>)> = if frame_here && outer && batch.iter().any(|(_, c, _)| c.compression == COMP_NONE) {
                    st.framed_here = true;
                    batch.into_par_iter().map(|(_, c, d)| -> Result<(Chunk, Vec<u8>)> {
                        if c.compression != COMP_NONE { return Ok((c, d)); }
                        let (comp, b) = frame(level, d)?;
                        Ok((Chunk { compression: comp, encoded_len: b.len() as u32, ..c }, b))
                    }).collect::<Result<_>>()?
                } else {
                    batch.into_iter().map(|(_, c, d)| (c, d)).collect()
                };
                for (c, data) in batch {
                    w.write(&out, image_base + c.image_off, &c, &data)?;
                }
            }
            Ok(())
        };
        let r1 = write_all();
        drop(rx);
        let r2 = reader.join().map_err(|_| anyhow::anyhow!("reader thread panicked"))?;
        r1?;
        r2
    });
    let file_digest = hex(&sha2::Digest::finalize(file_hash.lock().unwrap().clone()));
    let result = result.and_then(|()| {
        ensure!(file_sha256.map_or(true, |e| e.eq_ignore_ascii_case(&file_digest)), "file sha256 mismatch: package is corrupt");
        // The index goes in front of the image, so the cache file describes itself.
        out.write_all_at(&idx.bytes, 0)?;
        out.set_len(idx.hdr.cache_len())?;
        if o.fsync { out.sync_all()?; }
        Ok(())
    });
    drop(out);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, cache)?;
    st.file_sha256 = file_digest;
    (st.encoded_bytes, st.encoded_chunks, st.raw_bytes, st.fallback_bytes) = (w.encoded_bytes, w.encoded_chunks, w.raw_bytes, w.fallback_bytes);
    Ok(st)
}

/// The temporary name a file is written under before it is renamed into place.
pub(crate) fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Open a cache file made by `fetch` or `pull`: the index is checked, and the file must
/// have exactly the length the index says.
pub(crate) fn open_cache(p: &Path) -> Result<(Index, File)> {
    let file = File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut hb = [0u8; HDR];
    file.read_exact_at(&mut hb, 0).context("truncated header")?;
    let hdr = Header::decode(&hb)?;
    let len = file.metadata()?.len();
    ensure!(len == hdr.cache_len(), "{} is not a btrpkg cache file (fetch or pull it first)", p.display());
    let mut b = vec![0u8; hdr.index_len() as usize];
    file.read_exact_at(&mut b, 0)?;
    Ok((parse_index(&b)?, file))
}

// ---------------------------------------------------------------- install

#[derive(Default, Debug)]
pub(crate) struct InstallStats { files: u64, cloned_files: u64, cloned_bytes: u64, small_files: u64, small_bytes: u64, copied_bytes: u64, dirs: u64, symlinks: u64, hardlinks: u64 }

/// Place a package's entries under ROOT from a cache file (see fsx.rs for how paths are resolved:
/// never through a symlink, never outside ROOT).
pub(crate) fn install(cache: &Path, root: &Path, force_copy: bool) -> Result<InstallStats> {
    let (idx, file) = open_cache(cache)?;
    let image_base = idx.hdr.image_base();
    let r = fsx::Root::open(root)?;
    let mut st = InstallStats::default();
    let mut dirs = Vec::new();
    let mut can_clone = !force_copy;
    for e in &idx.entries {
        // pacman's metadata files live in the package root and are not installed
        if !e.path.contains(&b'/') && e.path.starts_with(b".") { continue; }
        let path = e.path.as_slice();
        match e.kind {
            K_DIR => {
                r.mkdir(path)?;
                dirs.push(e);
                st.dirs += 1;
            }
            K_SYMLINK => {
                r.place(path, |pfd, tmp| {
                    let t = fsx::c(&e.target)?;
                    fsx::check_rc(unsafe { libc::symlinkat(t.as_ptr(), pfd, tmp.as_ptr()) }, "symlink", path)?;
                    fsx::set_attrs_link(pfd, tmp, e.uid, e.gid, &e.xattrs, e.mtime, path)
                })?;
                st.symlinks += 1;
            }
            K_HARDLINK => {
                // check_entries: the target is an earlier file of this package
                let (sfd, sname) = r.existing(&e.target)?;
                r.place(path, |pfd, tmp| {
                    fsx::check_rc(unsafe { libc::linkat(sfd.as_raw_fd(), sname.as_ptr(), pfd, tmp.as_ptr(), 0) }, "link", path)
                })?;
                st.hardlinks += 1;
            }
            K_FILE => {
                r.place(path, |pfd, tmp| {
                    let fd = unsafe { libc::openat(pfd, tmp.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600) };
                    fsx::check_rc(fd, "create", path)?;
                    let f = unsafe { File::from_raw_fd(fd) };
                    if e.small {
                        let s = e.data_off as usize;
                        (&f).write_all(&idx.small[s..s + e.size as usize])?;
                        st.small_files += 1;
                        st.small_bytes += e.size;
                    } else {
                        // check_entries made sure the range lies inside the image
                        let src = image_base + e.data_off;
                        let mut done = false;
                        if can_clone {
                            match clone_range(&file, src, align(e.size), &f) {
                                Ok(()) => { f.set_len(e.size)?; done = true; st.cloned_files += 1; st.cloned_bytes += e.size; }
                                Err(err) => { eprintln!("btrpkg: reflink unavailable ({err}); copying"); can_clone = false; }
                            }
                        }
                        if !done {
                            let mut off = 0u64;
                            while off < e.size {
                                let n = unsafe { libc::copy_file_range(file.as_raw_fd(), &mut ((src + off) as i64) as *mut i64,
                                    f.as_raw_fd(), &mut (off as i64) as *mut i64, (e.size - off) as usize, 0) };
                                ensure!(n > 0, "copy_file_range: {}", io::Error::last_os_error());
                                off += n as u64;
                            }
                            st.copied_bytes += e.size;
                        }
                    }
                    fsx::set_attrs_fd(f.as_raw_fd(), e.uid, e.gid, e.mode, &e.xattrs, e.mtime, path)
                })?;
                st.files += 1;
            }
            _ => bail!("bad entry kind"),
        }
    }
    // directory attributes last, deepest first, so file creation doesn't bump mtimes
    for e in dirs.iter().rev() {
        let fd = r.open_dir(&e.path)?;
        fsx::set_attrs_fd(fd.as_raw_fd(), e.uid, e.gid, e.mode, &e.xattrs, e.mtime, &e.path)?;
    }
    Ok(st)
}

/// Describe an index, transport or cache file (told apart by their length).
fn info(p: &Path) -> Result<()> {
    let len = fs::metadata(p)?.len();
    let mut f = BufReader::new(File::open(p)?);
    let idx = read_index(&mut f)?;
    let h = &idx.hdr;
    let form = if len == h.index_len() { "index" } else if len == h.transport_len(&idx.chunks) { "transport" }
        else if len == h.cache_len() { "cache" } else { "unknown (wrong length)" };
    let enc: u64 = idx.chunks.iter().map(|c| c.encoded_len as u64).sum();
    let mut kinds: HashMap<u8, u64> = HashMap::new();
    for e in &idx.entries { *kinds.entry(e.kind).or_default() += 1; }
    println!("form={form} version={VERSION} id={}\nindex_bytes={} (header {HDR}, table {}, entries {} -> {}, blob {} -> {})",
        hex(&h.id()), h.index_len(), h.table_len(), h.entries_len, h.entries_raw_len, h.blob_len, h.blob_raw_len);
    println!("entries={} files={} dirs={} symlinks={} hardlinks={} small_bytes={}\nchunks={} zstd={} raw={} encoded_bytes={} image_bytes={}",
        idx.entries.len(), kinds.get(&K_FILE).unwrap_or(&0), kinds.get(&K_DIR).unwrap_or(&0),
        kinds.get(&K_SYMLINK).unwrap_or(&0), kinds.get(&K_HARDLINK).unwrap_or(&0), idx.small.len(),
        idx.chunks.len(), idx.chunks.iter().filter(|c| c.compression == COMP_ZSTD).count(),
        idx.chunks.iter().filter(|c| c.compression == COMP_NONE).count(), enc, h.image_len);
    Ok(())
}

const USAGE: &str = "usage:
  btrpkg convert [-l N] [--outer-level N] [--frames N] [--min-frame BYTES] [--small N] IN.pkg.tar.zst OUT(.pkg.btr|.pkg.btr.zst)
  btrpkg fetch [TRUST] [--no-encoded] [--verify-data] [--no-fsync] [-l N] [--sha256 HEX] IN|- CACHE
  btrpkg install [--copy] CACHE ROOT
  btrpkg info FILE
  btrpkg dump FILE                        (the parsed index as text, to compare with other readers)
  btrpkg keygen NAME                      (writes NAME.sec and NAME.pub)
  btrpkg sign --key NAME.sec FILE         (writes FILE.sig over FILE's index header)
  btrpkg publish [--key NAME.sec] IN.pkg.btr STORE
  btrpkg pull [TRUST] [--no-encoded] [--verify-data] [--no-fsync] [--h2c] [--parallel N] [--installed ROOT] [-l N] DIR|URL NAME CACHE
  btrpkg pull-ids [--h2c] [--parallel N] [--installed ROOT] [-l N] DIR|URL CACHEDIR NAME=IDHEX...
      (for package managers: every NAME's index must have the header ID IDHEX; all packages share
       one download queue; writes CACHEDIR/NAME.pkg.btr, and NAME.pkg.btr.sig when the server
       has a signature; one syncfs at the end)
  btrpkg fetch-ids [--landlock] [--progress] [--installed ROOT] [--cachedir DIR] [--want FILE] --server URL... DIR NAME=IDHEX...
      (the unprivileged, network half of pull-ids: indexes and the chunks that can't come from
       the machine into the directory store DIR; servers tried in order; --landlock: write only
       to DIR, run nothing. The other half: pull-ids [--missing-to FILE] [--consume] ... DIR CACHEDIR ...,
       which exits 3 and lists the chunks DIR lacks if a local file didn't check out, and with
       --consume removes DIR after success. For package managers: fetch-ids --index-only, then
       pull-ids --fetch-via FILE, which hashes and reuses local data, lists exactly the missing
       chunks in FILE, prints fetch N BYTES and waits for go on stdin while the caller runs
       fetch-ids --want FILE. pull-ids --plan checks local data the same way, writes nothing and
       prints need NAME CHUNK_BYTES INDEX_BYTES per package: the download size)
  btrpkg cache-clean [--keep N] [--dry-run] CACHEDIR
      (keep the newest N versions (default 3) of every package's .pkg.btr, like paccache -rk N)
  btrpkg upgrade [--config FILE] [--root DIR] [--keys DIR]... [--no-snapshot] [--dry-run] [--list] [--h2c] [PKG...]
      (the same as: pacbtr [options] [PKG...])
  btrpkg repo-update [--key NAME.sec] [--gpg-keyring FILE | --no-gpg] [--drop-missing] [--db-in FILE] [--jobs N] [-l N] [--small N] DIR REPO
NET (pull, pull-ids, upgrade): --http h1|h2|h3 (h2 over http:// is h2c), --cacert FILE
TRUST: --id HEX (expected header ID) | --sig FILE [--keys DIR]... (default: IN.sig or the store's
       NAME.btridx.sig, keys from /etc/btrpkg/keys and /usr/share/btrpkg/keys) | --unverified";

/// `pull-ids`: what libalpm runs to download btrpkg packages. Trust comes from the caller (the
/// header IDs in its database); signatures are saved for the caller to check.
fn pull_ids(s: &str, cdir: &Path, pkgs: &[&str], flags: &[&str], parallel: Option<usize>, installed: Option<&Path>, level: Option<i32>, fetch_via: Option<&Path>) -> Result<()> {
    ensure!(!pkgs.is_empty(), "pull-ids needs at least one NAME=IDHEX");
    let src = net::Source::parse(s, flags.contains(&"--h2c"), parallel);
    let mut ns = net::NetStats::default();
    let mut got: Vec<(String, Vec<u8>, Option<String>, PathBuf, sign::Trust)> = Vec::new();
    for p in pkgs {
        let (name, id) = p.split_once('=').with_context(|| format!("{p}: want NAME=IDHEX"))?;
        ensure!(!name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\0']), "{name}: bad package file name");
        let (ib, sig) = store::fetch_index(&src, name, &mut ns)?;
        got.push((name.to_string(), ib, sig, cdir.join(format!("{name}.pkg.btr")), sign::Trust::Id(store::unhex(id)?)));
    }
    let o = store::PullOpts { use_encoded: !flags.contains(&"--no-encoded"), installed, level: level.unwrap_or(3),
        verify_data: flags.contains(&"--verify-data"), fsync: false, before_fetch: None, plan_only: flags.contains(&"--plan") };
    // --fetch-via FILE: after all local reuse, list the chunks still missing in FILE, say
    // "fetch N BYTES" on stdout and wait for "go" on stdin while the caller has another
    // (unprivileged) process put them into the directory source
    let hook = |items: &[(String, usize)]| -> Result<()> {
        if items.is_empty() { return Ok(()); }
        let p = fetch_via.unwrap();
        let body: String = items.iter().map(|(r, _)| format!("{r}\n")).collect();
        store::write_atomic_nosync(p, body.as_bytes())?;
        let bytes: usize = items.iter().map(|(_, l)| l).sum();
        let mut out = io::stdout().lock();
        writeln!(out, "fetch\t{}\t{bytes}", items.len())?;
        out.flush()?;
        drop(out);
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        ensure!(line.trim() == "go", "the chunk download did not complete");
        Ok(())
    };
    let o = store::PullOpts { before_fetch: if fetch_via.is_some() { Some(&hook) } else { None }, ..o };
    let items: Vec<store::PullItem> = got.iter().map(|(n, ib, _, c, t)| store::PullItem { name: n, ib, cache: c, trust: t }).collect();
    let st = store::pull_many(&src, &items, &o, ns)?;
    if o.plan_only {
        // what a real run would download: chunk bytes, then the index's size
        for ((n, ib, ..), st) in got.iter().zip(&st) { println!("need\t{n}\t{}\t{}", st.fetched_bytes, ib.len()); }
        return Ok(());
    }
    for ((n, _, sig, c, _), st) in got.iter().zip(&st) {
        let sp = PathBuf::from(format!("{}.sig", c.display()));
        match sig {
            Some(text) => {
                let tmp = PathBuf::from(format!("{}.part", sp.display()));
                fs::write(&tmp, text.as_bytes())?;
                fs::rename(&tmp, &sp)?;
            }
            None => { let _ = fs::remove_file(&sp); }
        }
        println!("download\t{n}\t{}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes, st.installed_chunks);
    }
    let d = File::open(cdir)?;
    ensure!(unsafe { libc::syncfs(d.as_raw_fd()) } == 0, "syncfs: {}", io::Error::last_os_error());
    Ok(())
}

/// The parsed and checked index of FILE (an index, transport or cache file) as text: one line per
/// chunk and entry, numbers in decimal, byte strings in hex. libalpm's reader prints the same.
fn dump(p: &Path) -> Result<()> {
    let idx = read_index(&mut io::BufReader::new(File::open(p).with_context(|| format!("open {}", p.display()))?))?;
    let h = &idx.hdr;
    let mut o = String::new();
    use std::fmt::Write as _;
    writeln!(o, "id {}", hex(&h.id()))?;
    writeln!(o, "header {} {} {} {} {} {} index {} base {}", h.nchunks, h.image_len, h.entries_len, h.entries_raw_len, h.blob_len, h.blob_raw_len, h.index_len(), h.image_base())?;
    for c in &idx.chunks {
        writeln!(o, "c {} {} {} {} {} {}", c.image_off, c.unencoded_len, c.encoded_len, c.compression, hex(&c.stored), hex(&c.raw))?;
    }
    for e in &idx.entries {
        write!(o, "e {} {} {} {} {} {} {} {} {} {} {}", e.kind, e.mode, e.uid, e.gid, e.mtime, e.size, e.data_off, e.small as u8, hex(&e.path), hex(&e.target), e.xattrs.len())?;
        for (k, v) in &e.xattrs { write!(o, " {}={}", hex(k), hex(v))?; }
        o.push('\n');
    }
    writeln!(o, "small {} {}", idx.small.len(), hex(&sha256(&idx.small)))?;
    io::stdout().write_all(o.as_bytes())?;
    Ok(())
}

/// How the index of a package is checked, from the command line.
struct TrustArgs { id: Option<String>, sig: Option<PathBuf>, keys: Vec<PathBuf>, unverified: bool }

impl TrustArgs {
    /// `default_sig`: the signature text found next to the input, if any.
    fn resolve(&self, default_sig: Option<String>) -> Result<sign::Trust> {
        if self.unverified { return Ok(sign::Trust::Unverified); }
        if let Some(id) = &self.id { return Ok(sign::Trust::Id(store::unhex(id)?)); }
        let sig = match &self.sig {
            Some(p) => fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?,
            None => default_sig.context("no signature found: pass --sig FILE, --id HEX or --unverified")?,
        };
        let dirs: Vec<PathBuf> = if self.keys.is_empty() { sign::DEFAULT_KEY_DIRS.iter().map(PathBuf::from).collect() } else { self.keys.clone() };
        let k = sign::Keyring::load(&dirs)?;
        ensure!(!k.is_empty(), "no public keys in {}", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", "));
        Ok(sign::Trust::Signed(k, sig))
    }
}

fn sec_key(p: Option<&PathBuf>) -> Result<Option<(sign::SecretKey, String)>> {
    let Some(p) = p else { return Ok(None) };
    let name = p.file_name().context("key")?.to_string_lossy().trim_end_matches(".sec").to_string();
    Ok(Some((sign::SecretKey::load(p)?, name)))
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `pacbtr ARGS` is `btrpkg upgrade ARGS`
    if std::env::args().next().is_some_and(|a| a.rsplit('/').next() == Some("pacbtr")) { args.insert(0, "upgrade".into()); }
    let mut conf: PathBuf = "/etc/pacman.conf".into();
    let mut root: PathBuf = "/".into();
    let mut flags: Vec<&str> = vec![];
    let mut pos: Vec<&str> = vec![];
    let mut level: Option<i32> = None;
    let mut outer_level = 19;
    let mut sha: Option<String> = None;
    let mut nframes = 1u64;
    let mut min_frame = 1u64 << 20;
    let mut small_max = 4096u64;
    let mut key: Option<PathBuf> = None;
    let mut gpg_keyring: Option<PathBuf> = Some("/etc/pacman.d/gnupg/pubring.gpg".into());
    let mut db_in: Option<PathBuf> = None;
    let mut jobs = 4usize;
    let mut parallel: Option<usize> = None;
    let mut installed: Option<PathBuf> = None;
    let mut netopt = net::NetOptions::default();
    let mut servers: Vec<String> = Vec::new();
    let mut cachedir: Option<PathBuf> = None;
    let mut want: Option<PathBuf> = None;
    let mut missing_to: Option<PathBuf> = None;
    let mut keep: usize = 3;
    let mut fetch_via: Option<PathBuf> = None;
    let mut trust = TrustArgs { id: None, sig: None, keys: vec![], unverified: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().with_context(|| format!("{n} needs a value")).cloned();
        match a.as_str() {
            "-l" | "--level" => level = Some(val("-l")?.parse()?),
            "--frames" => nframes = val("--frames")?.parse()?,
            "--min-frame" => min_frame = val("--min-frame")?.parse()?,
            "--sha256" => sha = Some(val("--sha256")?),
            "--outer-level" => outer_level = val("--outer-level")?.parse()?,
            "--small" => small_max = val("--small")?.parse()?,
            "--key" => key = Some(val("--key")?.into()),
            "--id" => trust.id = Some(val("--id")?),
            "--sig" => trust.sig = Some(val("--sig")?.into()),
            "--keys" => trust.keys.push(val("--keys")?.into()),
            "--unverified" => trust.unverified = true,
            "--gpg-keyring" => gpg_keyring = Some(val("--gpg-keyring")?.into()),
            "--db-in" => db_in = Some(val("--db-in")?.into()),
            "--no-gpg" => gpg_keyring = None,
            "--jobs" => jobs = val("--jobs")?.parse()?,
            "--parallel" => parallel = Some(val("--parallel")?.parse()?),
            "--installed" => installed = Some(val("--installed")?.into()),
            "--http" => netopt.version = Some(val("--http")?),
            "--server" => servers.push(val("--server")?),
            "--keep" => keep = val("--keep")?.parse()?,
            "--fetch-via" => fetch_via = Some(val("--fetch-via")?.into()),
            "--cachedir" => cachedir = Some(val("--cachedir")?.into()),
            "--want" => want = Some(val("--want")?.into()),
            "--missing-to" => missing_to = Some(val("--missing-to")?.into()),
            "--cacert" => netopt.cainfo = Some(val("--cacert")?),
            "--config" => conf = val("--config")?.into(),
            "--root" => root = val("--root")?.into(),
            "-h" | "--help" => { println!("{USAGE}"); return Ok(()); }
            "--no-encoded" | "--copy" | "--h2c" | "--drop-missing" | "--no-snapshot" | "--dry-run" | "--list" | "--verify-data" | "--no-fsync" | "--progress" | "--landlock" | "--consume" | "--index-only" | "--plan" => flags.push(a.as_str()),
            s if s.starts_with('-') && s.len() > 1 => bail!("unknown option {s}\n{USAGE}"),
            s => pos.push(s),
        }
    }
    net::set_options(netopt)?;
    match pos.as_slice() {
        // .pkg.btr: chunks framed at -l (default 19). .pkg.btr.zst: raw chunks (-l 0
        // by default) inside one solid zstd stream at --outer-level.
        ["convert", i, o] => {
            let outer = o.ends_with(".zst");
            convert(Path::new(i), Path::new(o), level.unwrap_or(if outer { 0 } else { 19 }), outer_level, small_max, nframes, min_frame)
        }
        ["fetch", i, c] => {
            let t = trust.resolve(if *i == "-" { None } else { fs::read_to_string(format!("{i}.sig")).ok() })?;
            let fo = FetchOpts { use_encoded: !flags.contains(&"--no-encoded"), level: level.unwrap_or(3), verify_data: flags.contains(&"--verify-data"), fsync: !flags.contains(&"--no-fsync") };
            fetch_with(i, Path::new(c), sha.as_deref(), &t, &fo).map(|st| eprintln!("fetch: {st:?}"))
        }
        ["install", c, r] => install(Path::new(c), Path::new(r), flags.contains(&"--copy")).map(|st| eprintln!("install: {st:?}")),
        ["info", p] => info(Path::new(p)),
        ["dump", p] => dump(Path::new(p)),
        ["keygen", n] => sign::keygen(Path::new(n)).map(|k| println!("key {} -> {n}.sec, {n}.pub", sign::hex8(&k))),
        ["sign", f] => {
            let (k, name) = sec_key(key.as_ref())?.context("sign needs --key NAME.sec")?;
            let mut hb = [0u8; HDR];
            File::open(f)?.read_exact(&mut hb).context("not a btrpkg file")?;
            Header::decode(&hb)?;
            let sp = format!("{f}.sig");
            fs::write(&sp, k.sign(&hb, &name))?;
            println!("{sp}");
            Ok(())
        }
        ["publish", i, s] => {
            let k = sec_key(key.as_ref())?;
            store::publish(Path::new(i), Path::new(s), k.as_ref().map(|(k, n)| (k, n.as_str()))).map(|(name, st)| {
                eprintln!("publish: {st:?}");
                println!("upload\t{name}\t{}\t{}\t{}\t{}", st.new_chunks, st.new_bytes, st.index_bytes, st.raw_entries);
            })
        }
        ["pull", s, n, c] => {
            let src = net::Source::parse(s, flags.contains(&"--h2c"), parallel);
            let mut ns = net::NetStats::default();
            let (ib, sig) = store::fetch_index(&src, n, &mut ns)?;
            let t = trust.resolve(sig)?;
            let o = store::PullOpts { use_encoded: !flags.contains(&"--no-encoded"), installed: installed.as_deref(), level: level.unwrap_or(3),
                verify_data: flags.contains(&"--verify-data"), fsync: !flags.contains(&"--no-fsync"), before_fetch: None, plan_only: false };
            store::pull_index(&src, n, &ib, Path::new(c), &t, &o, ns).map(|st| {
                eprintln!("pull: {st:?}");
                println!("download\t{n}\t{}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes, st.installed_chunks);
            })
        }
        ["pull-ids", s, cdir, pkgs @ ..] => match pull_ids(s, Path::new(cdir), pkgs, &flags, parallel, installed.as_deref(), level, fetch_via.as_deref()) {
            Err(e) if missing_to.is_some() && e.downcast_ref::<net::AbsentChunks>().is_some() => {
                let a = e.downcast_ref::<net::AbsentChunks>().unwrap();
                fs::write(missing_to.as_ref().unwrap(), a.0.join("\n") + "\n")?;
                eprintln!("btrpkg: {a}; listed in {}", missing_to.as_ref().unwrap().display());
                std::process::exit(3);
            }
            Ok(()) if flags.contains(&"--consume") => {
                // the download directory was only input; remove_dir_all doesn't follow symlinks
                let d = Path::new(s);
                ensure!(fs::symlink_metadata(d).is_ok_and(|m| m.is_dir()), "--consume needs a directory, not {s}");
                fs::remove_dir_all(d).with_context(|| format!("remove {s}"))
            }
            r => r,
        },
        ["cache-clean", d] => cache::cache_clean(Path::new(d), keep, flags.contains(&"--dry-run")).map(|st| eprintln!("cache-clean: {st:?}")),
        ["fetch-ids", dir, pkgs @ ..] => {
            let dir = Path::new(dir);
            if flags.contains(&"--landlock") { fetchids::landlock_writes_to(dir)?; }
            ensure!(!servers.is_empty(), "fetch-ids needs --server URL (repeat for more, tried in order)");
            let srcs: Vec<net::Source> = servers.iter().map(|s| net::Source::parse(s, flags.contains(&"--h2c"), parallel)).collect();
            let mut ids = Vec::new();
            for p in pkgs.iter() {
                let (name, id) = p.split_once('=').with_context(|| format!("{p}: want NAME=IDHEX"))?;
                ensure!(!name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\0']), "{name}: bad package file name");
                ids.push((name.to_string(), store::unhex(id)?));
            }
            let w = match &want { Some(p) => Some(fs::read_to_string(p)?.lines().filter(|l| !l.is_empty()).map(String::from).collect()), None => None };
            let o = fetchids::FetchOpts { installed: installed.as_deref(), cachedir: cachedir.as_deref(), want: w, progress: flags.contains(&"--progress"),
                index_only: flags.contains(&"--index-only") };
            fetchids::fetch_ids(&srcs, dir, &ids, &o)
        }
        ["repo-update", d, r] => {
            let k = sec_key(key.as_ref())?;
            let o = repo::RepoOpts { key: k.as_ref().map(|(k, n)| (k, n.as_str())), gpg_keyring, drop_missing: flags.contains(&"--drop-missing"),
                level: level.unwrap_or(19), small: small_max, jobs, db_in: db_in.clone() };
            let st = repo::repo_update(Path::new(d), r, &o)?;
            eprintln!("repo-update: {st:?}");
            ensure!(st.failed == 0, "{} packages failed (left to pacman)", st.failed);
            Ok(())
        }
        ["upgrade", pkgs @ ..] => {
            let o = upgrade::UpgradeOpts { conf, root, keys: trust.keys.clone(), snapshot: !flags.contains(&"--no-snapshot"), h2c: flags.contains(&"--h2c"),
                parallel, dry_run: flags.contains(&"--dry-run"), list_only: flags.contains(&"--list"), use_encoded: !flags.contains(&"--no-encoded"),
                fail_after: std::env::var("BTRPKG_FAIL_AFTER").ok().and_then(|v| v.parse().ok()) };
            let t0 = std::time::Instant::now();
            let st = upgrade::upgrade(&pkgs.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &o)?;
            eprintln!("upgrade: {st:?} in {:.2} s", t0.elapsed().as_secs_f64());
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicU32, Ordering};
    const U: sign::Trust = sign::Trust::Unverified;

    fn pull_dir(store: &Path, name: &str, cache: &Path, trust: &sign::Trust) -> Result<store::PullStats> {
        store::pull(&net::Source::Dir(store.to_path_buf()), name, cache, trust, &store::PullOpts { use_encoded: true, installed: None, level: 3, verify_data: false, fsync: true, before_fetch: None, plan_only: false })
    }

    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("btrpkg-test-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    /// Deterministic mix of incompressible and compressible bytes.
    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len).map(|i| {
            if (i / 50_000) % 2 == 0 { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 } else { (i % 251) as u8 }
        }).collect()
    }

    pub(crate) struct Spec { pub big: Vec<u8>, pub other: Vec<u8> }

    /// A package with every entry kind: dirs, small and large files, a duplicate, an empty
    /// file, a symlink, a hardlink, an xattr and pacman's root dotfiles.
    pub(crate) fn make_pkg(dir: &Path, name: &str, spec: &Spec) -> PathBuf {
        let (uid, gid) = unsafe { (libc::getuid() as u64, libc::getgid() as u64) };
        let p = dir.join(format!("{name}.pkg.tar.zst"));
        let z = zstd::stream::write::Encoder::new(File::create(&p).unwrap(), 3).unwrap().auto_finish();
        let mut b = tar::Builder::new(z);
        let add = |b: &mut tar::Builder<_>, path: &str, kind: tar::EntryType, mode: u32, body: &[u8], link: Option<&str>| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(mode);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_mtime(1_700_000_000);
            h.set_size(body.len() as u64);
            if let Some(l) = link { h.set_link_name(l).unwrap(); }
            h.set_path(path).unwrap();
            h.set_cksum();
            b.append(&h, body).unwrap();
        };
        add(&mut b, ".PKGINFO", tar::EntryType::Regular, 0o644, format!("pkgname = {name}\n").as_bytes(), None);
        add(&mut b, "usr", tar::EntryType::Directory, 0o755, b"", None);
        add(&mut b, "usr/bin", tar::EntryType::Directory, 0o755, b"", None);
        add(&mut b, "usr/bin/tool", tar::EntryType::Regular, 0o755, &spec.big, None);
        add(&mut b, "usr/bin/tool-copy", tar::EntryType::Regular, 0o755, &spec.big, None);
        add(&mut b, "usr/bin/hard", tar::EntryType::Link, 0o755, b"", Some("usr/bin/tool"));
        add(&mut b, "usr/lib", tar::EntryType::Directory, 0o755, b"", None);
        add(&mut b, "usr/lib/libx.so.1", tar::EntryType::Regular, 0o644, &spec.other, None);
        add(&mut b, "usr/lib/libx.so", tar::EntryType::Symlink, 0o777, b"", Some("libx.so.1"));
        add(&mut b, "usr/share", tar::EntryType::Directory, 0o755, b"", None);
        add(&mut b, "usr/share/readme", tar::EntryType::Regular, 0o644, b"read me\n", None);
        add(&mut b, "usr/share/empty", tar::EntryType::Regular, 0o600, b"", None);
        b.append_pax_extensions([("SCHILY.xattr.user.btrpkg", b"yes".as_slice())]).unwrap();
        add(&mut b, "usr/share/tagged", tar::EntryType::Regular, 0o644, b"tagged\n", None);
        b.into_inner().unwrap();
        p
    }

    fn spec(seed: u64) -> Spec { Spec { big: data(300_000, 7), other: data(200_000, seed) } }

    /// The installed tree matches the package (contents, modes, links).
    fn check_tree(root: &Path, s: &Spec) {
        let r = |p: &str| fs::read(root.join(p)).unwrap();
        assert_eq!(r("usr/bin/tool"), s.big);
        assert_eq!(r("usr/bin/tool-copy"), s.big);
        assert_eq!(r("usr/lib/libx.so.1"), s.other);
        assert_eq!(r("usr/share/readme"), b"read me\n");
        assert_eq!(r("usr/share/empty"), b"");
        assert!(!root.join(".PKGINFO").exists());
        assert_eq!(fs::read_link(root.join("usr/lib/libx.so")).unwrap(), Path::new("libx.so.1"));
        let m = |p: &str| fs::symlink_metadata(root.join(p)).unwrap();
        assert_eq!(m("usr/bin/hard").ino(), m("usr/bin/tool").ino());
        assert_eq!(m("usr/bin/tool").permissions().mode() & 0o7777, 0o755);
        assert_eq!(m("usr/share/empty").permissions().mode() & 0o7777, 0o600);
        assert_eq!(m("usr/bin/tool").mtime(), 1_700_000_000);
        assert_eq!(m("usr/share").mtime(), 1_700_000_000);
        let mut v = [0u8; 8];
        let n = unsafe { libc::lgetxattr(std::ffi::CString::new(root.join("usr/share/tagged").into_os_string().into_encoded_bytes()).unwrap().as_ptr(), c"user.btrpkg".as_ptr(), v.as_mut_ptr() as *mut _, 8) };
        if n >= 0 { assert_eq!(&v[..n as usize], b"yes"); } // tmpfs without user xattrs: skip
    }

    fn convert_to(src: &Path, out: &Path, level: i32) {
        let zst = out.extension().is_some_and(|e| e == "zst");
        convert(src, out, level, 19, 4096, if zst { 2 } else { 1 }, 4096).unwrap();
    }

    #[test]
    fn convert_fetch_install() {
        let t = TempDir::new("cfi");
        let s = spec(1);
        let src = make_pkg(&t.0, "p-1-1", &s);
        for (out, level) in [("p.pkg.btr", 19), ("p.pkg.btr.zst", 0)] {
            let out = t.0.join(out);
            convert_to(&src, &out, level);
            let cache = t.0.join(format!("{}.cache", out.file_name().unwrap().to_string_lossy()));
            let st = fetch(out.to_str().unwrap(), &cache, true, 3, None, &U).unwrap();
            assert!(st.encoded_chunks + st.fallback_bytes.min(1) + st.raw_bytes.min(1) > 0);
            let root = t.0.join(format!("root-{level}"));
            let ist = install(&cache, &root, false).unwrap();
            assert_eq!((ist.files, ist.hardlinks, ist.symlinks, ist.dirs), (6, 1, 1, 4));
            check_tree(&root, &s);
            // and the file sha256 check
            let digest = hex(&sha256(&fs::read(&out).unwrap()));
            fs::remove_file(&cache).unwrap();
            fetch(out.to_str().unwrap(), &cache, false, 3, Some(&digest), &U).unwrap();
            fs::remove_file(&cache).unwrap();
            assert!(fetch(out.to_str().unwrap(), &cache, false, 3, Some(&"0".repeat(64)), &U).is_err());
            assert!(!cache.exists() && !cache.with_extension("part").exists());
        }
        // the large file is deduplicated: one copy in the image
        // (parse_index reads the index at the start of the transport file)
        let idx = parse_index(&fs::read(t.0.join("p.pkg.btr")).unwrap()).unwrap();
        assert_eq!(idx.hdr.image_len, align(300_000) + align(200_000));
        // install refuses anything that isn't a cache file
        assert!(install(&t.0.join("p.pkg.btr"), &t.0.join("root-x"), false).is_err());
    }

    #[test]
    fn corrupt_transport_leaves_nothing() {
        let t = TempDir::new("corrupt");
        let src = make_pkg(&t.0, "p-1-1", &spec(2));
        let good = t.0.join("p.pkg.btr");
        convert_to(&src, &good, 19);
        let b = fs::read(&good).unwrap();
        let mut r = &b[..];
        let idx = read_index(&mut r).unwrap();
        let h = &idx.hdr;
        let t0 = HDR as u64;
        let e0 = t0 + h.table_len();
        let b0 = e0 + h.entries_len;
        let c0 = h.index_len();
        let last = b.len() as u64 - 1;
        let cache = t.0.join("c.pkg.btr");
        let bad = t.0.join("bad.pkg.btr");
        for (what, pos) in [("header", 20), ("header hash", 230), ("table", t0 + 3), ("table hash field", t0 + 30), ("entries", e0 + 5),
                            ("blob", b0 + 2), ("first chunk", c0 + 10), ("last chunk", last)] {
            let mut c = b.clone();
            c[pos as usize] ^= 0x40;
            fs::write(&bad, &c).unwrap();
            let e = fetch(bad.to_str().unwrap(), &cache, true, 3, None, &U);
            assert!(e.is_err(), "{what}: accepted");
            assert!(!cache.exists() && !cache.with_extension("part").exists(), "{what}: left a file");
        }
        for (what, c) in [("truncated", b[..b.len() - 1].to_vec()), ("trailing", [b.as_slice(), b"x"].concat())] {
            fs::write(&bad, &c).unwrap();
            assert!(fetch(bad.to_str().unwrap(), &cache, true, 3, None, &U).is_err(), "{what}: accepted");
            assert!(!cache.exists() && !cache.with_extension("part").exists(), "{what}: left a file");
        }
        // the outer-zstd form: a flipped byte anywhere is caught
        let gz = t.0.join("p.pkg.btr.zst");
        convert_to(&src, &gz, 0);
        let z = fs::read(&gz).unwrap();
        for pos in [10, z.len() / 2, z.len() - 3] {
            let mut c = z.clone();
            c[pos] ^= 0x01;
            fs::write(&bad, &c).unwrap();
            assert!(fetch(bad.to_str().unwrap(), &cache, true, 3, None, &U).is_err(), "zst byte {pos}: accepted");
            assert!(!cache.exists() && !cache.with_extension("part").exists());
        }
    }


    type TarEnt<'a> = (&'a str, tar::EntryType, u32, &'a [u8], Option<&'a str>);

    /// A package from a list of tar entries (path, kind, mode, body, link target).
    fn make_tar(dir: &Path, name: &str, ents: &[TarEnt]) -> PathBuf {
        let (uid, gid) = unsafe { (libc::getuid() as u64, libc::getgid() as u64) };
        let p = dir.join(format!("{name}.pkg.tar.zst"));
        let z = zstd::stream::write::Encoder::new(File::create(&p).unwrap(), 3).unwrap().auto_finish();
        let mut b = tar::Builder::new(z);
        for (path, kind, mode, body, link) in ents {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_mode(*mode);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_mtime(1_700_000_000);
            h.set_size(body.len() as u64);
            if let Some(l) = link { h.set_link_name(l).unwrap(); }
            h.set_path(path).unwrap();
            h.set_cksum();
            b.append(&h, *body).unwrap();
        }
        b.into_inner().unwrap();
        p
    }

    fn no_tmp_left(root: &Path) -> bool {
        fn walk(d: &Path) -> bool {
            fs::read_dir(d).unwrap().all(|e| {
                let e = e.unwrap();
                let n = e.file_name().to_string_lossy().to_string();
                !n.starts_with(".btrpkg-new") && (!e.file_type().unwrap().is_dir() || walk(&e.path()))
            })
        }
        walk(root)
    }

    #[test]
    fn package_symlinks_and_hardlinks_cant_escape() {
        use tar::EntryType::{Directory as D, Regular as F, Symlink as S, Link as L};
        let t = TempDir::new("escape");
        let outside = t.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let o = outside.to_str().unwrap().to_string();
        // through the package's own symlink: refused when converting, and by any reader
        for (n, ents) in [
            ("through-symlink", vec![(".PKGINFO", F, 0o644, b"pkgname = x\n" as &[u8], None), ("usr", D, 0o755, b"", None),
                ("usr/esc", S, 0o777, b"", Some(o.as_str())), ("usr/esc/pwned", F, 0o644, b"x", None)]),
            ("dir-over-symlink", vec![("usr", D, 0o755, b"", None), ("usr/esc", S, 0o777, b"", Some(o.as_str())), ("usr/esc", D, 0o777, b"", None),
                ("usr/esc/pwned", F, 0o644, b"x", None)]),
            ("through-file", vec![("usr", D, 0o755, b"", None), ("usr/f", F, 0o644, b"x", None), ("usr/f/x", F, 0o644, b"x", None)]),
            ("hardlink-outside", vec![("usr", D, 0o755, b"", None), ("usr/h", L, 0o644, b"", Some("etc/passwd"))]),
            ("hardlink-to-symlink", vec![("usr", D, 0o755, b"", None), ("usr/s", S, 0o777, b"", Some(o.as_str())), ("usr/h", L, 0o644, b"", Some("usr/s"))]),
        ] {
            let src = make_tar(&t.0, n, &ents);
            assert!(convert(&src, &t.0.join(format!("{n}.pkg.btr")), 19, 19, 4096, 1, 4096).is_err(), "{n}: converted");
        }
        // through symlinks already on disk: refused by install, nothing outside changed, no
        // temporary file left
        let ok = make_tar(&t.0, "ok", &[("usr", D, 0o755, b"", None), ("usr/evil", D, 0o777, b"", None),
            ("usr/evil/x", F, 0o644, &[1u8; 9000], None), ("usr/y", F, 0o644, b"y", None)]);
        let btr = t.0.join("ok.pkg.btr");
        convert(&ok, &btr, 19, 19, 4096, 1, 4096).unwrap();
        let cache = t.0.join("ok.cache");
        fetch(btr.to_str().unwrap(), &cache, true, 3, None, &U).unwrap();
        let root = t.0.join("root");
        fs::create_dir_all(root.join("usr")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("usr/evil")).unwrap();
        let mode_before = fs::metadata(&outside).unwrap().permissions().mode();
        let e = install(&cache, &root, false).unwrap_err();
        assert!(format!("{e:#}").contains("not a directory") || format!("{e:#}").contains("symlink"), "{e:#}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(fs::metadata(&outside).unwrap().permissions().mode(), mode_before);
        assert!(no_tmp_left(&root));
        // a symlink higher up: the root's usr itself
        let root2 = t.0.join("root2");
        fs::create_dir_all(&root2).unwrap();
        std::os::unix::fs::symlink(&outside, root2.join("usr")).unwrap();
        assert!(install(&cache, &root2, false).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        // a failure mid-install (a directory where a file goes) leaves no temporary file
        let root3 = t.0.join("root3");
        fs::create_dir_all(root3.join("usr/y")).unwrap();
        assert!(install(&cache, &root3, false).is_err());
        assert!(no_tmp_left(&root3));
    }

    #[test]
    fn wrong_unencoded_hash_is_refused() {
        let t = TempDir::new("rawhash");
        let src = make_pkg(&t.0, "w-1-1", &spec(10));
        let good = t.0.join("w.pkg.btr");
        convert_to(&src, &good, 19);
        let b = fs::read(&good).unwrap();
        let idx = parse_index(&b).unwrap();
        // a self-consistent index (every section hash right) whose table claims other data for a chunk
        let i = idx.chunks.iter().position(|c| c.compression == COMP_ZSTD).unwrap();
        let mut chunks = idx.chunks.clone();
        chunks[i].raw[0] ^= 1;
        let ib = idx.with_chunks(&chunks);
        let bad = t.0.join("bad.pkg.btr");
        fs::write(&bad, [ib.as_slice(), &b[idx.bytes.len()..]].concat()).unwrap();
        let cache = t.0.join("c.pkg.btr");
        // --verify-data decodes every chunk: refused, nothing left
        let full = FetchOpts { use_encoded: true, level: 3, verify_data: true, fsync: false };
        let e = fetch_with(bad.to_str().unwrap(), &cache, None, &U, &full).unwrap_err();
        assert!(format!("{e:#}").contains("unencoded hash"), "{e:#}");
        assert!(!cache.exists() && !part_path(&cache).exists());
        // publish refuses it too
        assert!(store::publish(&bad, &t.0.join("store"), None).is_err());
        // the default fetch checks what the signed stored hash covers, so it takes the chunk; the
        // wrong unencoded hash can't be used for reuse later: reuse reads the data back
        let cdir = t.0.join("cdir");
        fs::create_dir_all(&cdir).unwrap();
        fetch(bad.to_str().unwrap(), &cdir.join("w-1-1.pkg.btr"), true, 3, None, &U).unwrap();
        let store = t.0.join("store");
        let src2 = make_pkg(&t.0, "w-1-2", &spec(10));
        convert_to(&src2, &t.0.join("w-1-2.pkg.btr"), 19);
        store::publish(&t.0.join("w-1-2.pkg.btr"), &store, None).unwrap();
        let p = pull_dir(&store, "w-1-2", &cdir.join("w-1-2.pkg.btr"), &U).unwrap();
        assert!(p.bad_local == 0 && p.reused_chunks == (idx.chunks.len() - 1) as u64, "{p:?}");
        let root = t.0.join("root");
        install(&cdir.join("w-1-2.pkg.btr"), &root, false).unwrap();
        check_tree(&root, &spec(10));
    }

    #[test]
    fn signed_store() {
        let t = TempDir::new("signed");
        let store = t.0.join("store");
        let src = make_pkg(&t.0, "s-1-1", &spec(8));
        convert_to(&src, &t.0.join("s-1-1.pkg.btr"), 19);
        sign::keygen(&t.0.join("repo")).unwrap();
        sign::keygen(&t.0.join("other")).unwrap();
        let k = sign::SecretKey::load(&t.0.join("repo.sec")).unwrap();
        store::publish(&t.0.join("s-1-1.pkg.btr"), &store, Some((&k, "repo"))).unwrap();
        let keys = t.0.join("keys");
        fs::create_dir_all(&keys).unwrap();
        fs::copy(t.0.join("repo.pub"), keys.join("repo.pub")).unwrap();
        let trust = |sig: String| sign::Trust::Signed(sign::Keyring::load(&[keys.clone()]).unwrap(), sig);
        let sig = store::read_sig(&store, "s-1-1").unwrap();
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        pull_dir(&store, "s-1-1", &cdir.join("a.pkg.btr"), &trust(sig.clone())).unwrap();
        // signed by a key the keyring doesn't have
        let other = sign::SecretKey::load(&t.0.join("other.sec")).unwrap();
        let ib = fs::read(store.join("index/s-1-1.btridx")).unwrap();
        assert!(pull_dir(&store, "s-1-1", &cdir.join("b.pkg.btr"), &trust(other.sign(&ib[..HDR], "other"))).is_err());
        // a tampered index (re-hashed, so only the signature can tell)
        let idx = parse_index(&ib).unwrap();
        let mut ch = idx.chunks.clone();
        ch[0].raw[1] ^= 1;
        fs::write(store.join("index/s-1-1.btridx"), idx.with_chunks(&ch)).unwrap();
        assert!(pull_dir(&store, "s-1-1", &cdir.join("c.pkg.btr"), &trust(sig)).is_err());
        // nothing but the first cache file
        assert_eq!(fs::read_dir(&cdir).unwrap().count(), 1);
        // a transport file with a detached signature (btrpkg sign)
        let tp = t.0.join("s-1-1.pkg.btr");
        let tsig = k.sign(&fs::read(&tp).unwrap()[..HDR], "repo");
        fetch(tp.to_str().unwrap(), &cdir.join("d.pkg.btr"), true, 3, None, &trust(tsig)).unwrap();
    }


    #[test]
    fn repo_update_adds_fields() {
        let t = TempDir::new("repo");
        let d = t.0.join("repo");
        fs::create_dir_all(&d).unwrap();
        let mut tb = tar::Builder::new(Vec::new());
        for (n, v, present) in [("p", "1-1", true), ("q", "2-1", true), ("gone", "1-1", false)] {
            let fname = format!("{n}-{v}-x86_64.pkg.tar.zst");
            let sum = if present {
                let src = make_pkg(&d, &format!("{n}-{v}-x86_64"), &spec(n.len() as u64));
                hex(&sha256(&fs::read(src).unwrap()))
            } else { "0".repeat(64) };
            let desc = format!("%FILENAME%\n{fname}\n\n%NAME%\n{n}\n\n%VERSION%\n{v}\n\n%SHA256SUM%\n{sum}\n\n%DATA%\nother.key=1\n\n");
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_path(format!("{n}-{v}/")).unwrap();
            h.set_mode(0o755);
            h.set_size(0);
            h.set_cksum();
            tb.append(&h, &[][..]).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("{n}-{v}/desc")).unwrap();
            h.set_mode(0o644);
            h.set_size(desc.len() as u64);
            h.set_cksum();
            tb.append(&h, desc.as_bytes()).unwrap();
        }
        fs::write(d.join("t.db.tar.zst"), zstd::bulk::compress(&tb.into_inner().unwrap(), 3).unwrap()).unwrap();
        std::os::unix::fs::symlink("t.db.tar.zst", d.join("t.db")).unwrap();
        sign::keygen(&t.0.join("k")).unwrap();
        let k = sign::SecretKey::load(&t.0.join("k.sec")).unwrap();
        let o = repo::RepoOpts { key: Some((&k, "k")), gpg_keyring: None, drop_missing: true, level: 19, small: 4096, jobs: 2, db_in: None };
        let st = repo::repo_update(&d, "t", &o).unwrap();
        assert_eq!((st.packages, st.converted, st.missing, st.dropped, st.failed), (3, 2, 1, 1, 0), "{st:?}");
        // the database: same layout, fields added, other %DATA% kept, the missing package gone
        let db = zstd::stream::decode_all(&fs::read(d.join("t.db.tar.zst")).unwrap()[..]).unwrap();
        let mut descs = std::collections::BTreeMap::new();
        for e in tar::Archive::new(&db[..]).entries().unwrap() {
            let mut e = e.unwrap();
            let p = e.path().unwrap().to_string_lossy().to_string();
            let mut s = String::new();
            e.read_to_string(&mut s).unwrap();
            descs.insert(p, s);
        }
        assert_eq!(descs.keys().cloned().collect::<Vec<_>>(), ["p-1-1/", "p-1-1/desc", "q-2-1/", "q-2-1/desc"]);
        let q = &descs["q-2-1/desc"];
        assert!(q.contains("%DATA%\nother.key=1\nbtrpkg.index=index/q-2-1-x86_64.btridx\nbtrpkg.id="), "{q}");
        let id = q.lines().find_map(|l| l.strip_prefix("btrpkg.id=")).unwrap();
        // the ID from the database is enough to pull, and the signature is there too
        let c = t.0.join("cache");
        fs::create_dir_all(&c).unwrap();
        pull_dir(&d, "q-2-1-x86_64", &c.join("q.pkg.btr"), &sign::Trust::Id(store::unhex(id).unwrap())).unwrap();
        assert!(store::read_sig(&d, "q-2-1-x86_64").is_some());
        // the original packages stay
        assert!(d.join("q-2-1-x86_64.pkg.tar.zst").exists());
        // a second run converts nothing
        let st = repo::repo_update(&d, "t", &o).unwrap();
        assert_eq!((st.converted, st.unchanged), (0, 2), "{st:?}");
    }


    #[test]
    fn pull_many_shares_a_queue_and_fails_as_a_whole() {
        let t = TempDir::new("many");
        let store = t.0.join("store");
        let names = ["a-1-1-x86_64", "b-1-1-x86_64"];
        for (n, s) in names.iter().zip([spec(3), spec(11)]) {
            let src = make_pkg(&t.0, n, &s);
            convert_to(&src, &t.0.join(format!("{n}.pkg.btr")), 19);
            store::publish(&t.0.join(format!("{n}.pkg.btr")), &store, None).unwrap();
        }
        let src = net::Source::Dir(store.clone());
        let mut ns = net::NetStats::default();
        let ibs: Vec<Vec<u8>> = names.iter().map(|n| store::fetch_index(&src, n, &mut ns).unwrap().0).collect();
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        let caches: Vec<PathBuf> = names.iter().map(|n| cdir.join(format!("{n}.pkg.btr"))).collect();
        let o = store::PullOpts { use_encoded: true, installed: None, level: 3, verify_data: false, fsync: true, before_fetch: None, plan_only: false };
        let items = |caches: &[PathBuf]| -> Vec<(String, Vec<u8>, PathBuf)> {
            names.iter().zip(&ibs).zip(caches).map(|((n, ib), c)| (n.to_string(), ib.clone(), c.clone())).collect()
        };
        let run = |v: &[(String, Vec<u8>, PathBuf)]| {
            let it: Vec<store::PullItem> = v.iter().map(|(n, ib, c)| store::PullItem { name: n, ib, cache: c, trust: &U }).collect();
            store::pull_many(&src, &it, &o, net::NetStats::default())
        };
        // a chunk only b has, damaged on the server: nothing of either package is left behind
        let idx_a = read_index_checked(&mut &ibs[0][..], &|h| U.check(h)).unwrap();
        let idx_b = read_index_checked(&mut &ibs[1][..], &|h| U.check(h)).unwrap();
        let only_b = idx_b.chunks.iter().find(|c| !idx_a.chunks.iter().any(|a| a.stored == c.stored)).unwrap();
        let x = hex(&only_b.stored);
        let cp = store.join(format!("chunks/{}/{x}", &x[..2]));
        let good = fs::read(&cp).unwrap();
        let mut bad = good.clone();
        let k = bad.len() / 2;
        bad[k] ^= 1;
        fs::write(&cp, &bad).unwrap();
        assert!(run(&items(&caches)).is_err());
        assert_eq!(fs::read_dir(&cdir).unwrap().count(), 0, "cache dir must stay empty");
        // repaired: both pulled in one call, and both install to the right trees
        fs::write(&cp, &good).unwrap();
        let st = run(&items(&caches)).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st.iter().map(|s| s.fetched_chunks).sum::<u64>(), (idx_a.chunks.len() + idx_b.chunks.len()) as u64);
        for (c, s) in caches.iter().zip([spec(3), spec(11)]) {
            let r = t.0.join(format!("root-{}", c.file_name().unwrap().to_str().unwrap()));
            install(c, &r, false).unwrap();
            check_tree(&r, &s);
        }
        // again: everything already in the cache
        let st = run(&items(&caches)).unwrap();
        assert!(st.iter().all(|s| s.fetched_chunks == 0 && s.reused_chunks == s.chunks), "{st:?}");
    }

    #[test]
    fn pull_reuses_installed_files() {
        let t = TempDir::new("installed");
        let store = t.0.join("store");
        let (s1, s2) = (spec(3), spec(4)); // same big file, other library
        for (n, s) in [("q-1-1-x86_64", &s1), ("q-2-1-x86_64", &s2)] {
            let src = make_pkg(&t.0, n, s);
            convert_to(&src, &t.0.join(format!("{n}.pkg.btr")), 19);
            store::publish(&t.0.join(format!("{n}.pkg.btr")), &store, None).unwrap();
        }
        // version 1 installed some other way (here: from a cache elsewhere, then that cache is gone)
        let root = t.0.join("root");
        let other = t.0.join("other");
        fs::create_dir_all(&other).unwrap();
        pull_dir(&store, "q-1-1-x86_64", &other.join("q-1-1-x86_64.pkg.btr"), &U).unwrap();
        install(&other.join("q-1-1-x86_64.pkg.btr"), &root, false).unwrap();
        fs::remove_dir_all(&other).unwrap();
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        let o = store::PullOpts { use_encoded: true, installed: Some(&root), level: 3, verify_data: false, fsync: true, before_fetch: None, plan_only: false };
        let src = net::Source::Dir(store.clone());
        let p = store::pull(&src, "q-2-1-x86_64", &cdir.join("q-2-1-x86_64.pkg.btr"), &U, &o).unwrap();
        // tool (300 kB, 3 chunks) is unchanged and comes from the installed file
        assert_eq!((p.installed_chunks, p.fetched_chunks, p.reused_chunks), (3, 2, 0), "{p:?}");
        let r2 = t.0.join("root2");
        install(&cdir.join("q-2-1-x86_64.pkg.btr"), &r2, false).unwrap();
        check_tree(&r2, &s2);
        // a changed installed file is not used
        let f = OpenOptions::new().write(true).open(root.join("usr/bin/tool")).unwrap();
        f.write_all_at(b"changed", 1000).unwrap();
        drop(f);
        fs::remove_file(cdir.join("q-2-1-x86_64.pkg.btr")).unwrap();
        let p = store::pull(&src, "q-2-1-x86_64", &cdir.join("q-2-1-x86_64.pkg.btr"), &U, &o).unwrap();
        assert_eq!((p.installed_chunks, p.fetched_chunks), (2, 3), "{p:?}");
        let r3 = t.0.join("root3");
        install(&cdir.join("q-2-1-x86_64.pkg.btr"), &r3, false).unwrap();
        check_tree(&r3, &s2);
        assert_eq!(store::pkgname("q-2-1-x86_64"), "q");
        assert_eq!(store::pkgname("omarchy-dev-4.0.0.r8083.gb20d787-1-x86_64"), "omarchy-dev");
    }

    #[test]
    fn publish_pull_reuses_chunks() {
        let t = TempDir::new("store");
        let store = t.0.join("store");
        let (s1, s2) = (spec(3), spec(4)); // same big file, other library
        for (n, s) in [("q-1-1", &s1), ("q-2-1", &s2)] {
            let src = make_pkg(&t.0, n, s);
            convert_to(&src, &t.0.join(format!("{n}.pkg.btr")), 19);
            store::publish(&t.0.join(format!("{n}.pkg.btr")), &store, None).unwrap();
        }
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        let p1 = pull_dir(&store, "q-1-1", &cdir.join("q-1-1.pkg.btr"), &U).unwrap();
        assert_eq!(p1.reused_chunks, 0);
        let p2 = pull_dir(&store, "q-2-1", &cdir.join("q-2-1.pkg.btr"), &U).unwrap();
        // tool (300 kB) is unchanged: its 3 chunks come from the first cache file
        assert_eq!(p2.reused_chunks, 3, "{p2:?}");
        assert_eq!(p2.fetched_chunks, 2, "{p2:?}");
        let root = t.0.join("root");
        install(&cdir.join("q-2-1.pkg.btr"), &root, false).unwrap();
        check_tree(&root, &s2);

        // a damaged cache file is not a source: its chunks are checked before reuse
        let c1 = cdir.join("q-1-1.pkg.btr");
        let (i1, f1) = open_cache(&c1).unwrap();
        let tool = i1.entries.iter().find(|e| e.path == b"usr/bin/tool").unwrap();
        drop(f1);
        let f1 = OpenOptions::new().write(true).open(&c1).unwrap();
        f1.write_all_at(b"garbage", i1.hdr.image_base() + tool.data_off + 10).unwrap();
        drop(f1);
        fs::remove_file(cdir.join("q-2-1.pkg.btr")).unwrap();
        let p3 = pull_dir(&store, "q-2-1", &cdir.join("q-2-1.pkg.btr"), &U).unwrap();
        assert_eq!((p3.bad_local, p3.reused_chunks, p3.fetched_chunks), (1, 2, 3), "{p3:?}");
        let root = t.0.join("root-again");
        install(&cdir.join("q-2-1.pkg.btr"), &root, false).unwrap();
        check_tree(&root, &s2);

        // the header ID from the repo database: a wrong one is refused before anything is written
        let ib = fs::read(store.join("index/q-2-1.btridx")).unwrap();
        let c3 = cdir.join("again.pkg.btr");
        assert!(pull_dir(&store, "q-2-1", &c3, &sign::Trust::Id([0; 32])).is_err());
        assert!(!c3.exists() && !part_path(&c3).exists());
        let id = Header::decode(&ib).unwrap().id();
        pull_dir(&store, "q-2-1", &c3, &sign::Trust::Id(id)).unwrap();

        // a damaged chunk in the store: refused, nothing left behind
        let idx = parse_index(&ib).unwrap();
        let x = hex(&idx.chunks[3].stored);
        let cp = store.join("chunks").join(&x[..2]).join(&x);
        let mut d = fs::read(&cp).unwrap();
        d[5] ^= 1;
        fs::write(&cp, &d).unwrap();
        let fresh = t.0.join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        assert!(pull_dir(&store, "q-2-1", &fresh.join("q.pkg.btr"), &U).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
        // a damaged index: refused
        let mut ib2 = ib.clone();
        ib2[HDR + 7] ^= 1;
        fs::write(store.join("index/bad.btridx"), &ib2).unwrap();
        assert!(pull_dir(&store, "bad", &fresh.join("bad.pkg.btr"), &U).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
    }

    #[test]
    fn publish_reuses_chunks_across_compressor_changes() {
        let t = TempDir::new("remap");
        let store = t.0.join("store");
        let s = spec(5);
        let src = make_pkg(&t.0, "r-1-1", &s);
        convert_to(&src, &t.0.join("r-1-1.pkg.btr"), 19);
        let (_, a) = store::publish(&t.0.join("r-1-1.pkg.btr"), &store, None).unwrap();
        assert_eq!(a.remapped, 0);
        // the same data, framed at another level: different bytes, same unencoded data
        let src2 = make_pkg(&t.0, "r-1-2", &s);
        convert_to(&src2, &t.0.join("r-1-2.pkg.btr"), 3);
        let (_, b) = store::publish(&t.0.join("r-1-2.pkg.btr"), &store, None).unwrap();
        assert!(b.remapped > 0, "{b:?}");
        assert_eq!(b.new_chunks, 0, "{b:?}");
        let i1 = parse_index(&fs::read(store.join("index/r-1-1.btridx")).unwrap()).unwrap();
        let i2 = parse_index(&fs::read(store.join("index/r-1-2.btridx")).unwrap()).unwrap();
        assert_eq!(i1.chunks, i2.chunks);
        let c = t.0.join("c");
        fs::create_dir_all(&c).unwrap();
        pull_dir(&store, "r-1-2", &c.join("r.pkg.btr"), &U).unwrap();
        install(&c.join("r.pkg.btr"), &t.0.join("root"), false).unwrap();
        check_tree(&t.0.join("root"), &s);

        // a raw-map entry pointing at a chunk with other data: publish refuses
        let x = hex(&i1.chunks[0].raw);
        let y = &i1.chunks[i1.chunks.len() - 1];
        fs::write(store.join("raw").join(&x[..2]).join(&x), format!("{} {} {}\n", y.compression, y.encoded_len, hex(&y.stored))).unwrap();
        let e = store::publish(&t.0.join("r-1-2.pkg.btr"), &store, None).unwrap_err();
        assert!(format!("{e:#}").contains("raw map"), "{e:#}");
    }
}
