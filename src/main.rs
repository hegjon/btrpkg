//! btrpkg: pacman packages whose file data is shipped as btrfs encoded extents (btrparts).
//!
//! Commands: see USAGE below (`btrpkg` with no arguments). The format is described in format.rs
//! (version 0); the repository layout, the pacman integration and the trust model in README.md.

use anyhow::{bail, ensure, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

mod format;
mod cache;
mod fetchids;
mod fsx;
mod gc;
mod net;
mod repo;
mod sign;
mod store;
mod upgrade;

use format::*;

// ---------------------------------------------------------------- convert

/// zstd level of every btrpart (made by convert, or by pull from installed files). 12 is the
/// highest level that decodes as fast as level 3 in btrfs (13 is 5-10% slower, 15-19 about 20%
/// slower, on every lab machine; bench/results/zstd-level-read-20261122), and stores 5.5% less
/// than level 3.
const LEVEL: i32 = 12;

/// One btrfs-readable zstd frame for a 4 KiB-aligned chunk of at most 128 KiB
/// (window <= 128 KiB, no checksum, no dictionary ID). Stays raw unless the
/// frame saves at least one 4 KiB block on disk.
fn frame(data: Vec<u8>) -> Result<(u8, Vec<u8>)> { frame_at(LEVEL, data) }

fn frame_at(level: i32, data: Vec<u8>) -> Result<(u8, Vec<u8>)> {
    let mut c = zstd::bulk::Compressor::new(level)?;
    c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(17))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(false))?;
    c.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
    let z = c.compress(&data)?;
    if align(z.len() as u64) < data.len() as u64 { Ok((COMP_ZSTD, z)) } else { Ok((COMP_NONE, data)) }
}

/// A package archive's tar stream, by its magic bytes: zstd (Arch, Omarchy), xz (Arch Linux ARM
/// ships .pkg.tar.xz, `any` packages included), gzip, or plain tar.
fn package_reader(input: &Path) -> Result<Box<dyn Read>> {
    let f = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut b = BufReader::new(f);
    let magic = b.fill_buf()?.get(..6).map(<[u8]>::to_vec).unwrap_or_default();
    Ok(if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::stream::read::Decoder::with_buffer(b)?)
    } else if magic.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Box::new(liblzma::read::XzDecoder::new_multi_decoder(b))
    } else if magic.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::bufread::MultiGzDecoder::new(b))
    } else {
        Box::new(b)
    })
}

/// A package archive's index and btrparts (in table order), made in memory: files of
/// `small_max` bytes or less go in the blob, identical contents are stored once, large files are
/// cut into btrparts of at most 128 KiB, each a zstd frame btrfs can read (or raw).
pub(crate) fn build(input: &Path, small_max: u64) -> Result<(Vec<u8>, Vec<(Chunk, Vec<u8>)>)> {
    let mut ar = tar::Archive::new(package_reader(input)?);
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
    // (window <= 128 KiB, no checksum, no dictionary).
    let compressed: Vec<(Chunk, Vec<u8>)> = jobs
        .into_par_iter()
        .map(|(off, data)| -> Result<(Chunk, Vec<u8>)> {
            let unencoded_len = data.len() as u32;
            let raw = sha256(&data);
            let (comp, bytes) = frame(data)?;
            let stored = if comp == COMP_NONE { raw } else { sha256(&bytes) };
            Ok((Chunk { image_off: off, unencoded_len, encoded_len: bytes.len() as u32, compression: comp, stored, raw }, bytes))
        })
        .collect::<Result<_>>()?;

    let chunks: Vec<Chunk> = compressed.iter().map(|(c, _)| *c).collect();
    let index = build_index(&entries, &small, &chunks, cursor)?;
    if stats.dedup_files > 0 { eprintln!("convert: {:?}", stats); }
    Ok((index, compressed))
}

/// The package name of a package file: its file name without `.pkg.tar[.zst|.xz|.gz]`.
pub(crate) fn pkg_stem(input: &Path) -> Result<String> {
    let n = input.file_name().context("no file name")?.to_string_lossy();
    let s = n.trim_end_matches(".zst").trim_end_matches(".xz").trim_end_matches(".gz");
    Ok(s.strip_suffix(".pkg.tar").with_context(|| format!("{n}: not a .pkg.tar.* file"))?.to_string())
}

/// Convert a package file into STORE: NAME.btrpkg (signed with `key`) and its btrparts in `parts`.
pub(crate) fn convert(input: &Path, store: &Path, parts: &Path, key: Option<(&sign::SecretKey, &str)>, reuse: &store::RawMap, small_max: u64) -> Result<(String, store::PublishStats)> {
    let name = pkg_stem(input)?;
    let (index, items) = build(input, small_max)?;
    Ok((name.clone(), store::publish(&index, items, &name, store, parts, key, reuse)?))
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

// ---------------------------------------------------------------- cache files

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

/// Describe an index or cache file (told apart by their length).
fn info(p: &Path) -> Result<()> {
    let len = fs::metadata(p)?.len();
    let mut f = BufReader::new(File::open(p)?);
    let idx = read_index(&mut f)?;
    let h = &idx.hdr;
    let form = if len == h.index_len() { "index" } else if len == h.cache_len() { "cache" } else { "unknown (wrong length)" };
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

/// What a signature covers: the 256-byte header of an index or cache file (its magic and
/// version keep it from being valid for anything else), or a whole `.btrdb` database index,
/// decompressed (it starts with the `BTRDB` magic). The file may hold the index as published
/// (zstd) or decompressed, as a client has it after applying a patch.
fn signed_message(p: &Path) -> Result<Vec<u8>> {
    let b = fs::read(p).with_context(|| p.display().to_string())?;
    if p.extension().is_some_and(|e| e == "btrdb") {
        const MAX: u64 = 64 << 20;
        let raw = if b.starts_with(b"BTRDB\0\0\0") { b } else {
            let mut raw = Vec::new();
            zstd::stream::read::Decoder::new(&b[..])?.take(MAX + 1).read_to_end(&mut raw).context("not a zstd-compressed .btrdb")?;
            raw
        };
        ensure!(raw.starts_with(b"BTRDB\0\0\0") && raw.len() as u64 <= MAX, "{}: not a .btrdb", p.display());
        return Ok(raw);
    }
    ensure!(b.len() >= HDR, "{}: not a btrpkg file", p.display());
    Header::decode(&b[..HDR])?;
    Ok(b[..HDR].to_vec())
}

const USAGE: &str = "usage:
  btrpkg convert [--key NAME.sec] [--parts REL] [--small N] IN.pkg.tar.* STORE
      (writes STORE/NAME.btrpkg, its .sig with --key, and the btrparts into REL (default
       pkg), relative to STORE)
  btrpkg install [--copy] CACHE ROOT
  btrpkg info FILE
  btrpkg dump FILE                        (the parsed index as text, to compare with other readers)
  btrpkg keygen NAME                      (writes NAME.sec and NAME.pub)
  btrpkg sign --key NAME.sec FILE         (writes FILE.sig: over the index header of a .btrpkg or
                                          .pkg.btr, over the whole file of a .btrdb)
  btrpkg verify [--keys DIR]... FILE      (checks FILE.sig the same way)
  btrpkg pull [TRUST] [--no-encoded] [--verify-data] [--no-fsync] [--h2c] [--parallel N] [--installed ROOT] [--parts REL] DIR|URL NAME CACHE
  btrpkg pull-ids [--h2c] [--parallel N] [--installed ROOT] DIR|URL CACHEDIR NAME=IDHEX...
      (for package managers: every NAME's index must have the header ID IDHEX; all packages share
       one download queue; writes CACHEDIR/NAME.pkg.btr, and NAME.pkg.btr.sig when the server
       has a signature; one syncfs at the end)
  btrpkg fetch-ids [--landlock] [--progress] [--installed ROOT] [--cachedir DIR] [--want FILE | --index-only [--skip-missing]] [--parts REL] --server URL... DIR NAME=IDHEX...
      (the unprivileged, network half of pull-ids: indexes and the chunks that can't come from
       the machine into the directory store DIR; servers tried in order; --landlock: write only
       to DIR, run nothing. The other half: pull-ids [--missing-to FILE] [--consume] ... DIR CACHEDIR ...,
       which exits 3 and lists the chunks DIR lacks if a local file didn't check out, and with
       --consume removes DIR after success. For package managers: fetch-ids --index-only (with
       --skip-missing a package no server has an index for is left out: no DIR/NAME.btrpkg,
       the caller downloads its package file instead), then
       pull-ids --fetch-via FILE, which hashes and reuses local data, lists exactly the missing
       chunks in FILE, prints fetch N BYTES and waits for go on stdin while the caller runs
       fetch-ids --want FILE. pull-ids --plan checks local data the same way, writes nothing and
       prints need NAME CHUNK_BYTES INDEX_BYTES per package: the download size)
  btrpkg cache-clean [--keep N] [--dry-run] CACHEDIR
      (keep the newest N versions (default 3) of every package's NAME.pkg.btr cache file, like
       paccache -rk N)
  btrpkg gc [--keep N] [--min-age SECS] [--dry-run] PARTS
       repository side (PARTS: the btrparts directory, e.g. omarchy/pkg): in every repository
       under PARTS's parent (they share it), keep each
       package's current version, anything newer, and N-1 older ones (default 2: current +
       previous); remove the other indexes and package files, then the btrparts no kept index
       names. Nothing younger than --min-age (default 3600) goes; busy mirror locks stop it.
  btrpkg upgrade [--config FILE] [--root DIR] [--keys DIR]... [--no-snapshot] [--dry-run] [--list] [--h2c] [PKG...]
      (the same as: pacbtr [options] [PKG...])
  btrpkg repo-update [--key NAME.sec] [--gpg-keyring FILE | --no-gpg] [--drop-missing] [--keep-going] [--parts REL] [--db-in FILE] [--jobs N] [--small N] DIR REPO
NET (pull, pull-ids, upgrade): --http h1|h2|h3 (h2 over http:// is h2c), --cacert FILE
TRUST: --id HEX (expected header ID) | --sig FILE [--keys DIR]... (default: the store's
       NAME.btrpkg.sig, keys from /etc/btrpkg/keys and /usr/share/btrpkg/keys) | --unverified";

/// `pull-ids`: what libalpm runs to download btrpkg packages. Trust comes from the caller (the
/// header IDs in its database); signatures are saved for the caller to check.
fn pull_ids(s: &str, parts: &str, cdir: &Path, pkgs: &[&str], flags: &[&str], parallel: Option<usize>, installed: Option<&Path>, fetch_via: Option<&Path>) -> Result<()> {
    ensure!(!pkgs.is_empty(), "pull-ids needs at least one NAME=IDHEX");
    let src = net::Source::parse(s, flags.contains(&"--h2c"), parallel);
    let mut ns = net::NetStats::default();
    let mut got: Vec<(String, Vec<u8>, Option<String>, PathBuf, sign::Trust)> = Vec::new();
    let ids: Vec<(String, Hash)> = pkgs.iter().map(|p| parse_name_id(p)).collect::<Result<_>>()?;
    let names: Vec<&str> = ids.iter().map(|(n, _)| n.as_str()).collect();
    for ((name, id), r) in ids.iter().zip(store::fetch_indexes(&src, &names, &mut ns)) {
        let (ib, sig) = r?;
        got.push((name.clone(), ib, sig, cdir.join(store::cache_rel(name)), sign::Trust::Id(*id)));
    }
    let o = store::PullOpts { use_encoded: !flags.contains(&"--no-encoded"), installed,
        verify_data: flags.contains(&"--verify-data"), fsync: false, before_fetch: None, plan_only: flags.contains(&"--plan"), progress: flags.contains(&"--progress"), parts };
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
    let st = store::pull_many(&src.parts(parts)?, &items, &o, ns)?;
    if o.plan_only {
        // what a real run would download: chunk bytes, then the index's size
        for ((n, ib, ..), st) in got.iter().zip(&st) { println!("need\t{n}\t{}\t{}", st.fetched_bytes, ib.len()); }
        return Ok(());
    }
    for ((n, _, sig, c, _), st) in got.iter().zip(&st) {
        let sp = repo::part_sig(c);
        match sig {
            Some(text) => store::write_atomic_nosync(&sp, text.as_bytes())?,
            None => { let _ = fs::remove_file(&sp); }
        }
        println!("download\t{n}\t{}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes, st.installed_chunks);
    }
    store::syncfs_dir(cdir)
}

/// NAME=IDHEX (pull-ids, fetch-ids): a package file name that stays inside the directories it
/// names files in, and its database ID.
fn parse_name_id(p: &str) -> Result<(String, Hash)> {
    let (name, id) = p.split_once('=').with_context(|| format!("{p}: want NAME=IDHEX"))?;
    ensure!(!name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\0']), "{name}: bad package file name");
    Ok((name.to_string(), store::unhex(id)?))
}

/// A btrparts directory REL (relative) resolved against store directory STORE, as clients
/// resolve it (net::Source::parts).
fn parts_dir(store: &Path, rel: &str) -> Result<PathBuf> {
    match net::Source::Dir(store.to_path_buf()).parts(rel)? { net::Source::Dir(p) => Ok(p), _ => unreachable!() }
}

/// The parsed and checked index of FILE (an index or cache file) as text: one line per
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
    let mut small_max = 4096u64;
    let mut key: Option<PathBuf> = None;
    let mut gpg_keyring: Option<PathBuf> = Some("/etc/pacman.d/gnupg/pubring.gpg".into());
    let mut db_in: Option<PathBuf> = None;
    let mut jobs = 4usize;
    let mut parallel: Option<usize> = None;
    let mut installed: Option<PathBuf> = None;
    let mut netopt = net::NetOptions::default();
    let mut servers: Vec<String> = Vec::new();
    let mut parts: Option<String> = None;
    let mut cachedir: Option<PathBuf> = None;
    let mut want: Option<PathBuf> = None;
    let mut missing_to: Option<PathBuf> = None;
    let mut keep: usize = 3;
    let mut keep_given = false;
    let mut min_age: u64 = 3600;
    let mut fetch_via: Option<PathBuf> = None;
    let mut trust = TrustArgs { id: None, sig: None, keys: vec![], unverified: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().with_context(|| format!("{n} needs a value")).cloned();
        match a.as_str() {
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
            "--parts" => parts = Some(val("--parts")?),
            "--keep" => { keep = val("--keep")?.parse()?; keep_given = true; }
            "--min-age" => min_age = val("--min-age")?.parse()?,
            "--fetch-via" => fetch_via = Some(val("--fetch-via")?.into()),
            "--cachedir" => cachedir = Some(val("--cachedir")?.into()),
            "--want" => want = Some(val("--want")?.into()),
            "--missing-to" => missing_to = Some(val("--missing-to")?.into()),
            "--cacert" => netopt.cainfo = Some(val("--cacert")?),
            "--config" => conf = val("--config")?.into(),
            "--root" => root = val("--root")?.into(),
            "-h" | "--help" => { println!("{USAGE}"); return Ok(()); }
            "--keep-going" | "--no-encoded" | "--copy" | "--h2c" | "--drop-missing" | "--no-snapshot" | "--dry-run" | "--list" | "--verify-data" | "--no-fsync" | "--progress" | "--landlock" | "--consume" | "--index-only" | "--skip-missing" | "--plan" => flags.push(a.as_str()),
            s if s.starts_with('-') && s.len() > 1 => bail!("unknown option {s}\n{USAGE}"),
            s => pos.push(s),
        }
    }
    net::set_options(netopt)?;
    match pos.as_slice() {
        ["convert", i, s] => {
            let k = sec_key(key.as_ref())?;
            let pdir = parts_dir(Path::new(s), parts.as_deref().unwrap_or(net::DEFAULT_PARTS))?;
            convert(Path::new(i), Path::new(s), &pdir, k.as_ref().map(|(k, n)| (k, n.as_str())), &store::raw_map(&pdir), small_max).map(|(name, st)| {
                eprintln!("convert: {st:?}");
                println!("{name}\t{} btrparts, {} new ({} bytes), index {} bytes", st.chunks, st.new_chunks, st.new_bytes, st.index_bytes);
            })
        }
        ["install", c, r] => install(Path::new(c), Path::new(r), flags.contains(&"--copy")).map(|st| eprintln!("install: {st:?}")),
        ["info", p] => info(Path::new(p)),
        ["dump", p] => dump(Path::new(p)),
        ["keygen", n] => sign::keygen(Path::new(n)).map(|k| println!("key {} -> {n}.sec, {n}.pub", sign::hex8(&k))),
        ["sign", f] => {
            let (k, name) = sec_key(key.as_ref())?.context("sign needs --key NAME.sec")?;
            let msg = signed_message(Path::new(f))?;
            let sp = format!("{f}.sig");
            fs::write(&sp, k.sign(&msg, &name))?;
            println!("{sp}");
            Ok(())
        }
        ["verify", f] => {
            let dirs: Vec<PathBuf> = if trust.keys.is_empty() { sign::DEFAULT_KEY_DIRS.iter().map(PathBuf::from).collect() } else { trust.keys.clone() };
            let sig = fs::read_to_string(format!("{f}.sig")).with_context(|| format!("{f}.sig"))?;
            let kf = sign::Keyring::load(&dirs)?.verify(&signed_message(Path::new(f))?, &sig)?.to_path_buf();
            println!("{f}: good signature by {}", kf.display());
            Ok(())
        }
        ["pull", s, n, c] => {
            let src = net::Source::parse(s, flags.contains(&"--h2c"), parallel);
            let mut ns = net::NetStats::default();
            let (ib, sig) = store::fetch_index(&src, n, &mut ns)?;
            let t = trust.resolve(sig)?;
            let o = store::PullOpts { use_encoded: !flags.contains(&"--no-encoded"), installed: installed.as_deref(),
                verify_data: flags.contains(&"--verify-data"), fsync: !flags.contains(&"--no-fsync"), before_fetch: None, plan_only: false, progress: false,
                parts: parts.as_deref().unwrap_or(net::DEFAULT_PARTS) };
            store::pull_index(&src, n, &ib, Path::new(c), &t, &o, ns).map(|st| {
                eprintln!("pull: {st:?}");
                println!("download\t{n}\t{}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes, st.installed_chunks);
            })
        }
        ["pull-ids", s, cdir, pkgs @ ..] => match pull_ids(s, parts.as_deref().unwrap_or(net::DEFAULT_PARTS), Path::new(cdir), pkgs, &flags, parallel, installed.as_deref(), fetch_via.as_deref()) {
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
        ["gc", p] => {
            let o = gc::GcOpts { keep: if keep_given { keep } else { 2 }, min_age: std::time::Duration::from_secs(min_age), dry_run: flags.contains(&"--dry-run") };
            gc::gc(Path::new(p), &o).map(|st| eprintln!("gc{}: {} repositories; indexes {} kept, {} removed ({} package files); btrparts {} kept, {} removed, {} too young to remove; {:.1} MiB",
                if o.dry_run { " (dry run)" } else { "" }, st.repos, st.indexes_kept, st.indexes_removed, st.pkg_files_removed,
                st.parts_kept, st.parts_removed, st.parts_young, st.bytes_removed as f64 / 1048576.0))
        }
        ["cache-clean", d] => cache::cache_clean(Path::new(d), keep, flags.contains(&"--dry-run")).map(|st| eprintln!("cache-clean: {st:?}")),
        ["fetch-ids", dir, pkgs @ ..] => {
            let dir = Path::new(dir);
            if flags.contains(&"--landlock") { fetchids::landlock_writes_to(dir)?; }
            ensure!(!servers.is_empty(), "fetch-ids needs --server URL (repeat for more, tried in order)");
            let srcs: Vec<net::Source> = servers.iter().map(|s| net::Source::parse(s, flags.contains(&"--h2c"), parallel)).collect();
            let ids = pkgs.iter().map(|p| parse_name_id(p)).collect::<Result<Vec<_>>>()?;
            let w = match &want { Some(p) => Some(fs::read_to_string(p)?.lines().filter(|l| !l.is_empty()).map(String::from).collect()), None => None };
            let o = fetchids::FetchOpts { installed: installed.as_deref(), cachedir: cachedir.as_deref(), want: w, progress: flags.contains(&"--progress"),
                index_only: flags.contains(&"--index-only"), skip_missing: flags.contains(&"--skip-missing"), parts: parts.as_deref().unwrap_or(net::DEFAULT_PARTS) };
            fetchids::fetch_ids(&srcs, dir, &ids, &o)
        }
        ["repo-update", d, r] => {
            let k = sec_key(key.as_ref())?;
            let o = repo::RepoOpts { key: k.as_ref().map(|(k, n)| (k, n.as_str())), gpg_keyring, drop_missing: flags.contains(&"--drop-missing"),
                small: small_max, jobs, db_in: db_in.clone(), parts: parts.clone().unwrap_or_else(|| repo::DEFAULT_REPO_PARTS.to_string()) };
            let st = repo::repo_update(Path::new(d), r, &o)?;
            eprintln!("repo-update: {st:?}");
            ensure!(st.failed == 0 || flags.contains(&"--keep-going"), "{} packages failed (left to pacman)", st.failed);
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
        store::pull(&net::Source::Dir(store.to_path_buf()), name, cache, trust, &store::PullOpts::default())
    }

    /// btrfs at P? Encoded writes and reflinks need it; CI runners (overlayfs on ext4) and a
    /// tmpfs /tmp don't have it, and making one there would need mkfs.
    fn on_btrfs(p: &Path) -> bool {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        unsafe { libc::statfs(c.as_ptr(), &mut st) == 0 && st.f_type as i64 == 0x9123683E }
    }
    macro_rules! needs_btrfs {
        ($p:expr) => {
            if !on_btrfs($p) { eprintln!("skipped: needs btrfs ({} isn't)", $p.display()); return; }
        };
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

    /// Convert SRC into STORE (btrparts in PARTS); the package's name.
    fn conv_parts(src: &Path, store: &Path, parts: &Path) -> (String, store::PublishStats) {
        convert(src, store, parts, None, &store::raw_map(parts), 4096).unwrap()
    }
    /// Convert SRC into STORE, btrparts inside it.
    fn conv(src: &Path, store: &Path) -> store::PublishStats {
        conv_parts(src, store, &store.join(net::DEFAULT_PARTS)).1
    }
    fn part(store: &Path, h: &Hash) -> PathBuf {
        let x = hex(h);
        store.join(net::DEFAULT_PARTS).join(&x[..2]).join(&x)
    }

    #[test]
    fn data_len_stops_at_the_padding() {
        // every raw btrpart: the bytes after data_len are zeros, the bytes before reach the end
        // of the file data; zstd btrparts are whole
        let t = TempDir::new("datalen");
        // libx.so.1: random bytes of an odd length, so its last piece stays raw and padded
        let mut x = 0x9e3779b97f4a7c15u64;
        let random: Vec<u8> = (0..200_123).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; (x >> 24) as u8 }).collect();
        let src = make_pkg(&t.0, "d-1-1", &Spec { big: data(300_000, 7), other: random });
        let store = t.0.join("store");
        conv(&src, &store);
        let idx = parse_index(&fs::read(store.join("d-1-1.btrpkg")).unwrap()).unwrap();
        let files = store::chunk_files(&idx);
        let (mut raw_tails, mut shorter) = (0, 0); // raw btrparts that end in padding
        for c in &idx.chunks {
            let d = &fs::read(part(&store, &c.stored)).unwrap();
            let n = store::data_len(c, &files);
            if c.compression == COMP_ZSTD { assert_eq!(n, c.encoded_len as usize); continue; }
            assert!(n >= 1 && n <= d.len());
            assert!(d[n..].iter().all(|&x| x == 0), "nonzero byte after data_len");
            if n < d.len() { shorter += 1; }
            if n < c.encoded_len as usize { raw_tails += 1; }
        }
        assert!(raw_tails > 0 && shorter == raw_tails, "no raw tail ({raw_tails}, {shorter})");
        // libx.so.1's tail: 200_123 - 131_072 = 69_051 data bytes of a 69_632-byte btrpart
        assert!(idx.chunks.iter().any(|c| c.compression == COMP_NONE && store::data_len(c, &files) == 69_051 && c.encoded_len == 69_632));
    }

    #[test]
    fn gc_shared_btrparts() {
        // two repositories share ../pkg; p has versions 1, 2, 3 in x86_64 (db names 3) and
        // version 1 in aarch64 (db names 1): with --keep 2 only x86_64's p-1 goes, and only the
        // btrparts no kept index of either repository names
        let t = TempDir::new("gc");
        let root = t.0.join("r");
        let (x, a) = (root.join("x86_64"), root.join("aarch64"));
        fs::create_dir_all(&x).unwrap();
        fs::create_dir_all(&a).unwrap();
        let parts = root.join("pkg");
        fs::create_dir_all(&parts).unwrap();
        // versions differ in "other" (seed); "big" is shared by all of them
        let mut stems = vec![];
        for (dir, v, seed) in [(&x, "1-1", 11u64), (&x, "2-1", 12), (&x, "3-1", 13), (&a, "1-1", 11)] {
            let stem = format!("p-{v}-x86_64");
            let src = make_pkg(dir, &stem, &spec(seed));
            conv_parts(&src, dir, &parts);
            stems.push((dir.to_path_buf(), stem));
        }
        let db = |dir: &Path, cur: &str| {
            let desc = format!("%FILENAME%\n{cur}.pkg.tar.zst\n\n%NAME%\np\n\n%DATA%\nbtrpkg.index={cur}.btrpkg\nbtrpkg.parts=../pkg\n\n");
            let mut tb = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_path("p/desc").unwrap(); h.set_mode(0o644); h.set_size(desc.len() as u64); h.set_cksum();
            tb.append(&h, desc.as_bytes()).unwrap();
            fs::write(dir.join("t.db"), tb.into_inner().unwrap()).unwrap();
        };
        db(&x, "p-3-1-x86_64");
        db(&a, "p-1-1-x86_64");
        let names = |idx: &Path| -> std::collections::HashSet<Hash> { parse_index(&fs::read(idx).unwrap()).unwrap().chunks.iter().map(|c| c.stored).collect() };
        let ix = |d: &Path, s: &str| d.join(store::index_rel(s));
        let p1 = names(&ix(&x, "p-1-1-x86_64"));
        let p2 = names(&ix(&x, "p-2-1-x86_64"));
        let p3 = names(&ix(&x, "p-3-1-x86_64"));
        let count = || walk_files(&parts).len();
        let before = count();
        // too young: nothing goes
        let o = gc::GcOpts { keep: 2, min_age: std::time::Duration::from_secs(3600), dry_run: false };
        let st = gc::gc(&parts, &o).unwrap();
        assert_eq!((st.repos, st.indexes_removed, st.parts_removed), (2, 0, 0), "{st:?}");
        let o = gc::GcOpts { min_age: std::time::Duration::ZERO, ..o };
        let dry = gc::gc(&parts, &gc::GcOpts { dry_run: true, ..o }).unwrap();
        assert_eq!(count(), before);
        let st = gc::gc(&parts, &o).unwrap();
        assert_eq!((dry.indexes_removed, dry.parts_removed), (st.indexes_removed, st.parts_removed));
        assert_eq!((st.indexes_removed, st.pkg_files_removed), (1, 1), "{st:?}");
        assert!(!ix(&x, "p-1-1-x86_64").exists() && !x.join("p-1-1-x86_64.pkg.tar.zst").exists());
        assert!(ix(&x, "p-2-1-x86_64").exists() && ix(&x, "p-3-1-x86_64").exists() && ix(&a, "p-1-1-x86_64").exists());
        // p-1's btrparts all stay: aarch64's current p-1 names the same ones
        let has = |h: &Hash| { let s = hex(h); parts.join(&s[..2]).join(&s).exists() };
        assert!(p1.iter().chain(&p2).chain(&p3).all(has));
        assert_eq!(st.parts_removed, 0, "{st:?}");
        // aarch64 moves to 3: its p-1 is previous now, still kept; at --keep 1 it goes, and with
        // it the btrparts only p-1 had
        db(&a, "p-3-1-x86_64");
        let src = make_pkg(&a, "p-3-1-x86_64", &spec(13));
        conv_parts(&src, &a, &parts);
        let st = gc::gc(&parts, &gc::GcOpts { keep: 1, ..o }).unwrap();
        assert_eq!(st.indexes_removed, 2, "{st:?}"); // x86_64 p-2, aarch64 p-1
        let only1: Vec<&Hash> = p1.iter().filter(|h| !p3.contains(*h)).collect();
        assert!(!only1.is_empty() && only1.iter().all(|h| !has(h)));
        assert!(p3.iter().all(has));
        assert!(st.parts_removed >= only1.len(), "{st:?}");
        // an unreadable index stops the run before anything goes
        fs::write(ix(&x, "p-0-1-x86_64"), b"junk").unwrap();
        fs::write(ix(&x, "p-9-1-x86_64"), b"junk").unwrap();
        assert!(gc::gc(&parts, &o).is_err());
    }

    fn walk_files(d: &Path) -> Vec<PathBuf> {
        let mut v = vec![];
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { v.extend(walk_files(&p)); } else { v.push(p); }
        }
        v
    }

    #[test]
    fn btrparts_paths() {
        let base = |s: net::Source| match s { net::Source::Http { base, .. } => base, net::Source::Dir(d) => d.display().to_string() };
        let h = net::Source::parse("http://h:1/omarchy/x86_64/", false, None);
        assert_eq!(base(h.parts("../pkg").unwrap()), "http://h:1/omarchy/pkg");
        assert_eq!(base(h.parts("pkg").unwrap()), "http://h:1/omarchy/x86_64/pkg");
        assert_eq!(base(h.parts("./a/../pkg/").unwrap()), "http://h:1/omarchy/x86_64/pkg");
        let a = net::Source::parse("https://m/arch/core/os/x86_64", false, None);
        assert_eq!(base(a.parts("../../../pkg").unwrap()), "https://m/arch/pkg");
        for bad in ["../../../x", "/abs", "http://evil/x", "", "a?b", "a#b", "a%2e", "a b", "a\\b"] {
            assert!(h.parts(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(base(net::Source::parse("file:///srv/repo/x86_64", false, None).parts("../pkg").unwrap()), "/srv/repo/pkg");
    }

    #[test]
    fn shared_btrparts() {
        // two architecture directories, one btrparts directory beside them: the second publish
        // of the same package adds no btrpart, and both pull through ../pkg
        let t = TempDir::new("shared");
        let src = make_pkg(&t.0, "s-1-1", &spec(3));
        let (x86, arm) = (t.0.join("repo/x86_64"), t.0.join("repo/aarch64"));
        fs::create_dir_all(&x86).unwrap();
        fs::create_dir_all(&arm).unwrap();
        let parts = parts_dir(&x86, "../pkg").unwrap();
        assert_eq!(parts, t.0.join("repo/pkg"));
        let (_, a) = conv_parts(&src, &x86, &parts);
        let (_, b) = conv_parts(&src, &arm, &parts_dir(&arm, "../pkg").unwrap());
        assert!(a.new_chunks > 0 && b.new_chunks == 0, "{a:?} {b:?}");
        assert!(!x86.join("pkg").exists() && !arm.join("pkg").exists());
        // the repository format version beside the shared btrparts directory, no cache-file names
        assert_eq!(fs::read_to_string(t.0.join("repo/version")).unwrap(), "0\n");
        assert!(x86.join("s-1-1.btrpkg").is_file() && !x86.join("s-1-1.pkg.btr").exists());
        let o = store::PullOpts { parts: "../pkg", ..Default::default() };
        for d in [&x86, &arm] {
            let c = t.0.join(format!("c-{}.pkg.btr", d.file_name().unwrap().to_string_lossy()));
            let st = store::pull(&net::Source::Dir(d.to_path_buf()), "s-1-1", &c, &U, &o).unwrap();
            assert!(st.fetched_chunks > 0, "{st:?}");
            let root = t.0.join(format!("root-{}", d.file_name().unwrap().to_string_lossy()));
            install(&c, &root, false).unwrap();
        }
        // without the field's path the btrparts aren't found
        let o2 = store::PullOpts { parts: net::DEFAULT_PARTS, ..o };
        assert!(store::pull(&net::Source::Dir(arm.clone()), "s-1-1", &t.0.join("x.pkg.btr"), &U, &o2).is_err());
    }

    #[test]
    fn convert_pull_install() {
        let t = TempDir::new("cpi");
        let s = spec(1);
        let src = make_pkg(&t.0, "p-1-1", &s);
        let store = t.0.join("store");
        let st = conv(&src, &store);
        assert!(st.chunks == 5 && st.new_chunks == 5, "{st:?}"); // tool (3) and libx.so.1 (2); tool-copy is deduplicated
        let cache = t.0.join("p.cache");
        let p = pull_dir(&store, "p-1-1", &cache, &U).unwrap();
        assert!(p.fetched_chunks > 0);
        let root = t.0.join("root");
        let ist = install(&cache, &root, false).unwrap();
        assert_eq!((ist.files, ist.hardlinks, ist.symlinks, ist.dirs), (6, 1, 1, 4));
        check_tree(&root, &s);
        // the large file is deduplicated: one copy in the image
        let idx = parse_index(&fs::read(store.join("p-1-1.btrpkg")).unwrap()).unwrap();
        assert_eq!(idx.hdr.image_len, align(300_000) + align(200_000));
        // every btrpart is level 12 or raw: framing again at 12 gives the same bytes
        for c in idx.chunks.iter().filter(|c| c.compression == COMP_ZSTD) {
            let d = fs::read(part(&store, &c.stored)).unwrap();
            let plain = zstd::bulk::decompress(&d, c.unencoded_len as usize).unwrap();
            assert_eq!(frame(plain).unwrap().1, d);
        }
        // install refuses anything that isn't a cache file
        assert!(install(&store.join("p-1-1.btrpkg"), &t.0.join("root-x"), false).is_err());
        // convert wants a package file name
        assert!(convert(&t.0.join("p.tar"), &store, &store.join("pkg"), None, &store::RawMap::new(), 4096).is_err());
    }

    #[test]
    fn corrupt_store_leaves_nothing() {
        let t = TempDir::new("corrupt");
        let src = make_pkg(&t.0, "p-1-1", &spec(2));
        let store = t.0.join("store");
        conv(&src, &store);
        let ip = store.join("p-1-1.btrpkg");
        let b = fs::read(&ip).unwrap();
        let idx = parse_index(&b).unwrap();
        let h = &idx.hdr;
        let t0 = HDR as u64;
        let e0 = t0 + h.table_len();
        let b0 = e0 + h.entries_len;
        let cache = t.0.join("c.pkg.btr");
        let check = |what: &str| {
            assert!(pull_dir(&store, "p-1-1", &cache, &U).is_err(), "{what}: accepted");
            assert!(!cache.exists() && !part_path(&cache).exists(), "{what}: left a file");
        };
        for (what, pos) in [("header", 20), ("header hash", 230), ("table", t0 + 3), ("table hash field", t0 + 30), ("entries", e0 + 5),
                            ("blob", b0 + 2), ("last byte", b.len() as u64 - 1)] {
            let mut c = b.clone();
            c[pos as usize] ^= 0x40;
            fs::write(&ip, &c).unwrap();
            check(what);
        }
        fs::write(&ip, &b[..b.len() - 1]).unwrap();
        check("truncated index");
        fs::write(&ip, &b).unwrap();
        for (what, c) in [("first btrpart", idx.chunks[0]), ("last btrpart", idx.chunks[idx.chunks.len() - 1])] {
            let p = part(&store, &c.stored);
            let good = fs::read(&p).unwrap();
            let mut bad = good.clone();
            bad[good.len() / 2] ^= 1;
            fs::write(&p, &bad).unwrap();
            check(what);
            fs::write(&p, &good[..good.len() - 1]).unwrap();
            check(&format!("{what} truncated"));
            fs::write(&p, &good).unwrap();
        }
        pull_dir(&store, "p-1-1", &cache, &U).unwrap();
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
            assert!(convert(&src, &t.0.join("store"), &t.0.join("store/pkg"), None, &store::RawMap::new(), 4096).is_err(), "{n}: converted");
        }
        // through symlinks already on disk: refused by install, nothing outside changed, no
        // temporary file left
        let ok = make_tar(&t.0, "ok", &[("usr", D, 0o755, b"", None), ("usr/evil", D, 0o777, b"", None),
            ("usr/evil/x", F, 0o644, &[1u8; 9000], None), ("usr/y", F, 0o644, b"y", None)]);
        let store = t.0.join("store");
        conv(&ok, &store);
        let cache = t.0.join("ok.cache");
        pull_dir(&store, "ok", &cache, &U).unwrap();
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
        needs_btrfs!(&t.0);
        let src = make_pkg(&t.0, "w-1-1", &spec(10));
        let store = t.0.join("store");
        conv(&src, &store);
        let ip = store.join("w-1-1.btrpkg");
        let idx = parse_index(&fs::read(&ip).unwrap()).unwrap();
        // a self-consistent index (every section hash right) whose table claims other data for a chunk
        let i = idx.chunks.iter().position(|c| c.compression == COMP_ZSTD).unwrap();
        let mut chunks = idx.chunks.clone();
        chunks[i].raw[0] ^= 1;
        fs::write(&ip, idx.with_chunks(&chunks)).unwrap();
        let cdir = t.0.join("cdir");
        fs::create_dir_all(&cdir).unwrap();
        // --verify-data decodes every chunk: refused, nothing left
        let full = store::PullOpts { verify_data: true, ..Default::default() };
        let c = cdir.join("w-1-1.pkg.btr");
        let e = store::pull(&net::Source::Dir(store.clone()), "w-1-1", &c, &U, &full).unwrap_err();
        assert!(format!("{e:#}").contains("unencoded hash"), "{e:#}");
        assert!(!c.exists() && !part_path(&c).exists());
        // the default pull checks what the signed stored hash covers, so it takes the chunk; the
        // wrong unencoded hash can't be used for reuse later: reuse reads the data back
        pull_dir(&store, "w-1-1", &c, &U).unwrap();
        let src2 = make_pkg(&t.0, "w-1-2", &spec(10));
        conv(&src2, &store);
        let p = pull_dir(&store, "w-1-2", &cdir.join("w-1-2.pkg.btr"), &U).unwrap();
        assert!(p.bad_local == 0 && p.reused_chunks == (idx.chunks.len() - 1) as u64, "{p:?}");
        let root = t.0.join("root");
        install(&cdir.join("w-1-2.pkg.btr"), &root, false).unwrap();
        check_tree(&root, &spec(10));
    }

    #[test]
    fn sign_btrdb() {
        // a .btrdb is signed whole (decompressed); anything else named .btrdb is refused
        let t = TempDir::new("signdb");
        sign::keygen(&t.0.join("repo")).unwrap();
        let k = sign::SecretKey::load(&t.0.join("repo.sec")).unwrap();
        let keys = t.0.join("keys");
        fs::create_dir_all(&keys).unwrap();
        fs::copy(t.0.join("repo.pub"), keys.join("repo.pub")).unwrap();
        let db = t.0.join("x.btrdb");
        fs::write(&db, zstd::encode_all(&b"BTRDB\0\0\0\0\0\0\0\0\0\0\0"[..], 3).unwrap()).unwrap();
        let msg = signed_message(&db).unwrap();
        assert_eq!(msg, b"BTRDB\0\0\0\0\0\0\0\0\0\0\0");
        // the same index decompressed has the same signed message
        let rawp = t.0.join("raw.btrdb");
        fs::write(&rawp, &msg).unwrap();
        assert_eq!(signed_message(&rawp).unwrap(), msg);
        let sig = k.sign(&msg, "repo");
        let ring = sign::Keyring::load(&[keys]).unwrap();
        assert!(ring.verify(&signed_message(&db).unwrap(), &sig).is_ok());
        let mut b = fs::read(&db).unwrap();
        let n = b.len();
        b[n - 1] ^= 1;
        fs::write(&db, &b).unwrap();
        assert!(signed_message(&db).is_err() || ring.verify(&signed_message(&db).unwrap(), &sig).is_err());
        fs::write(&db, zstd::encode_all(&b"BTRPKG\0\0"[..], 3).unwrap()).unwrap();
        assert!(signed_message(&db).is_err());
    }

    #[test]
    fn signed_store() {
        let t = TempDir::new("signed");
        let store = t.0.join("store");
        let src = make_pkg(&t.0, "s-1-1", &spec(8));
        sign::keygen(&t.0.join("repo")).unwrap();
        sign::keygen(&t.0.join("other")).unwrap();
        let k = sign::SecretKey::load(&t.0.join("repo.sec")).unwrap();
        convert(&src, &store, &store.join(net::DEFAULT_PARTS), Some((&k, "repo")), &store::RawMap::new(), 4096).unwrap();
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
        let ib = fs::read(store.join("s-1-1.btrpkg")).unwrap();
        assert!(pull_dir(&store, "s-1-1", &cdir.join("b.pkg.btr"), &trust(other.sign(&ib[..HDR], "other"))).is_err());
        // a tampered index (re-hashed, so only the signature can tell)
        let idx = parse_index(&ib).unwrap();
        let mut ch = idx.chunks.clone();
        ch[0].raw[1] ^= 1;
        fs::write(store.join("s-1-1.btrpkg"), idx.with_chunks(&ch)).unwrap();
        assert!(pull_dir(&store, "s-1-1", &cdir.join("c.pkg.btr"), &trust(sig)).is_err());
        // nothing but the first cache file
        assert_eq!(fs::read_dir(&cdir).unwrap().count(), 1);
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
        let o = repo::RepoOpts { key: Some((&k, "k")), gpg_keyring: None, drop_missing: true, small: 4096, jobs: 2, db_in: None, parts: "pkg".into() };
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
        assert!(q.contains("%DATA%\nother.key=1\nbtrpkg.index=q-2-1-x86_64.btrpkg\nbtrpkg.id="), "{q}");
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
            conv(&src, &store);
        }
        let src = net::Source::Dir(store.clone());
        let mut ns = net::NetStats::default();
        let ibs: Vec<Vec<u8>> = names.iter().map(|n| store::fetch_index(&src, n, &mut ns).unwrap().0).collect();
        let cdir = t.0.join("cache");
        fs::create_dir_all(&cdir).unwrap();
        let caches: Vec<PathBuf> = names.iter().map(|n| cdir.join(format!("{n}.pkg.btr"))).collect();
        let o = store::PullOpts::default();
        let items = |caches: &[PathBuf]| -> Vec<(String, Vec<u8>, PathBuf)> {
            names.iter().zip(&ibs).zip(caches).map(|((n, ib), c)| (n.to_string(), ib.clone(), c.clone())).collect()
        };
        let run = |v: &[(String, Vec<u8>, PathBuf)]| {
            let it: Vec<store::PullItem> = v.iter().map(|(n, ib, c)| store::PullItem { name: n, ib, cache: c, trust: &U }).collect();
            store::pull_many(&src.parts(net::DEFAULT_PARTS).unwrap(), &it, &o, net::NetStats::default())
        };
        // a chunk only b has, damaged on the server: nothing of either package is left behind
        let idx_a = read_index_checked(&mut &ibs[0][..], &|h| U.check(h)).unwrap();
        let idx_b = read_index_checked(&mut &ibs[1][..], &|h| U.check(h)).unwrap();
        let only_b = idx_b.chunks.iter().find(|c| !idx_a.chunks.iter().any(|a| a.stored == c.stored)).unwrap();
        let x = hex(&only_b.stored);
        let cp = store.join(format!("pkg/{}/{x}", &x[..2]));
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

    fn other_cache(t: &Path) -> PathBuf {
        let d = t.join("other2");
        fs::create_dir_all(&d).unwrap();
        d.join("q-1-1-x86_64.pkg.btr")
    }

    #[test]
    fn pull_reuses_installed_files() {
        let t = TempDir::new("installed");
        let store = t.0.join("store");
        let (s1, s2) = (spec(3), spec(4)); // same big file, other library
        for (n, s) in [("q-1-1-x86_64", &s1), ("q-2-1-x86_64", &s2)] {
            let src = make_pkg(&t.0, n, s);
            conv(&src, &store);
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
        let o = store::PullOpts { installed: Some(&root), ..Default::default() };
        let src = net::Source::Dir(store.clone());
        let p = store::pull(&src, "q-2-1-x86_64", &cdir.join("q-2-1-x86_64.pkg.btr"), &U, &o).unwrap();
        // tool (300 kB, 3 chunks) is unchanged and comes from the installed file
        assert_eq!((p.installed_chunks, p.fetched_chunks, p.reused_chunks), (3, 2, 0), "{p:?}");
        let r2 = t.0.join("root2");
        install(&cdir.join("q-2-1-x86_64.pkg.btr"), &r2, false).unwrap();
        check_tree(&r2, &s2);
        // version 1's tool moved to another path (and its other names are gone): found through
        // pacman's local database by size and name, checked like any installed file
        let rm = t.0.join("root-moved");
        pull_dir(&store, "q-1-1-x86_64", &other_cache(&t.0), &U).unwrap();
        install(&other_cache(&t.0), &rm, false).unwrap();
        fs::create_dir_all(rm.join("usr/libexec")).unwrap();
        fs::rename(rm.join("usr/bin/tool"), rm.join("usr/libexec/tool")).unwrap();
        fs::remove_file(rm.join("usr/bin/tool-copy")).unwrap();
        fs::remove_file(rm.join("usr/bin/hard")).unwrap();
        let om = store::PullOpts { installed: Some(&rm), ..Default::default() };
        let cm = cdir.join("m").join("q-2-1-x86_64.pkg.btr");
        fs::create_dir_all(cm.parent().unwrap()).unwrap();
        let p = store::pull(&src, "q-2-1-x86_64", &cm, &U, &om).unwrap();
        assert_eq!((p.installed_chunks, p.installed_moved, p.fetched_chunks), (0, 0, 5), "no database: {p:?}");
        let db = rm.join("var/lib/pacman/local/q-1-1");
        fs::create_dir_all(&db).unwrap();
        fs::write(db.join("desc"), "%NAME%\nq\n\n%VERSION%\n1-1\n\n").unwrap();
        fs::write(db.join("files"), "%FILES%\nusr/\nusr/bin/\nusr/bin/../../etc/passwd\nusr/lib/libx.so.1\nusr/libexec/tool\nusr/share/readme\n\n%BACKUP%\nusr/share/x\t0\n").unwrap();
        fs::remove_file(&cm).unwrap();
        let p = store::pull(&src, "q-2-1-x86_64", &cm, &U, &om).unwrap();
        assert_eq!((p.installed_chunks, p.installed_moved, p.fetched_chunks), (3, 3, 2), "moved: {p:?}");
        let r4 = t.0.join("root4");
        install(&cm, &r4, false).unwrap();
        check_tree(&r4, &s2);
        // another package's entry with a name that starts the same is not used
        fs::rename(&db, rm.join("var/lib/pacman/local/q-extra-1-1")).unwrap();
        fs::write(rm.join("var/lib/pacman/local/q-extra-1-1/desc"), "%NAME%\nq-extra\n").unwrap();
        fs::remove_file(&cm).unwrap();
        let p = store::pull(&src, "q-2-1-x86_64", &cm, &U, &om).unwrap();
        assert_eq!((p.installed_moved, p.fetched_chunks), (0, 5), "{p:?}");
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
        needs_btrfs!(&t.0);
        let store = t.0.join("store");
        let (s1, s2) = (spec(3), spec(4)); // same big file, other library
        for (n, s) in [("q-1-1", &s1), ("q-2-1", &s2)] {
            let src = make_pkg(&t.0, n, s);
            conv(&src, &store);
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
        let ib = fs::read(store.join("q-2-1.btrpkg")).unwrap();
        let c3 = cdir.join("again.pkg.btr");
        assert!(pull_dir(&store, "q-2-1", &c3, &sign::Trust::Id([0; 32])).is_err());
        assert!(!c3.exists() && !part_path(&c3).exists());
        let id = Header::decode(&ib).unwrap().id();
        pull_dir(&store, "q-2-1", &c3, &sign::Trust::Id(id)).unwrap();

        // a damaged chunk in the store: refused, nothing left behind
        let idx = parse_index(&ib).unwrap();
        let x = hex(&idx.chunks[3].stored);
        let cp = store.join("pkg").join(&x[..2]).join(&x);
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
        fs::write(store.join("bad.btrpkg"), &ib2).unwrap();
        assert!(pull_dir(&store, "bad", &fresh.join("bad.pkg.btr"), &U).is_err());
        assert_eq!(fs::read_dir(&fresh).unwrap().count(), 0);
    }

    #[test]
    fn publish_reuses_chunks_across_compressor_changes() {
        let t = TempDir::new("remap");
        let store = t.0.join("store");
        let s = spec(5);
        let src = make_pkg(&t.0, "r-1-1", &s);
        let a = conv(&src, &store);
        assert_eq!(a.remapped, 0);
        // the same data framed by another compressor (here: level 3): different bytes, same
        // unencoded data
        let parts = store.join(net::DEFAULT_PARTS);
        let reframed = |src: &Path| -> (Vec<u8>, Vec<(Chunk, Vec<u8>)>) {
            let (ib, items) = build(src, 4096).unwrap();
            let items: Vec<(Chunk, Vec<u8>)> = items.into_iter().map(|(c, d)| {
                if c.compression != COMP_ZSTD { return (c, d); }
                let (comp, b) = frame_at(3, zstd::bulk::decompress(&d, c.unencoded_len as usize).unwrap()).unwrap();
                (Chunk { compression: comp, encoded_len: b.len() as u32, stored: if comp == COMP_NONE { c.raw } else { sha256(&b) }, ..c }, b)
            }).collect();
            let chunks: Vec<Chunk> = items.iter().map(|(c, _)| *c).collect();
            (parse_index(&ib).unwrap().with_chunks(&chunks), items)
        };
        let src2 = make_pkg(&t.0, "r-1-2", &s);
        let (ib2, items2) = reframed(&src2);
        assert!(items2.iter().zip(&parse_index(&fs::read(store.join("r-1-1.btrpkg")).unwrap()).unwrap().chunks).any(|((c, _), o)| c.stored != o.stored));
        let b = store::publish(&ib2, items2, "r-1-2", &store, &parts, None, &store::raw_map(&parts)).unwrap();
        assert!(b.remapped > 0, "{b:?}");
        assert_eq!(b.new_chunks, 0, "{b:?}");
        let i1 = parse_index(&fs::read(store.join("r-1-1.btrpkg")).unwrap()).unwrap();
        let i2 = parse_index(&fs::read(store.join("r-1-2.btrpkg")).unwrap()).unwrap();
        assert_eq!(i1.chunks, i2.chunks);
        let c = t.0.join("c");
        fs::create_dir_all(&c).unwrap();
        pull_dir(&store, "r-1-2", &c.join("r.pkg.btr"), &U).unwrap();
        install(&c.join("r.pkg.btr"), &t.0.join("root"), false).unwrap();
        check_tree(&t.0.join("root"), &s);

        // nothing kept on disk for this: the map comes from the indexes
        assert!(!store.join("pkg/.raw").exists());
        // a damaged btrpart is not reused: the next build writes its own (and the index names it)
        let c0 = i1.chunks.iter().find(|c| c.compression != 0).unwrap();
        let h = hex(&c0.stored);
        fs::write(store.join("pkg").join(&h[..2]).join(&h), b"junk").unwrap();
        let src3 = make_pkg(&t.0, "r-1-3", &s);
        let (ib3, items3) = reframed(&src3);
        let p3 = store::publish(&ib3, items3, "r-1-3", &store, &parts, None, &store::raw_map(&parts)).unwrap();
        assert!(p3.new_chunks >= 1, "{p3:?}");
        let i3 = parse_index(&fs::read(store.join("r-1-3.btrpkg")).unwrap()).unwrap();
        let c3 = i3.chunks.iter().find(|x| x.raw == c0.raw).unwrap();
        assert_ne!(c3.stored, c0.stored);
        pull_dir(&store, "r-1-3", &c.join("r3.pkg.btr"), &U).unwrap();
    }
}
