//! `btrpkg upgrade` (also run as `pacbtr`): upgrade installed packages through btrpkg, with pacman
//! keeping the database (step 1 of the pacman integration in HISTORY.md).
//!
//!  1. Read pacman.conf, the local database and the sync databases. Targets: installed packages
//!     whose sync entry (first repo wins, as in pacman) is newer and has btrpkg fields.
//!  2. Fetch each target's index and signature; it must match the database's `btrpkg.id` and carry
//!     a signature from /etc/btrpkg/keys or /usr/share/btrpkg/keys.
//!  3. Leave to pacman what this path can't do right: packages with an `.INSTALL` (pre_upgrade
//!     would run after the files are in place) or backup files (no `.pacnew` handling), and
//!     packages whose old version has backup files.
//!  4. Build a metadata-only package per target (.PKGINFO, .BUILDINFO, .MTREE) and let pacman
//!     check dependencies and conflicts with `pacman -U --dbonly --print`. Nothing is changed yet.
//!  5. Check file conflicts against the local database (pacman's own check is skipped by --dbonly).
//!  6. Pull every target into the package cache (reusing older cache files and installed files).
//!  7. Snapshot the root (snapper, config `root`).
//!  8. Place the files, remove the files the old versions had and the new ones don't, then
//!     `pacman -U --dbonly` with the metadata packages: pacman writes the database entries and
//!     runs the hooks. On any failure after the snapshot: `snapper undochange` back to it.
use super::*;
use std::collections::{BTreeMap, HashSet};
use std::process::Command;

#[derive(Debug, Clone)]
struct Repo { name: String, servers: Vec<String> }

struct Conf { db_path: PathBuf, cache_dir: PathBuf, repos: Vec<Repo> }

fn arch() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    unsafe { std::ffi::CStr::from_ptr(u.machine.as_ptr()) }.to_string_lossy().into_owned()
}

/// The parts of pacman.conf this needs: DBPath, CacheDir, Architecture and the repos' servers
/// (Include files followed), in order.
fn read_conf(p: &Path) -> Result<Conf> {
    let mut c = Conf { db_path: "/var/lib/pacman".into(), cache_dir: "/var/cache/pacman/pkg".into(), repos: vec![] };
    let mut arch_v = "auto".to_string();
    let mut section = String::new();
    let mut raw: Vec<(String, Vec<String>)> = vec![];
    fn lines(p: &Path, out: &mut Vec<(String, String)>, section: &mut String, depth: u32) -> Result<()> {
        ensure!(depth < 8, "Include loop in {}", p.display());
        for l in fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?.lines() {
            let l = l.split('#').next().unwrap().trim();
            if l.is_empty() { continue; }
            if l.starts_with('[') && l.ends_with(']') { *section = l[1..l.len() - 1].to_string(); continue; }
            let (k, v) = l.split_once('=').map(|(k, v)| (k.trim(), v.trim())).unwrap_or((l, ""));
            if k == "Include" {
                lines(Path::new(v), out, section, depth + 1)?;
            } else {
                out.push((section.clone(), format!("{k}={v}")));
            }
        }
        Ok(())
    }
    let mut kv = vec![];
    lines(p, &mut kv, &mut section, 0)?;
    let mut cache_set = false;
    for (sec, l) in kv {
        let (k, v) = l.split_once('=').unwrap();
        if sec == "options" {
            match k {
                "DBPath" => c.db_path = v.into(),
                "CacheDir" if !cache_set => { c.cache_dir = v.into(); cache_set = true; }
                "Architecture" => arch_v = v.split_whitespace().next().unwrap_or("auto").into(),
                _ => {}
            }
        } else if k == "Server" {
            match raw.iter_mut().find(|r| r.0 == sec) { Some(r) => r.1.push(v.into()), None => raw.push((sec.clone(), vec![v.into()])) }
        } else if !raw.iter().any(|r| r.0 == sec) {
            raw.push((sec.clone(), vec![]));
        }
    }
    let a = if arch_v == "auto" { arch() } else { arch_v };
    c.repos = raw.into_iter().map(|(name, s)| {
        let servers = s.iter().map(|u| u.replace("$repo", &name).replace("$arch", &a)).collect();
        Repo { name, servers }
    }).collect();
    Ok(c)
}

#[derive(Debug, Clone, Default)]
struct DbEntry { name: String, version: String, data: Vec<String>, backup: Vec<String>, dir: String }

impl DbEntry {
    fn from_desc(d: &repo::Desc) -> DbEntry {
        let one = |k| d.get(k).unwrap_or_default().to_string();
        DbEntry { name: one("%NAME%"), version: one("%VERSION%"),
            data: d.lines("%DATA%").to_vec(), backup: d.lines("%BACKUP%").to_vec(), dir: String::new() }
    }
    fn field(&self, k: &str) -> Option<&str> { repo::data_field(&self.data, k) }
}

fn read_sync_db(p: &Path) -> Result<Vec<DbEntry>> {
    let (_, tarb) = repo::read_db(p)?;
    Ok(repo::descs(&tarb)?.iter().map(DbEntry::from_desc).collect())
}

/// Installed packages: name -> (desc, files with dirs ending in '/').
fn read_local(db_path: &Path) -> Result<BTreeMap<String, (DbEntry, Vec<String>)>> {
    let mut m = BTreeMap::new();
    for d in fs::read_dir(db_path.join("local"))? {
        let d = d?.path();
        if !d.is_dir() { continue; }
        let mut e = DbEntry::from_desc(&repo::Desc::parse(&fs::read_to_string(d.join("desc"))?));
        e.dir = d.file_name().unwrap().to_string_lossy().into();
        let files = fs::read_to_string(d.join("files")).unwrap_or_default();
        let mut in_files = false;
        let mut list = vec![];
        for l in files.lines() {
            if l.starts_with('%') { in_files = l == "%FILES%"; continue; }
            if in_files && !l.is_empty() { list.push(l.to_string()); }
        }
        m.insert(e.name.clone(), (e, list));
    }
    Ok(m)
}

/// The bytes of a package's own entry (small file from the blob, large file from the cache file).
fn entry_bytes(idx: &Index, cache: Option<&File>, path: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(e) = idx.entries.iter().find(|e| e.path == path && e.kind == K_FILE) else { return Ok(None) };
    if e.small { return Ok(Some(idx.small[e.data_off as usize..(e.data_off + e.size) as usize].to_vec())); }
    let f = cache.context("large metadata file needs the cache file")?;
    let mut b = vec![0u8; e.size as usize];
    f.read_exact_at(&mut b, idx.hdr.image_base() + e.data_off)?;
    Ok(Some(b))
}

fn pkginfo_lines<'a>(pi: &'a str, key: &str) -> Vec<&'a str> {
    pi.lines().filter_map(|l| l.split_once(" = ").filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim())).collect()
}

/// A metadata-only package: .PKGINFO, .BUILDINFO, .MTREE from the package itself.
fn meta_package(idx: &Index, cache: Option<&File>, out: &Path) -> Result<()> {
    let z = zstd::stream::write::Encoder::new(File::create(out)?, 3)?.auto_finish();
    let mut b = tar::Builder::new(z);
    for name in [".PKGINFO", ".BUILDINFO", ".MTREE"] {
        let Some(data) = entry_bytes(idx, cache, name.as_bytes())? else {
            ensure!(name != ".PKGINFO", "package without .PKGINFO");
            continue;
        };
        let e = idx.entries.iter().find(|e| e.path == name.as_bytes()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_path(name)?;
        h.set_mode(0o644);
        h.set_mtime(e.mtime.max(0) as u64);
        h.set_size(data.len() as u64);
        h.set_cksum();
        b.append(&h, &data[..])?;
    }
    b.into_inner()?;
    Ok(())
}

fn pacman(conf: &Path, args: &[&str], metas: &[PathBuf]) -> Result<std::process::Output> {
    let mut c = Command::new("pacman");
    c.arg("--config").arg(conf).args(args).args(metas);
    let o = c.output().context("run pacman")?;
    Ok(o)
}

/// Give each path under `root` the modification time it has in the snapshot (paths missing on
/// either side are skipped). Never follows a symlink.
fn restore_mtimes(snap: &Path, root: &Path, paths: &[&str]) -> Result<u64> {
    let s = fsx::Root::open_existing(snap)?;
    let r = fsx::Root::open_existing(root)?;
    let mut n = 0;
    for p in paths {
        let p = p.trim_end_matches('/').as_bytes();
        let (Ok((sfd, sname)), Ok((rfd, rname))) = (s.existing(p), r.existing(p)) else { continue };
        let mut stt: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(sfd.as_raw_fd(), sname.as_ptr(), &mut stt, libc::AT_SYMLINK_NOFOLLOW) } != 0 { continue; }
        let ts = [libc::timespec { tv_sec: stt.st_atime, tv_nsec: stt.st_atime_nsec }, libc::timespec { tv_sec: stt.st_mtime, tv_nsec: stt.st_mtime_nsec }];
        if unsafe { libc::utimensat(rfd.as_raw_fd(), rname.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } == 0 { n += 1; }
    }
    Ok(n)
}

struct Target { name: String, repo: usize, old: DbEntry, new: DbEntry, file: String, idx: Index }

#[derive(Default, Debug)]
pub(crate) struct UpgradeStats { pub phases: Vec<(&'static str, f64)>, pub targets: u64, pub left_to_pacman: u64, pub fetched_bytes: u64, pub index_bytes: u64, pub reused_chunks: u64, pub installed_chunks: u64, pub fetched_chunks: u64, pub removed_files: u64, pub snapshot: Option<u32> }

pub(crate) struct UpgradeOpts { pub conf: PathBuf, pub root: PathBuf, pub keys: Vec<PathBuf>, pub snapshot: bool, pub h2c: bool, pub parallel: Option<usize>, pub dry_run: bool, pub list_only: bool, pub fail_after: Option<usize>, pub use_encoded: bool }

fn log(s: &str) { eprintln!(":: {s}"); }

pub(crate) fn upgrade(want: &[String], o: &UpgradeOpts) -> Result<UpgradeStats> {
    let mut st = UpgradeStats::default();
    let mut t = std::time::Instant::now();
    let mut lap = |st: &mut UpgradeStats, name: &'static str| { st.phases.push((name, t.elapsed().as_secs_f64())); t = std::time::Instant::now(); };
    let conf = read_conf(&o.conf)?;
    let local = read_local(&conf.db_path)?;
    let mut sync: BTreeMap<String, (usize, DbEntry)> = BTreeMap::new();
    for (i, r) in conf.repos.iter().enumerate() {
        let p = conf.db_path.join("sync").join(format!("{}.db", r.name));
        let Ok(es) = read_sync_db(&p) else { continue };
        for e in es { sync.entry(e.name.clone()).or_insert((i, e)); }
    }
    // 1. targets
    let mut cands = vec![];
    let mut no_data: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, (old, _)) in &local {
        if !want.is_empty() && !want.contains(name) { continue; }
        let Some((ri, new)) = sync.get(name) else { continue };
        if cache::vercmp(&new.version, &old.version).is_le() { continue; }
        if new.field("btrpkg.index").is_none() || new.field("btrpkg.id").is_none() {
            no_data.entry(conf.repos[*ri].name.as_str()).or_default().push(name.as_str());
            st.left_to_pacman += 1;
            continue;
        }
        cands.push((name.clone(), *ri, old.clone(), new.clone()));
    }
    for w in want { ensure!(local.contains_key(w), "{w} is not installed"); }
    for (repo, names) in &no_data {
        log(&format!("left to pacman, no btrpkg data in [{repo}]: {} package(s){}", names.len(),
            if names.len() <= 5 || !want.is_empty() { format!(" ({})", names.join(" ")) } else { String::new() }));
    }
    if cands.is_empty() { log("nothing to upgrade through btrpkg"); return Ok(st); }
    let keyring_dirs: Vec<PathBuf> = if o.keys.is_empty() { sign::DEFAULT_KEY_DIRS.iter().map(PathBuf::from).collect() } else { o.keys.clone() };
    let keyring = sign::Keyring::load(&keyring_dirs)?;

    // 2./3. indexes, trust, eligibility
    let mut ns = net::NetStats::default();
    let mut targets: Vec<Target> = vec![];
    let mut files: Vec<String> = Vec::new();
    for (name, _, _, new) in &cands {
        let rel = new.field("btrpkg.index").unwrap();
        files.push(store::index_name(rel).with_context(|| format!("{name}: odd btrpkg.index {rel:?}"))?.to_string());
    }
    // all indexes of a repository at once, server after server for what is still missing
    let mut fetched: Vec<Option<(Vec<u8>, Option<String>)>> = (0..cands.len()).map(|_| None).collect();
    for ri in 0..conf.repos.len() {
        let mut left: Vec<usize> = (0..cands.len()).filter(|&k| cands[k].1 == ri).collect();
        for s in &conf.repos[ri].servers {
            if left.is_empty() { break; }
            let src = net::Source::parse(s, o.h2c, o.parallel);
            let names: Vec<&str> = left.iter().map(|&k| files[k].as_str()).collect();
            let res = store::fetch_indexes(&src, &names, &mut ns);
            let mut still = Vec::new();
            for (&k, r) in left.iter().zip(res) {
                match r {
                    Ok(x) => fetched[k] = Some(x),
                    Err(e) => { log(&format!("{}: {s}: {e:#}", cands[k].0)); still.push(k); }
                }
            }
            left = still;
        }
    }
    for ((name, ri, old, new), (file, got)) in cands.into_iter().zip(files.into_iter().zip(fetched)) {
        let id = store::unhex(new.field("btrpkg.id").unwrap())?;
        let (ib, sig) = got.with_context(|| format!("{name}: no server has its index"))?;
        let trust = sign::Trust::All(vec![sign::Trust::Id(id), sign::Trust::Signed(keyring.clone(), sig.clone().with_context(|| format!("{name}: index not signed"))?)]);
        let idx = read_index_checked(&mut &ib[..], &|h| trust.check(h)).with_context(|| name.clone())?;
        st.index_bytes += ib.len() as u64;
        let pi = String::from_utf8_lossy(&entry_bytes(&idx, None, b".PKGINFO")?.context("no .PKGINFO in the index's small files")?).into_owned();
        let has_install = idx.entries.iter().any(|e| e.path == b".INSTALL");
        let backup = !pkginfo_lines(&pi, "backup").is_empty();
        if has_install || backup || !old.backup.is_empty() {
            log(&format!("{name}: {}, left to pacman", if has_install { "has an install script" } else { "has backup files" }));
            st.left_to_pacman += 1;
            continue;
        }
        targets.push(Target { name, repo: ri, old, new, file, idx });
    }
    if targets.is_empty() { log("nothing to upgrade through btrpkg"); return Ok(st); }
    lap(&mut st, "databases+indexes");
    st.targets = targets.len() as u64;
    if o.list_only {
        for t in &targets { println!("{}", t.name); }
        return Ok(st);
    }
    log(&format!("upgrading through btrpkg: {}", targets.iter().map(|t| format!("{} {} -> {}", t.name, t.old.version, t.new.version)).collect::<Vec<_>>().join(", ")));

    // 4. metadata packages and pacman's dependency/conflict check (nothing changes yet)
    let work = conf.cache_dir.join(format!(".btrpkg-meta-{}", std::process::id()));
    fs::create_dir_all(&work)?;
    let cleanup_work = |w: &Path| { let _ = fs::remove_dir_all(w); };
    let res = (|| -> Result<UpgradeStats> {
        // small .MTREE/.BUILDINFO come from the index now; large ones after the pull
        let mut metas = vec![];
        for t in &targets {
            let m = work.join(format!("{}.pkg.tar.zst", t.file));
            if meta_package(&t.idx, None, &m).is_ok() { metas.push(m); } else { metas.push(PathBuf::new()); }
        }
        // 5. file conflicts against the local database
        let mut owners: HashMap<&str, Vec<&str>> = HashMap::new();
        for (n, (_, files)) in &local { for f in files { owners.entry(f.as_str()).or_default().push(n.as_str()); } }
        let tnames: HashSet<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        // owned by an installed package that isn't being upgraded here
        let other_owner = |p: &str| owners.get(p).and_then(|v| v.iter().find(|n| !tnames.contains(**n)).copied());
        let mut new_paths: HashMap<String, &str> = HashMap::new();
        let mut new_dirs: HashSet<String> = HashSet::new();
        for t in &targets {
            for e in &t.idx.entries {
                if !e.path.contains(&b'/') && e.path.starts_with(b".") { continue; }
                let mut p = String::from_utf8(e.path.clone()).context("non-UTF-8 path")?;
                if e.kind == K_DIR { p.push('/'); new_dirs.insert(p.clone()); }
                if e.kind != K_DIR {
                    if let Some(prev) = new_paths.insert(p.clone(), t.name.as_str()) {
                        bail!("file conflict: {p} is in both {prev} and {}", t.name);
                    }
                }
                if e.kind != K_DIR {
                    if let Some(own) = other_owner(&p) { bail!("file conflict: {p} ({}) is owned by {own}", t.name); }
                    if !owners.contains_key(p.as_str()) && fs::symlink_metadata(o.root.join(&p)).is_ok() {
                        bail!("file conflict: {p} ({}) exists in the filesystem", t.name);
                    }
                }
            }
        }
        lap(&mut st, "conflicts");
        // 6. pull
        // one pull per repository: all its packages share one download queue
        let caches: Vec<PathBuf> = targets.iter().map(|t| conf.cache_dir.join(format!("{}.pkg.btr", t.file))).collect();
        // already checked above against the database ID and a signature; same bytes
        let trusts: Vec<sign::Trust> = targets.iter().map(|t| sign::Trust::Id(t.idx.hdr.id())).collect();
        // the database says where each package's btrparts are (btrpkg.parts)
        let parts_of = |t: &Target| t.new.field("btrpkg.parts").unwrap_or(net::DEFAULT_PARTS).to_string();
        let mut groups: Vec<(usize, String)> = targets.iter().map(|t| (t.repo, parts_of(t))).collect();
        groups.sort(); groups.dedup();
        for (r, parts) in groups {
            let po = store::PullOpts { use_encoded: o.use_encoded, installed: Some(&o.root), fsync: false, parts: &parts, ..Default::default() };
            let src = net::Source::parse(&conf.repos[r].servers[0], o.h2c, o.parallel).parts(&parts)?;
            let items: Vec<store::PullItem> = targets.iter().enumerate().filter(|(_, t)| t.repo == r && parts_of(t) == parts)
                .map(|(k, t)| store::PullItem { name: &t.file, ib: &t.idx.bytes, cache: &caches[k], trust: &trusts[k] }).collect();
            for ps in store::pull_many(&src, &items, &po, net::NetStats::default())? {
                st.fetched_bytes += ps.fetched_bytes;
                st.reused_chunks += ps.reused_chunks;
                st.installed_chunks += ps.installed_chunks;
                st.fetched_chunks += ps.fetched_chunks;
            }
        }
        for (k, t) in targets.iter().enumerate() {
            if metas[k].as_os_str().is_empty() {
                let (_, f) = open_cache(&caches[k])?;
                let m = work.join(format!("{}.pkg.tar.zst", t.file));
                meta_package(&t.idx, Some(&f), &m)?;
                metas[k] = m;
            }
        }
        // --print stops after pacman's transaction preparation (dependencies, conflicts, replaces)
        // the cache files are on disk before any installed file refers to their extents
        store::syncfs_dir(&conf.cache_dir)?;
        lap(&mut st, "pull");
        let p = pacman(&o.conf, &["-U", "--noconfirm", "--print", "--print-format", "%n %v"], &metas)?;
        ensure!(p.status.success(), "pacman's dependency check failed:\n{}{}", String::from_utf8_lossy(&p.stdout), String::from_utf8_lossy(&p.stderr));
        lap(&mut st, "pacman-check");
        if o.dry_run { log("dry run: stopping before any change"); return Ok(std::mem::take(&mut st)); }

        // 7. snapshot
        if o.snapshot {
            let out = Command::new("snapper").args(["-c", "root", "create", "-c", "number", "-p", "-d"])
                .arg(format!("btrpkg upgrade {}", targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(" "))).output().context("run snapper")?;
            ensure!(out.status.success(), "snapper: {}", String::from_utf8_lossy(&out.stderr));
            st.snapshot = Some(String::from_utf8_lossy(&out.stdout).trim().parse().context("snapper's snapshot number")?);
            log(&format!("snapshot {}", st.snapshot.unwrap()));
            lap(&mut st, "snapshot");
        }
        // 8. place, remove, database
        let r = (|| -> Result<()> {
            for (k, t) in targets.iter().enumerate() {
                if o.fail_after == Some(k) { bail!("failure injected after {k} packages (BTRPKG_FAIL_AFTER)"); }
                install(&caches[k], &o.root, false).with_context(|| format!("placing {}", t.name))?;
            }
            // the old versions' files that no new version has and no other package owns
            let mut gone: Vec<&str> = vec![];
            for t in &targets {
                for f in &local[&t.name].1 {
                    if new_paths.contains_key(f.as_str()) || new_dirs.contains(f.as_str()) { continue; }
                    if other_owner(f).is_some() { continue; }
                    gone.push(f.as_str());
                }
            }
            lap(&mut st, "place");
            gone.sort_by(|a, b| b.len().cmp(&a.len()).then(b.cmp(a))); // deepest first
            let root = fsx::Root::open(&o.root)?;
            for f in gone {
                let is_dir = f.ends_with('/');
                let (pfd, name) = match root.existing(f.trim_end_matches('/').as_bytes()) { Ok(x) => x, Err(_) => continue };
                let r = unsafe { libc::unlinkat(pfd.as_raw_fd(), name.as_ptr(), if is_dir { libc::AT_REMOVEDIR } else { 0 }) };
                if r == 0 { st.removed_files += 1; }
                else if !is_dir { let e = io::Error::last_os_error(); if e.raw_os_error() != Some(libc::ENOENT) { bail!("remove {f}: {e}"); } }
            }
            lap(&mut st, "remove");
            let p = pacman(&o.conf, &["-U", "--dbonly", "--noconfirm"], &metas)?;
            lap(&mut st, "pacman-dbonly+hooks");
            eprint!("{}", String::from_utf8_lossy(&p.stdout));
            ensure!(p.status.success(), "pacman -U --dbonly failed:\n{}", String::from_utf8_lossy(&p.stderr));
            Ok(())
        })();
        if let Err(e) = r {
            if let Some(n) = st.snapshot {
                log(&format!("failed ({e:#}); undoing changes since snapshot {n}"));
                let u = Command::new("snapper").args(["-c", "root", "undochange", &format!("{n}..0")]).output().context("run snapper undochange")?;
                ensure!(u.status.success(), "{e:#}\nAND snapper undochange {n}..0 failed: {}", String::from_utf8_lossy(&u.stderr));
                // undochange restores contents, owners and modes, not modification times
                let mut touched: Vec<&str> = new_paths.keys().map(|s| s.as_str()).chain(new_dirs.iter().map(|s| s.as_str())).collect();
                for t in &targets { touched.extend(local[&t.name].1.iter().map(|s| s.as_str())); }
                let fixed = restore_mtimes(&o.root.join(format!(".snapshots/{n}/snapshot")), &o.root, &touched)?;
                bail!("{e:#} (undone: back to snapshot {n}, {fixed} modification times restored)");
            }
            return Err(e);
        }
        Ok(std::mem::take(&mut st))
    })();
    cleanup_work(&work);
    res
}
