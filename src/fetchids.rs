//! `fetch-ids`: the network half of a btrpkg download, meant to run unprivileged (pacman's
//! DownloadUser, seccomp, and Landlock applied here: reads anywhere, writes only to DIR, no exec).
//!
//! It writes a directory store into DIR: `index/NAME.btridx` (+ `.sig`) and
//! `chunks/xx/HASH` for every chunk that can't come from the machine. Whether a chunk can come
//! from the machine is only estimated here (an older cache file of the package lists it, or the
//! installed file it belongs to is long enough); nothing local is hashed, because the privileged
//! second half (`pull-ids DIR CACHEDIR`) has to check everything it reuses anyway. If that check
//! fails for some chunk (a locally changed file), the second half names the missing chunks and
//! this runs again with `--want FILE` to fetch exactly those.
//!
//! Servers are tried in order (pacman's CacheServer entries first, then Server): whatever one
//! server didn't deliver is asked from the next. Chunks already in DIR are not fetched again, so
//! an interrupted download continues where it stopped.
//!
//! With `--progress`, stdout carries one line per event for the caller (libalpm):
//!   total NAME BYTES      bytes this package needs from the network (after the estimate)
//!   got NAME BYTES        cumulative bytes received for NAME
//!   done NAME             NAME complete
use crate::format::*;
use crate::{net, sign, store};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) struct FetchOpts<'a> {
    pub installed: Option<&'a Path>,
    pub cachedir: Option<&'a Path>,
    pub want: Option<Vec<String>>,
    pub progress: bool,
    /// only the indexes (and signatures)
    pub index_only: bool,
}

/// Restrict this process: read the whole filesystem, write and create only under `dir`, run
/// nothing. Network access stays (Landlock's network rules aren't used).
pub(crate) fn landlock_writes_to(dir: &Path) -> Result<()> {
    use landlock::{Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI, path_beneath_rules};
    let abi = ABI::V3;
    let st = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi) & !AccessFs::Execute))?
        .add_rules(path_beneath_rules([dir], AccessFs::from_all(abi) & !AccessFs::Execute))?
        .restrict_self()
        .context("landlock")?;
    ensure!(st.ruleset != RulesetStatus::NotEnforced, "Landlock is not available in this kernel");
    Ok(())
}

fn say(on: bool, line: String) {
    if on {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }
}

fn chunk_rel(stored: &Hash) -> String {
    let x = hex(stored);
    format!("chunks/{}/{x}", &x[..2])
}

pub(crate) fn fetch_ids(servers: &[net::Source], dir: &Path, pkgs: &[(String, Hash)], o: &FetchOpts) -> Result<()> {
    ensure!(!servers.is_empty(), "no server");
    let mut ns = net::NetStats::default();
    // 1. indexes (and signatures), each checked against its ID before it is kept
    let mut idxs: Vec<Index> = Vec::new();
    for (name, id) in pkgs {
        let ip = dir.join("index").join(format!("{name}.btridx"));
        let trust = sign::Trust::Id(*id);
        if let Ok(b) = fs::read(&ip) {
            if let Ok(idx) = read_index_checked(&mut &b[..], &|h| trust.check(h)) { idxs.push(idx); continue; }
        }
        let mut got = None;
        for s in servers {
            match store::fetch_index(s, name, &mut ns) {
                Ok((ib, sig)) => {
                    match read_index_checked(&mut &ib[..], &|h| trust.check(h)) {
                        Ok(idx) => { got = Some((idx, ib, sig)); break; }
                        Err(e) => eprintln!("btrpkg: {name} from {}: {e:#}", s.describe()),
                    }
                }
                Err(e) => eprintln!("btrpkg: {name} from {}: {e:#}", s.describe()),
            }
        }
        let (idx, ib, sig) = got.with_context(|| format!("{name}: no server has its index"))?;
        store::write_atomic(&ip, &ib)?;
        let sp = dir.join("index").join(format!("{name}.btridx.sig"));
        match sig { Some(t) => store::write_atomic(&sp, t.as_bytes())?, None => { let _ = fs::remove_file(&sp); } }
        idxs.push(idx);
    }

    if o.index_only { return Ok(()); }

    // 2. which chunks to fetch: (store path, length, owning package)
    let mut need: Vec<(String, usize, usize)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let want: Option<HashSet<&str>> = o.want.as_ref().map(|w| w.iter().map(|s| s.as_str()).collect());
    for (j, idx) in idxs.iter().enumerate() {
        let local: HashSet<(Hash, u32)> = match (o.cachedir, &want) {
            (Some(cd), None) => cached_chunks(cd, &pkgs[j].0),
            _ => HashSet::new(),
        };
        let files = store::chunk_files(idx);
        for c in &idx.chunks {
            let rel = chunk_rel(&c.stored);
            if let Some(w) = &want { if !w.contains(rel.as_str()) { continue; } }
            else {
                if local.contains(&(c.raw, c.unencoded_len)) { continue; }
                if let (Some(root), Some((path, foff, dlen))) = (o.installed, files.get(&c.image_off)) {
                    let p = root.join(std::ffi::OsStr::from_bytes(path));
                    if fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && m.size() >= foff + dlen) { continue; }
                }
            }
            if !seen.insert(rel.clone()) { continue; }
            if fs::symlink_metadata(dir.join(&rel)).is_ok_and(|m| m.is_file() && m.len() == c.encoded_len as u64) { continue; }
            need.push((rel, c.encoded_len as usize, j));
        }
    }
    let mut total = vec![0u64; pkgs.len()];
    for (_, len, j) in &need { total[*j] += *len as u64; }
    for (j, (name, _)) in pkgs.iter().enumerate() { say(o.progress, format!("total\t{name}\t{}", total[j])); }

    // 3. fetch, server after server; every chunk is checked against its stored hash first
    let by_rel: HashMap<String, Chunk> = idxs.iter()
        .flat_map(|idx| idx.chunks.iter().map(|c| (chunk_rel(&c.stored), *c))).collect();
    let mut got = vec![0u64; pkgs.len()];
    let mut done: HashSet<String> = HashSet::new();
    let mut left: Vec<(String, usize, usize)> = need;
    for (k, s) in servers.iter().enumerate() {
        if left.is_empty() { break; }
        let items: Vec<(String, usize)> = left.iter().map(|(r, l, _)| (r.clone(), *l)).collect();
        let r = s.get_many(&items, &mut ns, |batch| {
            for (i, d) in batch {
                let (rel, _, j) = &left[i];
                let c = &by_rel[rel.as_str()];
                check_chunk(0, c, &d).with_context(|| format!("{rel} from {}", s.describe()))?;
                store::write_atomic_nosync(&dir.join(rel), &d)?;
                got[*j] += d.len() as u64;
                done.insert(rel.clone());
            }
            for (j, (name, _)) in pkgs.iter().enumerate() { if total[j] > 0 { say(o.progress, format!("got\t{name}\t{}", got[j])); } }
            Ok(())
        });
        if let Err(e) = r {
            if k + 1 < servers.len() { eprintln!("btrpkg: {}: {e:#}; trying the next server", s.describe()); }
            else { eprintln!("btrpkg: {}: {e:#}", s.describe()); }
        }
        left.retain(|(r, _, _)| !done.contains(r));
    }
    if !left.is_empty() {
        bail!("{} chunks not available from any server (first: {})", left.len(), left[0].0);
    }
    for (name, _) in pkgs { say(o.progress, format!("done\t{name}")); }
    Ok(())
}

/// (unencoded hash, length) of every chunk in older cache files of NAME's package.
fn cached_chunks(cachedir: &Path, name: &str) -> HashSet<(Hash, u32)> {
    let want = store::pkgname(name);
    let mut s = HashSet::new();
    let Ok(rd) = fs::read_dir(cachedir) else { return s };
    for e in rd.flatten() {
        let p: PathBuf = e.path();
        let Some(stem) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".pkg.btr")) else { continue };
        if store::pkgname(stem) != want { continue; }
        if let Some((_, _, chunks)) = store::cache_head(&p) {
            for c in chunks { s.insert((c.raw, c.unencoded_len)); }
        }
    }
    s
}
