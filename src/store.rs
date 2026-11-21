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

pub(crate) fn hpath(store: &Path, kind: &str, h: &[u8]) -> PathBuf {
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

/// Like `write_atomic` without the fsync (for many small files that are synced together later,
/// or not at all: a download directory that is checked before use).
pub(crate) fn write_atomic_nosync(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    let tmp = { let mut t = p.as_os_str().to_owned(); t.push(format!(".tmp{}", std::process::id())); PathBuf::from(t) };
    fs::write(&tmp, data)?;
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
pub(crate) fn cache_head(p: &Path) -> Option<(File, Header, Vec<Chunk>)> {
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
pub(crate) fn chunk_files(idx: &Index) -> HashMap<u64, (Vec<u8>, u64, u64)> {
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
fn read_installed_path(p: &Path, off: u64, data_len: u64, len: u32) -> Option<(File, Vec<u8>)> {
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
    /// called with the chunks still missing (store path, length) after all local reuse, before
    /// they are read from the source: lets another process fetch them into a directory source
    pub before_fetch: Option<&'a (dyn Fn(&[(String, usize)]) -> Result<()> + Sync)>,
    /// stop after checking local data: nothing is written; `fetched_*` in the stats say what
    /// would be downloaded (a chunk two packages need counts once, on the first)
    pub plan_only: bool,
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

pub(crate) fn pull_index(src: &net::Source, name: &str, ib: &[u8], cache: &Path, trust: &sign::Trust, o: &PullOpts, ns: net::NetStats) -> Result<PullStats> {
    let mut v = pull_many(src, &[PullItem { name, ib, cache, trust }], o, ns)?;
    Ok(v.pop().unwrap())
}

pub(crate) struct PullItem<'a> { pub name: &'a str, pub ib: &'a [u8], pub cache: &'a Path, pub trust: &'a sign::Trust }

/// Where a chunk's data will come from.
enum From { Local(usize, u64), Installed(PathBuf, u64, u64), Download }

#[derive(Debug)]
struct Planned;
impl std::fmt::Display for Planned { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "planned") } }
impl std::error::Error for Planned {}

/// One package being pulled.
struct Job<'a> {
    item: &'a PullItem<'a>,
    idx: Index,
    st: PullStats,
    out: Option<File>,
    tmp: PathBuf,
    srcs: Vec<File>,
}

/// Pull several packages from one source. Each chunk comes from, in this order: an older cache
/// file of the same package (reflinked), an installed file of the same path (whole pieces
/// reflinked, tails compressed here), or the source. Reused data is read back and its unencoded
/// hash checked first, in parallel. All packages' downloads share one queue, so the connection
/// stays busy across package boundaries. Every index must pass its `trust` before anything is
/// written. A failure leaves none of the packages' cache files behind.
pub(crate) fn pull_many(src: &net::Source, items: &[PullItem], o: &PullOpts, mut ns: net::NetStats) -> Result<Vec<PullStats>> {
    let mut jobs: Vec<Job> = Vec::new();
    for it in items {
        let idx = read_index_checked(&mut &it.ib[..], &|h| it.trust.check(h)).with_context(|| it.name.to_string())?;
        ensure!(it.ib.len() as u64 == idx.hdr.index_len(), "{}: trailing data after the index", it.name);
        let st = PullStats { index_bytes: it.ib.len() as u64, chunks: idx.chunks.len() as u64, ..Default::default() };
        jobs.push(Job { item: it, idx, st, out: None, tmp: part_path(it.cache), srcs: vec![] });
    }
    let result = (|| -> Result<()> {
        // 1. per package: where each chunk can come from (map lookups only)
        let mut plans: Vec<Vec<From>> = Vec::new();
        for j in jobs.iter_mut() {
            // Already in the cache (cache files are checked when they are made).
            if open_cache(j.item.cache).is_ok_and(|(c, _)| c.bytes == j.item.ib) {
                j.st.reused_chunks = j.st.chunks;
                plans.push(vec![]);
                continue;
            }
            let dir = j.item.cache.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let want = pkgname(j.item.name);
            let mut local: HashMap<(Hash, u32), (usize, u64)> = HashMap::new();
            for ent in fs::read_dir(dir)? {
                let p = ent?.path();
                let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".pkg.btr")) else { continue };
                if p == j.item.cache || pkgname(stem) != want { continue; }
                let Some((f, h, chunks)) = cache_head(&p) else { continue };
                j.srcs.push(f);
                for c in &chunks {
                    local.entry((c.raw, c.unencoded_len)).or_insert((j.srcs.len() - 1, h.image_base() + c.image_off));
                }
            }
            let files = if o.installed.is_some() { chunk_files(&j.idx) } else { HashMap::new() };
            let plan = j.idx.chunks.iter().map(|c| {
                if let Some(&(s, off)) = local.get(&(c.raw, c.unencoded_len)) { return From::Local(s, off); }
                if let (Some(root), Some((path, foff, dlen))) = (o.installed, files.get(&c.image_off)) {
                    return From::Installed(root.join(std::ffi::OsStr::from_bytes(path)), *foff, *dlen);
                }
                From::Download
            }).collect();
            if o.plan_only {
                // a File standing for "needs work"; nothing is written in plan mode
                j.out = Some(File::open("/dev/null")?);
                plans.push(plan);
                continue;
            }
            let _ = fs::remove_file(&j.tmp);
            let out = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&j.tmp)?;
            if o.use_encoded { set_nocomp(&out); }
            j.out = Some(out);
            plans.push(plan);
        }

        // 2. read back and check every reusable range, all packages at once, in parallel
        struct Checked { job: usize, chunk: usize, ok: bool, data: Option<Vec<u8>>, file: Option<File> }
        let todo: Vec<(usize, usize)> = plans.iter().enumerate()
            .flat_map(|(j, p)| p.iter().enumerate().filter(|(_, f)| !matches!(f, From::Download)).map(move |(i, _)| (j, i))).collect();
        let checked: Vec<Checked> = todo.par_iter().map(|&(j, i)| {
            let c = &jobs[j].idx.chunks[i];
            match &plans[j][i] {
                From::Local(s, off) => {
                    let mut buf = vec![0u8; c.unencoded_len as usize];
                    let ok = jobs[j].srcs[*s].read_exact_at(&mut buf, *off).is_ok() && sha256(&buf) == c.raw;
                    Checked { job: j, chunk: i, ok, data: None, file: None }
                }
                From::Installed(p, foff, dlen) => match read_installed_path(p, *foff, *dlen, c.unencoded_len) {
                    Some((f, buf)) if sha256(&buf) == c.raw => {
                        let whole = *dlen == c.unencoded_len as u64;
                        Checked { job: j, chunk: i, ok: true, data: (!whole).then_some(buf), file: Some(f) }
                    }
                    _ => Checked { job: j, chunk: i, ok: false, data: None, file: None },
                },
                From::Download => unreachable!(),
            }
        }).collect();

        if o.plan_only {
            let ok: std::collections::HashSet<(usize, usize)> = checked.iter().filter(|c| c.ok).map(|c| (c.job, c.chunk)).collect();
            let mut seen: std::collections::HashSet<Hash> = std::collections::HashSet::new();
            for (j, plan) in plans.iter().enumerate() {
                if jobs[j].out.is_none() { continue; }
                for i in 0..plan.len() {
                    let c = jobs[j].idx.chunks[i];
                    if ok.contains(&(j, i)) { jobs[j].st.installed_chunks += 1; continue; }
                    if !seen.insert(c.stored) { continue; }
                    jobs[j].st.fetched_chunks += 1;
                    jobs[j].st.fetched_bytes += c.encoded_len as u64;
                }
            }
            return Err(Planned.into());
        }

        // 3. reflink what checked out; tails of installed files are compressed (in parallel)
        let mut can_clone = true;
        let mut missing: Vec<(usize, usize)> = Vec::new();
        let mut tails: Vec<(usize, Chunk, Vec<u8>)> = Vec::new();
        let mut ok_set: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
        for ck in checked {
            let (j, i) = (ck.job, ck.chunk);
            let c = jobs[j].idx.chunks[i];
            let off = jobs[j].idx.hdr.image_base() + c.image_off;
            if !ck.ok {
                if matches!(plans[j][i], From::Local(..)) { jobs[j].st.bad_local += 1; }
                continue;
            }
            match &plans[j][i] {
                From::Local(s, soff) if can_clone => {
                    let r = clone_range_at(&jobs[j].srcs[*s], *soff, c.unencoded_len as u64, jobs[j].out.as_ref().unwrap(), off);
                    match r {
                        Ok(()) => { jobs[j].st.reused_chunks += 1; jobs[j].st.reused_bytes += c.encoded_len as u64; ok_set.insert((j, i)); }
                        Err(e) => { eprintln!("btrpkg: reflink unavailable ({e}); downloading instead"); can_clone = false; }
                    }
                }
                From::Installed(_, foff, dlen) => {
                    let f = ck.file.as_ref().unwrap();
                    if ck.data.is_none() && can_clone && clone_range_at(f, *foff, *dlen, jobs[j].out.as_ref().unwrap(), off).is_ok() {
                        jobs[j].st.installed_chunks += 1;
                        jobs[j].st.installed_bytes += c.unencoded_len as u64;
                        jobs[j].st.installed_cloned += 1;
                        ok_set.insert((j, i));
                    } else {
                        let data = match ck.data {
                            Some(d) => d,
                            None => { let mut b = vec![0u8; c.unencoded_len as usize]; f.read_exact_at(&mut b[..*dlen as usize], *foff)?; b }
                        };
                        tails.push((j, c, data));
                        ok_set.insert((j, i));
                    }
                }
                _ => {}
            }
        }
        let level = o.level;
        let framed: Vec<(usize, Chunk, Vec<u8>)> = tails.into_par_iter().map(|(j, c, d)| -> Result<(usize, Chunk, Vec<u8>)> {
            if !o.use_encoded { return Ok((j, Chunk { compression: COMP_NONE, encoded_len: c.unencoded_len, stored: c.raw, ..c }, d)); }
            let (comp, b) = frame(level, d)?;
            let stored = if comp == COMP_NONE { c.raw } else { sha256(&b) };
            Ok((j, Chunk { compression: comp, encoded_len: b.len() as u32, stored, ..c }, b))
        }).collect::<Result<_>>()?;
        let mut w = ChunkWriter::new(o.use_encoded)?;
        for (j, c, d) in framed {
            jobs[j].st.installed_chunks += 1;
            jobs[j].st.installed_bytes += c.unencoded_len as u64;
            w.write(jobs[j].out.as_ref().unwrap(), jobs[j].idx.hdr.image_base() + c.image_off, &c, &d)?;
        }
        for (j, plan) in plans.iter().enumerate() {
            if jobs[j].out.is_none() { continue; }
            for i in 0..plan.len() { if !ok_set.contains(&(j, i)) { missing.push((j, i)); } }
        }

        // 4. one download queue for everything still missing, checked in parallel as it arrives
        let items: Vec<(String, usize)> = missing.iter().map(|&(j, i)| {
            let c = &jobs[j].idx.chunks[i];
            let x = hex(&c.stored);
            (format!("chunks/{}/{x}", &x[..2]), c.encoded_len as usize)
        }).collect();
        if let Some(hook) = o.before_fetch {
            // the same chunk can be missing for two packages; ask for it once
            let mut seen = std::collections::HashSet::new();
            let uniq: Vec<(String, usize)> = items.iter().filter(|(r, _)| seen.insert(r.clone())).cloned().collect();
            hook(&uniq)?;
        }
        src.get_many(&items, &mut ns, |batch| {
            batch.par_iter().try_for_each(|(k, d)| {
                let (j, i) = missing[*k];
                let c = &jobs[j].idx.chunks[i];
                if o.verify_data { check_chunk_full(i, c, d) } else { check_chunk(i, c, d) }.with_context(|| jobs[j].item.name.to_string())
            })?;
            for (k, d) in batch {
                let (j, i) = missing[k];
                let c = jobs[j].idx.chunks[i];
                jobs[j].st.fetched_chunks += 1;
                jobs[j].st.fetched_bytes += d.len() as u64;
                w.write(jobs[j].out.as_ref().unwrap(), jobs[j].idx.hdr.image_base() + c.image_off, &c, &d)?;
            }
            Ok(())
        })?;
        // the writer is shared: its counts go on the first package, like the request counts
        if let Some(j) = jobs.first_mut() { (j.st.encoded_chunks, j.st.raw_chunks, j.st.fallback_chunks) = (w.encoded_chunks, w.raw_chunks, w.fallback_chunks); }
        for j in jobs.iter_mut() {
            let Some(out) = j.out.as_ref() else { continue };
            // Same cache-file layout as `fetch`, so `install` works unchanged.
            out.write_all_at(j.item.ib, 0)?;
            out.set_len(j.idx.hdr.cache_len())?;
            if o.fsync { out.sync_all()?; }
        }
        Ok(())
    })();
    if let Err(e) = result {
        if o.plan_only && e.downcast_ref::<Planned>().is_some() {
            return Ok(jobs.into_iter().map(|j| j.st).collect());
        }
        for j in &jobs { if j.out.is_some() { let _ = fs::remove_file(&j.tmp); } }
        return Err(e);
    }
    for j in jobs.iter_mut() {
        if j.out.take().is_some() { fs::rename(&j.tmp, j.item.cache)?; }
    }
    if let Some(j) = jobs.first_mut() { (j.st.requests, j.st.retries) = (ns.requests, ns.retries); }
    Ok(jobs.into_iter().map(|j| j.st).collect())
}
