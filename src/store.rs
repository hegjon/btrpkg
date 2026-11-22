//! Content-addressed store: `btrpkg publish` and `btrpkg pull`. A btrpart is one table entry
//! of an index (the code calls it a chunk): up to 128 KiB of one file's data, stored the way
//! btrfs stores an extent (a zstd frame it can take as an encoded extent, or raw).
//!
//! Store layout (static files, servable by any HTTP server; `pull` reads it as a directory):
//!   PARTS/ab/<sha256>    btrpart bytes exactly as written to disk, named by the SHA-256 of
//!                        those bytes (the table's stored hash). PARTS is `btrparts` inside the
//!                        store by default; a repository can put it elsewhere (`../btrparts`,
//!                        shared by its architecture directories) and says so in its database
//!                        (`btrpkg.parts`).
//!   NAME.pkg.btr         the package's index (format.rs); its chunk table names the chunks
//!   NAME.pkg.btr.sig     signify signature over the index header (sign.rs), when published
//!                        with a key
//! Publish writes chunks, then the signature, then the index, so a published index never refers
//! to a missing chunk or lacks its signature.
use super::*;
use std::os::unix::ffi::OsStrExt;

/// A btrpart's path in its directory, "xx/HASH" (also the key in --want files).
pub(crate) fn part_rel(h: &[u8]) -> String {
    let x = hex(h);
    format!("{}/{x}", &x[..2])
}

pub(crate) fn hpath(dir: &Path, h: &[u8]) -> PathBuf { dir.join(part_rel(h)) }

/// The index of package NAME (NAME-VER-REL-ARCH) in a repository, next to NAME.pkg.tar.zst, as
/// the database's btrpkg.index names it; and back. It is a .pkg.btr without its data (the same
/// bytes a cache file starts with, and the same signature): clients fill the data in.
pub(crate) const INDEX_EXT: &str = ".pkg.btr";
pub(crate) fn index_rel(name: impl std::fmt::Display) -> String { format!("{name}{INDEX_EXT}") }
pub(crate) fn index_name(rel: &str) -> Option<&str> {
    rel.strip_suffix(INDEX_EXT).filter(|n| !n.is_empty() && !n.starts_with('.') && !n.contains('/'))
}
/// Its signify signature.
pub(crate) fn sig_rel(name: impl std::fmt::Display) -> String { format!("{name}{INDEX_EXT}.sig") }
/// Repositories: the SHA-256 of a package file an index was made from, beside the package file
/// (`PKGFILE.sha256sum`, in sha256sum's format: "HASH  PKGFILE"), so repo-update knows the file
/// is converted already.
pub(crate) fn sum_rel(pkgfile: &str) -> String { format!("{pkgfile}.sha256sum") }

/// NAME-VER-REL-ARCH -> (NAME, VER-REL)
pub(crate) fn split_stem(stem: &str) -> Option<(&str, &str)> {
    let name = pkgname(stem);
    let rest = stem.strip_prefix(name)?.strip_prefix('-')?;
    let (ver, _arch) = rest.rsplit_once('-')?;
    Some((name, ver))
}

/// syncfs(2) on the filesystem of directory `p`.
pub(crate) fn syncfs_dir(p: &Path) -> Result<()> {
    let d = File::open(p).with_context(|| p.display().to_string())?;
    ensure!(unsafe { libc::syncfs(d.as_raw_fd()) } == 0, "syncfs {}: {}", p.display(), io::Error::last_os_error());
    Ok(())
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
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    // unique per call: threads of one process may write files side by side
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = { let mut t = p.as_os_str().to_owned(); t.push(format!(".tmp{}.{n}", std::process::id())); PathBuf::from(t) };
    fs::write(&tmp, data)?;
    fs::rename(&tmp, p)?;
    Ok(())
}

#[derive(Default, Debug)]
pub(crate) struct PublishStats { pub chunks: u64, pub new_chunks: u64, pub new_bytes: u64, pub reused_chunks: u64, pub reused_bytes: u64, pub remapped: u64, pub index_bytes: u64 }

/// Publish a converted package (its index and its btrparts, from `build`) into STORE as
/// NAME.pkg.btr plus btrparts, signed with `key` (secret key, key name) when given. Signing
/// happens here, after any remapping, because remapping rewrites the table and so the header.
/// What `publish` reuses when the same data comes framed differently (another zstd level or
/// version): the unencoded hash of every btrpart named by an index of the repositories that
/// share the btrparts directory `parts` (found under its parent, like gc finds them), mapped to
/// the btrpart as stored. Built from the indexes each time, nothing is kept on disk. An index
/// that can't be read is skipped: reuse only saves space, and `publish` checks a btrpart fully
/// before it reuses it.
pub(crate) type RawMap = HashMap<Hash, Chunk>;

pub(crate) fn raw_map(parts: &Path) -> RawMap {
    let mut m = RawMap::new();
    // no btrparts directory yet: nothing published to reuse
    let Ok(parts) = parts.canonicalize() else { return m };
    let Some(root) = parts.parent() else { return m };
    let mut dirs = Vec::new();
    if let Err(e) = crate::gc::find_repos(root, &parts, 0, &mut dirs) {
        eprintln!("btrpkg: reuse across builds: {e:#} (some indexes not seen)");
    }
    let files: Vec<PathBuf> = dirs.iter().flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
        .filter(|e| index_name(&e.file_name().to_string_lossy()).is_some() && e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path()).collect();
    let tables: Vec<Vec<Chunk>> = files.par_iter().filter_map(|p| {
        let f = File::open(p).ok()?;
        read_head(&mut BufReader::new(f)).ok().map(|(_, _, chunks, _)| chunks)
    }).collect();
    for c in tables.into_iter().flatten() { m.entry(c.raw).or_insert(c); }
    m
}

/// Publish into STORE with the btrparts in `parts` (absolute, or relative to the
/// working directory: a caller resolves a repository's relative path first). `reuse`: see
/// `raw_map` (an empty map reuses only identical btrparts).
pub(crate) fn publish(index: &[u8], items: Vec<(Chunk, Vec<u8>)>, name: &str, store: &Path, parts: &Path, key: Option<(&sign::SecretKey, &str)>, reuse: &RawMap) -> Result<PublishStats> {
    let idx = parse_index(index)?;
    ensure!(items.len() == idx.chunks.len() && items.iter().zip(&idx.chunks).all(|((c, d), t)| c == t && d.len() == c.encoded_len as usize),
        "{name}: btrparts don't match the index");

    // Decide for every chunk, in parallel: reuse (by its unencoded hash or identical stored
    // bytes), or write it. Writes go out in parallel without a per-file fsync, each distinct file
    // once, then one syncfs; the index that points at the chunks is written (and synced) last.
    enum Act<'a> { Remap(Chunk), Same, New(&'a [u8]) }
    let acts: Vec<(Chunk, Act)> = items.par_iter().map(|(c, d)| -> (Chunk, Act) {
        let (c, d) = (*c, d.as_slice());
        // Same unencoded data published before: reuse that btrpart, even if this build framed
        // it to different bytes, but only if it is there and intact (check_chunk_full covers the
        // unencoded hash, so an index naming a btrpart with other data can't get it reused)
        if let Some(m) = reuse.get(&c.raw) {
            let c2 = Chunk { compression: m.compression, encoded_len: m.encoded_len, stored: m.stored, ..c };
            if fs::read(hpath(parts, &c2.stored)).is_ok_and(|cd| check_chunk_full(0, &c2, &cd).is_ok()) {
                return (c2, Act::Remap(c));
            }
        }
        // an existing btrpart is reused only if it is intact; a damaged one is replaced
        // (identical bytes, so the full check above covers them)
        if fs::read(hpath(parts, &c.stored)).is_ok_and(|old| old == d) { (c, Act::Same) } else { (c, Act::New(d)) }
    }).collect();

    let mut st = PublishStats::default();
    let mut out = Vec::with_capacity(acts.len());
    let mut chunk_writes: HashMap<Hash, &[u8]> = HashMap::new();
    for (c2, act) in &acts {
        st.chunks += 1;
        match act {
            Act::Remap(c) => {
                if c2.stored != c.stored { st.remapped += 1; }
                st.reused_chunks += 1;
                st.reused_bytes += c2.encoded_len as u64;
            }
            Act::Same | Act::New(_) => {
                if let Act::New(d) = act {
                    if chunk_writes.insert(c2.stored, d).is_none() { st.new_chunks += 1; st.new_bytes += d.len() as u64; }
                } else {
                    st.reused_chunks += 1;
                    st.reused_bytes += c2.encoded_len as u64;
                }
            }
        }
        out.push(*c2);
    }
    chunk_writes.par_iter().try_for_each(|(h, d)| write_atomic_nosync(&hpath(parts, h), d))?;
    if !chunk_writes.is_empty() { syncfs_dir(parts)?; }
    let ib = if out == idx.chunks { idx.bytes.clone() } else { idx.with_chunks(&out) };
    parse_index(&ib).context("rebuilt index")?;
    st.index_bytes = ib.len() as u64;
    let ip = store.join(index_rel(name));
    let sp = store.join(sig_rel(name));
    match key {
        Some((k, kname)) => write_atomic(&sp, k.sign(&ib[..HDR], kname).as_bytes())?,
        None => { let _ = fs::remove_file(&sp); }
    }
    write_atomic(&ip, &ib)?;
    Ok(st)
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
    /// of those, fetched without their padding (HTTP Range), and the zero bytes not sent
    pub ranged: u64, pub range_saved: u64,
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
    fs::read_to_string(store.join(sig_rel(name))).ok()
}

/// Largest index `pull` downloads (ansible's, the largest seen, is 3.6 MB).
const MAX_INDEX: usize = 256 << 20;

/// Index and signature of NAME from a source.
pub(crate) fn fetch_index(src: &net::Source, name: &str, ns: &mut net::NetStats) -> Result<(Vec<u8>, Option<String>)> {
    fetch_indexes(src, &[name], ns).pop().unwrap()
}

/// The indexes (and signatures, if there are any) of `names` from one source, all requested
/// at once (over HTTP/2 on one connection); one result per name, in order.
pub(crate) fn fetch_indexes(src: &net::Source, names: &[&str], ns: &mut net::NetStats) -> Vec<Result<(Vec<u8>, Option<String>)>> {
    let reqs: Vec<(String, usize)> = names.iter().flat_map(|n| [(index_rel(n), MAX_INDEX), (sig_rel(n), 4096)]).collect();
    let mut got = src.get_all(&reqs, ns).into_iter();
    names.iter().map(|name| {
        let (ib, sig) = (got.next().unwrap(), got.next().unwrap());
        let ib = ib?.with_context(|| format!("no index for {name}"))?;
        Ok((ib, sig?.map(|b| String::from_utf8_lossy(&b).into_owned())))
    }).collect()
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

/// Bytes of a btrpart that carry data: for a raw btrpart at the end of a file, its data up to
/// the end of the file (the rest is padding, zeros); everything else in full (a zstd frame is
/// only usable whole). Any file the btrpart holds will do: the bytes after its end are zeros in
/// the btrpart's content, and the content is checked against its SHA-256 anyway.
pub(crate) fn data_len(c: &Chunk, files: &HashMap<u64, (Vec<u8>, u64, u64)>) -> usize {
    match files.get(&c.image_off) {
        Some((_, _, dlen)) if c.compression == COMP_NONE => (*dlen as usize).min(c.encoded_len as usize),
        _ => c.encoded_len as usize,
    }
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
    /// progress lines on stdout, as fetch-ids prints them (total/got/done NAME BYTES)
    pub progress: bool,
    /// where the source's btrparts are, relative to it (net::DEFAULT_PARTS unless the
    /// repository's database says otherwise)
    pub parts: &'a str,
}

impl Default for PullOpts<'_> {
    fn default() -> Self {
        PullOpts { use_encoded: true, installed: None, verify_data: false, fsync: true, before_fetch: None,
            plan_only: false, progress: false, parts: net::DEFAULT_PARTS }
    }
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
    let mut v = pull_many(&src.parts(o.parts)?, &[PullItem { name, ib, cache, trust }], o, ns)?;
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
/// written. A failure leaves none of the packages' cache files behind. `src` is the btrparts
/// directory itself (`Source::parts`), not the store.
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
        let framed: Vec<(usize, Chunk, Vec<u8>)> = tails.into_par_iter().map(|(j, c, d)| -> Result<(usize, Chunk, Vec<u8>)> {
            if !o.use_encoded { return Ok((j, Chunk { compression: COMP_NONE, encoded_len: c.unencoded_len, stored: c.raw, ..c }, d)); }
            let (comp, b) = frame(d)?;
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
        let files: Vec<HashMap<u64, (Vec<u8>, u64, u64)>> = jobs.iter().map(|j| chunk_files(&j.idx)).collect();
        let items: Vec<(String, usize, usize)> = missing.iter().map(|&(j, i)| {
            let c = &jobs[j].idx.chunks[i];
            (part_rel(&c.stored), c.encoded_len as usize, data_len(c, &files[j]))
        }).collect();
        // progress: bytes each package still needs from the source
        let mut need = vec![0u64; jobs.len()];
        for &(j, i) in &missing { need[j] += jobs[j].idx.chunks[i].encoded_len as u64; }
        let say = |line: String| {
            if o.progress { let mut out = std::io::stdout().lock(); let _ = writeln!(out, "{line}"); let _ = out.flush(); }
        };
        for (j, n) in need.iter().enumerate() { say(format!("total\t{}\t{n}", jobs[j].item.name)); }
        let mut got = vec![0u64; jobs.len()];
        if let Some(hook) = o.before_fetch {
            // the same chunk can be missing for two packages; ask for it once
            let mut seen = std::collections::HashSet::new();
            let uniq: Vec<(String, usize)> = items.iter().filter(|(r, _, _)| seen.insert(r.clone())).map(|(r, l, _)| (r.clone(), *l)).collect();
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
                got[j] += d.len() as u64;
                w.write(jobs[j].out.as_ref().unwrap(), jobs[j].idx.hdr.image_base() + c.image_off, &c, &d)?;
            }
            for (j, g) in got.iter().enumerate() { if need[j] > 0 { say(format!("got\t{}\t{g}", jobs[j].item.name)); } }
            Ok(())
        })?;
        for j in jobs.iter() { say(format!("done\t{}", j.item.name)); }
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
    if let Some(j) = jobs.first_mut() { (j.st.requests, j.st.retries, j.st.ranged, j.st.range_saved) = (ns.requests, ns.retries, ns.ranged, ns.saved); }
    Ok(jobs.into_iter().map(|j| j.st).collect())
}
