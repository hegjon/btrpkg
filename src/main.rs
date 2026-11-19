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
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

mod format;
mod store;

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
        d.window_log_max(31)?;
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
                    ensure!(size <= 1 << 34, "zstd frame too large");
                    let mut d = zstd::bulk::Decompressor::new()?;
                    d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(31))?;
                    let out = d.decompress(f, size as usize).context("truncated package: zstd frame")?;
                    ensure!(out.len() as u64 == size, "zstd frame decoded to the wrong size");
                    Ok(out)
                }
                _ => {
                    // no content size (older single-frame files): stream it
                    let mut d = zstd::stream::read::Decoder::new(f)?;
                    d.window_log_max(31)?;
                    let mut out = Vec::new();
                    d.read_to_end(&mut out).context("truncated package: zstd frame")?;
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

/// Check a chunk's bytes against the table before anything is written: the kernel stores
/// encoded frames without looking at them.
fn check_chunk(i: usize, c: &Chunk, d: &[u8]) -> Result<()> {
    ensure!(d.len() == c.encoded_len as usize && sha256(d) == c.stored, "chunk {i} is corrupt");
    Ok(())
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
pub(crate) fn fetch(input: &str, cache: &Path, use_encoded: bool, level: i32, file_sha256: Option<&str>) -> Result<FetchStats> {
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
    let idx = read_index(&mut rd)?;
    let image_base = idx.hdr.image_base();

    let tmp = cache.with_extension("part");
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
                batch.par_iter().try_for_each(|(i, c, d)| check_chunk(*i, c, d))?;
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

pub(crate) fn install(cache: &Path, root: &Path, force_copy: bool) -> Result<InstallStats> {
    let (idx, file) = open_cache(cache)?;
    let image_base = idx.hdr.image_base();
    let mut st = InstallStats::default();
    let mut dirs = Vec::new();
    let mut can_clone = !force_copy;
    for e in &idx.entries {
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
                    (&f).write_all(&idx.small[s..s + e.size as usize])?;
                    st.small_files += 1;
                    st.small_bytes += e.size;
                } else {
                    // check_entries made sure the range lies inside the image
                    let src = image_base + e.data_off;
                    let alen = align(e.size);
                    let mut done = false;
                    if can_clone {
                        match clone_range(&file, src, alen, &f) {
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut flags: Vec<&str> = vec![];
    let mut pos: Vec<&str> = vec![];
    let mut level: Option<i32> = None;
    let mut outer_level = 19;
    let mut sha: Option<String> = None;
    let mut nframes = 1u64;
    let mut min_frame = 1u64 << 20;
    let mut small_max = 4096u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-l" | "--level" => level = Some(it.next().context("-l needs a value")?.parse()?),
            "--frames" => nframes = it.next().context("--frames needs a value")?.parse()?,
            "--min-frame" => min_frame = it.next().context("--min-frame needs a value")?.parse()?,
            "--sha256" => sha = Some(it.next().context("--sha256 needs a value")?.clone()),
            "--outer-level" => outer_level = it.next().context("--outer-level needs a value")?.parse()?,
            "--small" => small_max = it.next().context("--small needs a value")?.parse()?,
            s if s.starts_with("--") => flags.push(s),
            s => pos.push(s),
        }
    }
    match pos.as_slice() {
        // .pkg.btr: chunks framed at -l (default 19). .pkg.btr.zst: raw chunks (-l 0
        // by default) inside one solid zstd stream at --outer-level.
        ["convert", i, o] => {
            let outer = o.ends_with(".zst");
            convert(Path::new(i), Path::new(o), level.unwrap_or(if outer { 0 } else { 19 }), outer_level, small_max, nframes, min_frame)
        }
        ["fetch", i, c] => fetch(i, Path::new(c), !flags.contains(&"--no-encoded"), level.unwrap_or(3), sha.as_deref())
            .map(|st| eprintln!("fetch: {st:?}")),
        ["install", c, r] => install(Path::new(c), Path::new(r), flags.contains(&"--copy")).map(|st| eprintln!("install: {st:?}")),
        ["info", p] => info(Path::new(p)),
        ["publish", i, s] => store::publish(Path::new(i), Path::new(s)).map(|(name, st)| {
            eprintln!("publish: {st:?}");
            println!("upload\t{name}\t{}\t{}\t{}\t{}", st.new_chunks, st.new_bytes, st.index_bytes, st.raw_entries);
        }),
        ["pull", s, n, c] => store::pull(Path::new(s), n, Path::new(c), !flags.contains(&"--no-encoded"), sha.as_deref()).map(|st| {
            eprintln!("pull: {st:?}");
            println!("download\t{n}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes);
        }),
        _ => bail!("usage: btrpkg convert [-l N] [--outer-level N] [--frames N] [--min-frame BYTES] [--small N] IN OUT(.pkg.btr|.pkg.btr.zst) | fetch [--no-encoded] [-l N] [--sha256 HEX] IN CACHE | install [--copy] CACHE ROOT | info FILE | publish IN.pkg.btr STORE | pull [--no-encoded] [--sha256 HEX] STORE NAME CACHE"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicU32, Ordering};

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
        let n = unsafe { libc::lgetxattr(cpath(&root.join("usr/share/tagged")).as_ptr(), c"user.btrpkg".as_ptr(), v.as_mut_ptr() as *mut _, 8) };
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
            let st = fetch(out.to_str().unwrap(), &cache, true, 3, None).unwrap();
            assert!(st.encoded_chunks + st.fallback_bytes.min(1) + st.raw_bytes.min(1) > 0);
            let root = t.0.join(format!("root-{level}"));
            let ist = install(&cache, &root, false).unwrap();
            assert_eq!((ist.files, ist.hardlinks, ist.symlinks, ist.dirs), (6, 1, 1, 4));
            check_tree(&root, &s);
            // and the file sha256 check
            let digest = hex(&sha256(&fs::read(&out).unwrap()));
            fs::remove_file(&cache).unwrap();
            fetch(out.to_str().unwrap(), &cache, false, 3, Some(&digest)).unwrap();
            fs::remove_file(&cache).unwrap();
            assert!(fetch(out.to_str().unwrap(), &cache, false, 3, Some(&"0".repeat(64))).is_err());
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
            let e = fetch(bad.to_str().unwrap(), &cache, true, 3, None);
            assert!(e.is_err(), "{what}: accepted");
            assert!(!cache.exists() && !cache.with_extension("part").exists(), "{what}: left a file");
        }
        for (what, c) in [("truncated", b[..b.len() - 1].to_vec()), ("trailing", [b.as_slice(), b"x"].concat())] {
            fs::write(&bad, &c).unwrap();
            assert!(fetch(bad.to_str().unwrap(), &cache, true, 3, None).is_err(), "{what}: accepted");
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
            assert!(fetch(bad.to_str().unwrap(), &cache, true, 3, None).is_err(), "zst byte {pos}: accepted");
            assert!(!cache.exists() && !cache.with_extension("part").exists());
        }
    }

    #[test]
    fn publish_pull_reuses_chunks() {
        let t = TempDir::new("store");
        let store = t.0.join("store");
        let (s1, s2) = (spec(3), spec(4)); // same big file, other library
        for (n, s) in [("q-1-1", &s1), ("q-2-1", &s2)] {
            let src = make_pkg(&t.0, n, s);
            convert_to(&src, &t.0.join(format!("{n}.pkg.btr")), 19);
            store::publish(&t.0.join(format!("{n}.pkg.btr")), &store).unwrap();
        }
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        let p1 = store::pull(&store, "q-1-1", &cdir.join("q-1-1.pkg.btr"), true, None).unwrap();
        assert_eq!(p1.reused_chunks, 0);
        let p2 = store::pull(&store, "q-2-1", &cdir.join("q-2-1.pkg.btr"), true, None).unwrap();
        // tool (300 kB) is unchanged: its 3 chunks come from the first cache file
        assert_eq!(p2.reused_chunks, 3, "{p2:?}");
        assert_eq!(p2.fetched_chunks, 2, "{p2:?}");
        let root = t.0.join("root");
        install(&cdir.join("q-2-1.pkg.btr"), &root, false).unwrap();
        check_tree(&root, &s2);

        // the index sha256 from the repo database
        let ib = fs::read(store.join("index/q-2-1.btridx")).unwrap();
        let c3 = cdir.join("again.pkg.btr");
        assert!(store::pull(&store, "q-2-1", &c3, true, Some(&"0".repeat(64))).is_err());
        store::pull(&store, "q-2-1", &c3, true, Some(&hex(&sha256(&ib)))).unwrap();

        // a damaged chunk in the store: refused, nothing left behind
        let idx = parse_index(&ib).unwrap();
        let x = hex(&idx.chunks[3].stored);
        let cp = store.join("chunks").join(&x[..2]).join(&x);
        let mut d = fs::read(&cp).unwrap();
        d[5] ^= 1;
        fs::write(&cp, &d).unwrap();
        let fresh = t.0.join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        assert!(store::pull(&store, "q-2-1", &fresh.join("q.pkg.btr"), true, None).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
        // a damaged index: refused
        let mut ib2 = ib.clone();
        ib2[HDR + 7] ^= 1;
        fs::write(store.join("index/bad.btridx"), &ib2).unwrap();
        assert!(store::pull(&store, "bad", &fresh.join("bad.pkg.btr"), true, None).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
    }

    #[test]
    fn publish_reuses_chunks_across_compressor_changes() {
        let t = TempDir::new("remap");
        let store = t.0.join("store");
        let s = spec(5);
        let src = make_pkg(&t.0, "r-1-1", &s);
        convert_to(&src, &t.0.join("r-1-1.pkg.btr"), 19);
        let (_, a) = store::publish(&t.0.join("r-1-1.pkg.btr"), &store).unwrap();
        assert_eq!(a.remapped, 0);
        // the same data, framed at another level: different bytes, same unencoded data
        let src2 = make_pkg(&t.0, "r-1-2", &s);
        convert_to(&src2, &t.0.join("r-1-2.pkg.btr"), 3);
        let (_, b) = store::publish(&t.0.join("r-1-2.pkg.btr"), &store).unwrap();
        assert!(b.remapped > 0, "{b:?}");
        assert_eq!(b.new_chunks, 0, "{b:?}");
        let i1 = parse_index(&fs::read(store.join("index/r-1-1.btridx")).unwrap()).unwrap();
        let i2 = parse_index(&fs::read(store.join("index/r-1-2.btridx")).unwrap()).unwrap();
        assert_eq!(i1.chunks, i2.chunks);
        let c = t.0.join("c");
        fs::create_dir_all(&c).unwrap();
        store::pull(&store, "r-1-2", &c.join("r.pkg.btr"), true, None).unwrap();
        install(&c.join("r.pkg.btr"), &t.0.join("root"), false).unwrap();
        check_tree(&t.0.join("root"), &s);
    }
}
