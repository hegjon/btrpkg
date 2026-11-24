//! `btrpkg gc`: remove old package versions from the repositories that share one btrparts
//! directory, then the btrparts no kept index names.
//!
//! A btrparts directory can be shared (`omarchy/pkg` by `omarchy/x86_64` and
//! `omarchy/aarch64`; `arch/pkg` by core and extra), so a btrpart can only go when no index
//! of ANY of those repositories names it. The repositories are found under the btrparts
//! directory's parent (a directory with `*.btrpkg` indexes and one REPO.db), every one of them
//! takes part in the mark, and a repository whose database points its btrpkg entries somewhere
//! else is marked but not pruned.
//!
//! Per package name in a repository, kept are: the version the database names (current), every
//! version newer than it (published, database not swapped yet), and the `keep - 1` newest versions
//! older than it (clients with an older database). A package the database no longer has keeps its
//! `keep - 1` newest versions. A pruned version loses its index (`NAME.btrpkg`), its `.sig` and `.src` and, in a
//! mirror, its `.pkg.tar.*` file and `.sig`. Then every btrpart in the directory that no kept index
//! names goes.
//!
//! Safety: nothing younger than `min_age` is removed (a publish writes btrparts before its index,
//! an index before the database); every repository's `.lock` (the mirror's, see
//! mirror/repo-mirror.sh) is held for the whole run, and a busy one stops the run; an index that
//! can't be read stops the run before anything is removed. Indexes go before btrparts, so a run
//! that stops halfway leaves btrparts behind, never an index without its btrparts.
use super::*;
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime};

#[derive(Default, Debug)]
pub(crate) struct GcStats {
    pub repos: usize,
    pub indexes_kept: usize,
    pub indexes_removed: usize,
    pub pkg_files_removed: usize,
    pub parts_kept: usize,
    pub parts_removed: usize,
    pub parts_young: usize,
    pub bytes_removed: u64,
}

pub(crate) struct GcOpts {
    pub keep: usize,
    pub min_age: Duration,
    pub dry_run: bool,
}

struct Repo {
    dir: PathBuf,
    /// index stems (NAME-VER-REL-ARCH) the database names
    current: HashSet<String>,
    /// prune it (its btrpkg entries use this btrparts directory)
    prune: bool,
}

/// The repositories under `root` (depth ≤ 5): directories with `*.btrpkg` indexes (and one
/// non-dot `*.db`, checked by read_repo).
pub(crate) fn find_repos(root: &Path, parts: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 5 || root == parts { return Ok(()); }
    let mut is_repo = false;
    // errors stop gc here, before anything is removed: a repository it can't see isn't marked
    for e in fs::read_dir(root).with_context(|| root.display().to_string())? {
        let e = e?;
        let n = e.file_name();
        if n.to_string_lossy().starts_with('.') { continue; }
        let t = e.file_type()?;
        if t.is_file() && store::index_name(&n.to_string_lossy()).is_some() { is_repo = true; }
        // real directories only: a symlinked repository would be marked twice, which is harmless,
        // but a symlink loop isn't
        if t.is_dir() { find_repos(&e.path(), parts, depth + 1, out)?; }
    }
    if is_repo { out.push(root.to_path_buf()); }
    Ok(())
}

fn read_repo(dir: &Path, parts: &Path) -> Result<Repo> {
    let dbs: Vec<PathBuf> = fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?
        .into_iter().filter(|p| p.extension().is_some_and(|x| x == "db") && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    ensure!(dbs.len() == 1, "{}: {} databases (*.db), expected one", dir.display(), dbs.len());
    let (_, tarb) = repo::read_db(&dbs[0])?;
    let mut current = HashSet::new();
    let (mut ours, mut other) = (0usize, 0usize);
    for d in repo::descs(&tarb)? {
        let Some(idx) = d.data("btrpkg.index") else { continue };
        let stem = store::index_name(idx).with_context(|| format!("{}: odd btrpkg.index {idx:?}", dbs[0].display()))?;
        current.insert(stem.to_string());
        let p = d.data("btrpkg.parts").unwrap_or(net::DEFAULT_PARTS);
        if crate::parts_dir(dir, p).ok().and_then(|q| q.canonicalize().ok()).as_deref() == Some(parts) { ours += 1 } else { other += 1 }
    }
    if other > 0 || ours == 0 {
        eprintln!("btrpkg gc: {}: {ours} entries use this btrparts directory, {other} others: marked, not pruned", dir.display());
    }
    Ok(Repo { dir: dir.to_path_buf(), current, prune: ours > 0 && other == 0 })
}

fn young(m: &fs::Metadata, o: &GcOpts, now: SystemTime) -> bool {
    m.modified().map(|t| now.duration_since(t).unwrap_or_default() < o.min_age).unwrap_or(true)
}

/// Remove P (if there) and count it; `dry` only counts.
pub(crate) fn remove(p: &Path, dry: bool, bytes: &mut u64) -> Result<bool> {
    let Ok(m) = fs::symlink_metadata(p) else { return Ok(false) };
    *bytes += m.blocks() * 512;
    if !dry { fs::remove_file(p).with_context(|| p.display().to_string())?; }
    Ok(true)
}

pub(crate) fn gc(parts: &Path, o: &GcOpts) -> Result<GcStats> {
    ensure!(o.keep >= 1, "--keep must be at least 1 (the current version)");
    let parts = parts.canonicalize().with_context(|| parts.display().to_string())?;
    let root = parts.parent().context("btrparts directory has no parent")?;
    let mut dirs = Vec::new();
    find_repos(root, &parts, 0, &mut dirs)?;
    ensure!(!dirs.is_empty(), "no repository (a directory with *.btrpkg indexes) under {}", root.display());
    // hold every repository's lock (a mirror run holds it while it publishes)
    let mut locks = Vec::new();
    for d in &dirs {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(d.join(".lock"))?;
        ensure!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "{}: busy (a mirror run holds .lock); try again later", d.display());
        locks.push(f);
    }
    let repos: Vec<Repo> = dirs.iter().map(|d| read_repo(d, &parts)).collect::<Result<_>>()?;
    let now = SystemTime::now();
    let mut st = GcStats { repos: repos.len(), ..Default::default() };

    // 1. per repository: which index stems stay
    let mut doomed: Vec<(PathBuf, String)> = Vec::new(); // (repo dir, stem)
    let mut kept: Vec<PathBuf> = Vec::new();             // index files to mark from
    for r in &repos {
        let mut by_name: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new(); // name -> (ver, stem)
        for e in fs::read_dir(&r.dir)? {
            let e = e?; // an index not seen would not be marked
            let n = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = store::index_name(&n) else { continue };
            if !e.file_type()?.is_file() { continue; }
            match store::split_stem(stem) {
                Some((name, ver)) => by_name.entry(name.to_string()).or_default().push((ver.to_string(), stem.to_string())),
                None => kept.push(e.path()), // not NAME-VER-REL-ARCH: keep it, never guess
            }
        }
        for (_, mut v) in by_name {
            v.sort_by(|a, b| cache::vercmp(&b.0, &a.0)); // newest first
            let cur = v.iter().position(|(_, s)| r.current.contains(s));
            let mut older_left = o.keep - 1;
            for (k, (_, stem)) in v.iter().enumerate() {
                let keep = match cur {
                    Some(c) if k <= c => true, // current, or newer than it
                    _ => if older_left > 0 { older_left -= 1; true } else { false },
                };
                let ip = r.dir.join(store::index_rel(stem));
                let too_young = fs::metadata(&ip).map(|m| young(&m, o, now)).unwrap_or(true);
                if keep || !r.prune || too_young { kept.push(ip); } else { doomed.push((r.dir.clone(), stem.clone())); }
            }
        }
    }

    // 2. mark: every btrpart a kept index names, in any repository (all read before anything goes)
    let mut live: HashSet<Hash> = HashSet::new();
    for ip in &kept {
        let b = fs::read(ip).with_context(|| ip.display().to_string())?;
        let idx = parse_index(&b).with_context(|| format!("{}: unreadable index, nothing removed", ip.display()))?;
        live.extend(idx.chunks.iter().map(|c| c.stored));
    }
    st.indexes_kept = kept.len();

    // 3. old versions: index, signature, and the mirror's package file (+ .sig, .sha256sum)
    for (dir, stem) in &doomed {
        println!("{}{}", if o.dry_run { "would remove " } else { "removed " }, dir.join(store::index_rel(stem)).display());
        for f in [store::sig_rel(stem), store::index_rel(stem)] {
            remove(&dir.join(f), o.dry_run, &mut st.bytes_removed)?;
        }
        st.indexes_removed += 1;
        for ext in ["zst", "xz", "gz"] {
            let p = dir.join(format!("{stem}.pkg.tar.{ext}"));
            if remove(&p, o.dry_run, &mut st.bytes_removed)? { st.pkg_files_removed += 1; }
            remove(&dir.join(format!("{stem}.pkg.tar.{ext}.sig")), o.dry_run, &mut st.bytes_removed)?;
            remove(&dir.join(store::sum_rel(&format!("{stem}.pkg.tar.{ext}"))), o.dry_run, &mut st.bytes_removed)?;
        }
    }

    // 4. sweep btrparts
    let is_hash = |n: &str| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    for sub in fs::read_dir(&parts)?.flatten() {
        let sn = sub.file_name().to_string_lossy().into_owned();
        if sn.len() != 2 || !sub.file_type().is_ok_and(|t| t.is_dir()) { continue; }
        for e in fs::read_dir(sub.path())?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !is_hash(&n) { continue; } // .tmp files of a running publish and the like
            if live.contains(&store::unhex(&n)?) { st.parts_kept += 1; continue; }
            let m = e.metadata()?;
            if young(&m, o, now) { st.parts_young += 1; continue; }
            remove(&e.path(), o.dry_run, &mut st.bytes_removed)?;
            st.parts_removed += 1;
        }
    }
    if !o.dry_run && st.parts_removed > 0 { store::syncfs_dir(&parts)?; }
    drop(locks);
    Ok(st)
}
