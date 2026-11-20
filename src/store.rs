//! Content-addressed chunk store: `btrpkg publish` and `btrpkg pull`.
//!
//! Store layout (static files, servable by any HTTP server; `pull` reads it as a directory):
//!   chunks/ab/<sha256>   chunk bytes exactly as written to disk (zstd frame or raw),
//!                        named by the SHA-256 of those bytes (the table's stored hash)
//!   raw/ab/<sha256>      "compression stored_len chunk-hash": maps the SHA-256 of a chunk's
//!                        unencoded data to the chunk already stored for it, so a changed
//!                        compressor (different frame bytes) doesn't create new chunks.
//!                        Build server only, not published.
//!   index/NAME.btridx    the package's index (format.rs); its chunk table names the chunks
//!   index/NAME.btridx.sig  signify signature over the index header (sign.rs), when published
//!                        with a key
//! Publish writes chunks, then the signature, then the index, so a published index never refers
//! to a missing chunk or lacks its signature.
use super::*;
use std::os::unix::ffi::OsStrExt;

fn hpath(store: &Path, kind: &str, h: &[u8]) -> PathBuf {
    let x = hex(h);
    store.join(kind).join(&x[..2]).join(x)
}

pub(crate) fn unhex(s: &str) -> Result<Hash> {
    ensure!(s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()), "bad hash {s:?}");
    let mut o = [0u8; 32];
    for (i, b) in o.iter_mut().enumerate() { *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?; }
    Ok(o)
}

pub(crate) fn write_atomic(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    let tmp = { let mut t = p.as_os_str().to_owned(); t.push(format!(".tmp{}", std::process::id())); PathBuf::from(t) };
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, p)?;
    Ok(())
}

#[derive(Default, Debug)]
pub(crate) struct PublishStats { pub chunks: u64, pub new_chunks: u64, pub new_bytes: u64, pub reused_chunks: u64, pub reused_bytes: u64, pub remapped: u64, pub raw_entries: u64, pub index_bytes: u64 }

/// Publish a transport file (.pkg.btr) into STORE as index/NAME.btridx plus chunks, signed with
/// `key` (secret key, key name) when given. Signing happens here, after any remapping, because
/// remapping rewrites the table and so the header.
pub(crate) fn publish(input: &Path, store: &Path, key: Option<(&sign::SecretKey, &str)>) -> Result<(String, PublishStats)> {
    let b = fs::read(input).with_context(|| format!("open {}", input.display()))?;
    let idx = parse_index(&b)?;
    ensure!(b.len() as u64 == idx.hdr.transport_len(&idx.chunks), "publish takes a .pkg.btr transport file (btrpkg convert)");
    let mut pos = idx.bytes.len();
    let mut items = Vec::with_capacity(idx.chunks.len());
    for c in &idx.chunks {
        let e = c.encoded_len as usize;
        items.push((*c, &b[pos..pos + e]));
        pos += e;
    }
    // Check every chunk, and the raw hash too: it is what reuse across builds relies on.
    items.par_iter().enumerate().try_for_each(|(i, (c, d))| check_chunk_full(i, c, d))?;

    let mut st = PublishStats::default();
    let mut out = Vec::with_capacity(items.len());
    for (c, d) in items {
        st.chunks += 1;
        let rp = hpath(store, "raw", &c.raw);
        let c2 = match fs::read_to_string(&rp) {
            Ok(s) => {
                // Same unencoded data published before: reuse that chunk, even if this
                // build compressed it to different bytes. Check the mapped chunk first, so a
                // damaged map can't get a wrong chunk into a signed index.
                let f: Vec<&str> = s.split_whitespace().collect();
                ensure!(f.len() == 3, "bad raw map entry {}", rp.display());
                let c2 = Chunk { compression: f[0].parse()?, encoded_len: f[1].parse()?, stored: unhex(f[2])?, ..c };
                let cd = fs::read(hpath(store, "chunks", &c2.stored)).context("raw map points at a missing chunk")?;
                // check_chunk_full covers the raw hash: a mapped chunk with other data is refused
                check_chunk_full(0, &c2, &cd).context("raw map points at a bad chunk")?;
                if c2.stored != c.stored { st.remapped += 1; }
                st.reused_chunks += 1;
                st.reused_bytes += c2.encoded_len as u64;
                c2
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cp = hpath(store, "chunks", &c.stored);
                // an existing chunk is reused only if it is intact; a damaged one is replaced
                if fs::read(&cp).is_ok_and(|old| old == d) {
                    // (identical bytes, so the full check above covers them)
                    st.reused_chunks += 1;
                    st.reused_bytes += d.len() as u64;
                } else {
                    write_atomic(&cp, d)?;
                    st.new_chunks += 1;
                    st.new_bytes += d.len() as u64;
                }
                write_atomic(&rp, format!("{} {} {}\n", c.compression, c.encoded_len, hex(&c.stored)).as_bytes())?;
                st.raw_entries += 1;
                c
            }
            Err(e) => return Err(e.into()),
        };
        out.push(c2);
    }
    let ib = if out == idx.chunks { idx.bytes.clone() } else { idx.with_chunks(&out) };
    parse_index(&ib).context("rebuilt index")?;
    st.index_bytes = ib.len() as u64;
    let name = input.file_name().context("no file name")?.to_string_lossy();
    let name = name.trim_end_matches(".btrpkg").trim_end_matches(".pkg.btr").to_string();
    let ip = store.join("index").join(format!("{name}.btridx"));
    let sp = store.join("index").join(format!("{name}.btridx.sig"));
    match key {
        Some((k, kname)) => write_atomic(&sp, k.sign(&ib[..HDR], kname).as_bytes())?,
        None => { let _ = fs::remove_file(&sp); }
    }
    write_atomic(&ip, &ib)?;
    Ok((name, st))
}

#[derive(Default, Debug)]
#[allow(dead_code)]
pub(crate) struct PullStats {
    pub chunks: u64, pub index_bytes: u64,
    /// from older cache files of the same package (reflinked)
    pub reused_chunks: u64, pub reused_bytes: u64,
    /// from installed files (read and checked; whole pieces reflinked, tails compressed here)
    pub installed_chunks: u64, pub installed_bytes: u64, pub installed_cloned: u64,
    /// downloaded
    pub fetched_chunks: u64, pub fetched_bytes: u64,
    /// cache ranges that failed their check and were not used
    pub bad_local: u64,
    pub encoded_chunks: u64, pub raw_chunks: u64, pub fallback_chunks: u64,
    pub requests: u64, pub retries: u64,
}

/// Header and chunk table of a cache file, if it is one.
fn cache_head(p: &Path) -> Option<(File, Header, Vec<Chunk>)> {
    let f = File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    let mut r = BufReader::new(&f);
    let (hdr, _, chunks, _) = read_head(&mut r).ok()?;
    (len == hdr.cache_len()).then_some(())?;
    drop(r);
    Some((f, hdr, chunks))
}

/// The package name of NAME-VERSION-REL-ARCH (pacman's file naming).
pub(crate) fn pkgname(name: &str) -> &str {
    let parts: Vec<&str> = name.rsplitn(4, '-').collect();
    match parts.len() { 4 => parts[3], 3 => parts[2], _ => name }
}

/// The signature published next to an index in a directory store, if any.
#[allow(dead_code)]
pub(crate) fn read_sig(store: &Path, name: &str) -> Option<String> {
    fs::read_to_string(store.join("index").join(format!("{name}.btridx.sig"))).ok()
}

/// Largest index `pull` downloads (ansible's, the largest seen, is 3.6 MB).
const MAX_INDEX: usize = 256 << 20;

/// Index and signature of NAME from a source.
pub(crate) fn fetch_index(src: &net::Source, name: &str, ns: &mut net::NetStats) -> Result<(Vec<u8>, Option<String>)> {
    let ib = src.get(&format!("index/{name}.btridx"), MAX_INDEX, ns)?.with_context(|| format!("no index for {name}"))?;
    let sig = src.get(&format!("index/{name}.btridx.sig"), 4096, ns)?.map(|b| String::from_utf8_lossy(&b).into_owned());
    Ok((ib, sig))
}

/// Where a chunk's data sits in an installed file: (path, offset in the file, bytes of file data).
fn chunk_files(idx: &Index) -> HashMap<u64, (Vec<u8>, u64, u64)> {
    let mut m = HashMap::new();
    for e in &idx.entries {
        if e.kind != K_FILE || e.small || e.size == 0 { continue; }
        let mut off = 0u64;
        let first = idx.chunks.partition_point(|c| c.image_off < e.data_off);
        for c in &idx.chunks[first..] {
            if off >= e.size { break; }
            m.entry(c.image_off).or_insert_with(|| (e.path.clone(), off, (e.size - off).min(c.unencoded_len as u64)));
            off += c.unencoded_len as u64;
        }
    }
    m
}

/// Read a piece of an installed file (a regular file, not followed through a final symlink),
/// zero-padded to `len`. Returns the open file too, for reflinking the piece.
fn read_installed(root: &Path, path: &[u8], off: u64, data_len: u64, len: u32) -> Option<(File, Vec<u8>)> {
    let p = root.join(std::ffi::OsStr::from_bytes(path));
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(p).ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() < off + data_len { return None; }
    let mut buf = vec![0u8; len as usize];
    f.read_exact_at(&mut buf[..data_len as usize], off).ok()?;
    Some((f, buf))
}

pub(crate) struct PullOpts<'a> {
    pub use_encoded: bool,
    /// an installed root to take unchanged data from (the package's older version, however it was
    /// installed)
    pub installed: Option<&'a Path>,
    /// zstd level for chunks compressed here (from installed files)
    pub level: i32,
    /// decode every downloaded chunk and check its unencoded hash too
    pub verify_data: bool,
    /// fsync the cache file before renaming it into place (a caller placing many packages syncs
    /// the filesystem once instead)
    pub fsync: bool,
}

/// Pull NAME from a source into CACHE. The index must pass `trust` before anything is written.
/// Each chunk comes from, in this order: an older cache file of the same package (reflinked,
/// after reading the range back and checking its hash), an installed file of the same path
/// (read, checked against the unencoded hash, compressed here), or the source (checked and
/// written as an encoded extent).
#[allow(dead_code)]
pub(crate) fn pull(src: &net::Source, name: &str, cache: &Path, trust: &sign::Trust, o: &PullOpts) -> Result<PullStats> {
    let mut ns = net::NetStats::default();
    let (ib, _) = fetch_index(src, name, &mut ns)?;
    pull_index(src, name, &ib, cache, trust, o, ns)
}

pub(crate) fn pull_index(src: &net::Source, name: &str, ib: &[u8], cache: &Path, trust: &sign::Trust, o: &PullOpts, mut ns: net::NetStats) -> Result<PullStats> {
    let idx = read_index_checked(&mut &ib[..], &|h| trust.check(h))?;
    ensure!(ib.len() as u64 == idx.hdr.index_len(), "trailing data after the index");
    let mut st = PullStats { index_bytes: ib.len() as u64, chunks: idx.chunks.len() as u64, ..Default::default() };
    // Already in the cache (cache files are checked when they are made).
    if open_cache(cache).is_ok_and(|(c, _)| c.bytes == ib) {
        st.reused_chunks = st.chunks;
        return Ok(st);
    }

    // Older cache files of the same package: (unencoded hash, length) -> (cache file, offset).
    // Keyed on the unencoded data, so a chunk framed locally (from a .pkg.btr.zst) is reused too.
    let dir = cache.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let want = pkgname(name);
    let mut srcs: Vec<File> = Vec::new();
    let mut local: HashMap<(Hash, u32), (usize, u64)> = HashMap::new();
    for ent in fs::read_dir(dir)? {
        let p = ent?.path();
        let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".pkg.btr")) else { continue };
        if p == cache || pkgname(stem) != want { continue; }
        let Some((f, h, chunks)) = cache_head(&p) else { continue };
        srcs.push(f);
        for c in &chunks {
            local.entry((c.raw, c.unencoded_len)).or_insert((srcs.len() - 1, h.image_base() + c.image_off));
        }
    }
    let files = if o.installed.is_some() { chunk_files(&idx) } else { HashMap::new() };

    let tmp = part_path(cache);
    let _ = fs::remove_file(&tmp);
    let out = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
    if o.use_encoded { set_nocomp(&out); }
    let image_base = idx.hdr.image_base();

    let mut run = || -> Result<()> {
        let mut can_clone = true;
        let mut w = ChunkWriter::new(o.use_encoded)?;
        let mut missing = Vec::new();
        let mut from_installed = Vec::new();
        for (i, c) in idx.chunks.iter().enumerate() {
            let off = image_base + c.image_off;
            if can_clone {
                if let Some(&(s, soff)) = local.get(&(c.raw, c.unencoded_len)) {
                    // Another cache file says it has this data; read it back and check before
                    // sharing it (the file may be damaged or older than the hash checks).
                    let mut buf = vec![0u8; c.unencoded_len as usize];
                    if srcs[s].read_exact_at(&mut buf, soff).is_err() || sha256(&buf) != c.raw {
                        st.bad_local += 1;
                    } else {
                        match clone_range_at(&srcs[s], soff, c.unencoded_len as u64, &out, off) {
                            Ok(()) => { st.reused_chunks += 1; st.reused_bytes += c.encoded_len as u64; continue; }
                            Err(e) => { eprintln!("btrpkg: reflink unavailable ({e}); downloading instead"); can_clone = false; }
                        }
                    }
                }
            }
            if let (Some(root), Some((path, foff, dlen))) = (o.installed, files.get(&c.image_off)) {
                if let Some((f, buf)) = read_installed(root, path, *foff, *dlen, c.unencoded_len) {
                    if sha256(&buf) == c.raw {
                        // A whole piece (not a file's unaligned tail) is shared with the installed
                        // file's extents instead of being compressed and written again.
                        if can_clone && *dlen == c.unencoded_len as u64 && clone_range_at(&f, *foff, *dlen, &out, off).is_ok() {
                            st.installed_chunks += 1;
                            st.installed_bytes += c.unencoded_len as u64;
                            st.installed_cloned += 1;
                            continue;
                        }
                        from_installed.push((*c, buf));
                        continue;
                    }
                }
            }
            missing.push((i, *c));
        }
        // Unchanged data from installed files: compressed here, in parallel, like the raw chunks
        // of a .pkg.btr.zst.
        let level = o.level;
        let framed: Vec<(Chunk, Vec<u8>)> = from_installed.into_par_iter().map(|(c, d)| -> Result<(Chunk, Vec<u8>)> {
            if !o.use_encoded { return Ok((Chunk { compression: COMP_NONE, encoded_len: c.unencoded_len, stored: c.raw, ..c }, d)); }
            let (comp, b) = frame(level, d)?;
            let stored = if comp == COMP_NONE { c.raw } else { sha256(&b) };
            Ok((Chunk { compression: comp, encoded_len: b.len() as u32, stored, ..c }, b))
        }).collect::<Result<_>>()?;
        for (c, d) in framed {
            st.installed_chunks += 1;
            st.installed_bytes += c.unencoded_len as u64;
            w.write(&out, image_base + c.image_off, &c, &d)?;
        }
        // The rest from the source, checked in parallel as batches arrive.
        let items: Vec<(String, usize)> = missing.iter().map(|(_, c)| { let x = hex(&c.stored); (format!("chunks/{}/{x}", &x[..2]), c.encoded_len as usize) }).collect();
        src.get_many(&items, &mut ns, |batch| {
            batch.par_iter().try_for_each(|(k, d)| if o.verify_data { check_chunk_full(missing[*k].0, &missing[*k].1, d) } else { check_chunk(missing[*k].0, &missing[*k].1, d) })?;
            for (k, d) in batch {
                let c = missing[k].1;
                st.fetched_chunks += 1;
                st.fetched_bytes += d.len() as u64;
                w.write(&out, image_base + c.image_off, &c, &d)?;
            }
            Ok(())
        })?;
        (st.encoded_chunks, st.raw_chunks, st.fallback_chunks) = (w.encoded_chunks, w.raw_chunks, w.fallback_chunks);
        // Same cache-file layout as `fetch`, so `install` works unchanged.
        out.write_all_at(ib, 0)?;
        out.set_len(idx.hdr.cache_len())?;
        if o.fsync { out.sync_all()?; }
        Ok(())
    };
    let r = run();
    drop(out);
    (st.requests, st.retries) = (ns.requests, ns.retries);
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, cache)?;
    Ok(st)
}
