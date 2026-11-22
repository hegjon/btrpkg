//! `btrpkg repo-update`: add btrpkg indexes and chunks to a pacman repository.
//!
//! For every package of the repo database whose file is in the directory: check the file against
//! the database's %SHA256SUM% and its OpenPGP signature (`gpgv` with pacman's keyring), convert it,
//! publish its btrparts (into the btrparts directory, `../btrparts` by default: beside the
//! architecture directories, which share it), signature and index (into the directory), then
//! rewrite the database with four `%DATA%` lines per package:
//!   btrpkg.index=index/NAME.btridx   btrpkg.id=<header hash>   btrpkg.size=<index + btrpart bytes>
//!   btrpkg.parts=<the btrparts directory, relative to the repository's directory>
//! pacman reads `%DATA%` silently, so the same database serves plain pacman unchanged. The
//! `.pkg.tar.zst` files stay. Order: chunks, signatures and indexes first, the database last.
//! Packages already converted from the same file (index/NAME.src holds its SHA-256) are skipped.
use super::*;
use std::sync::Mutex;

#[derive(Default, Debug)]
pub(crate) struct RepoStats { pub packages: u64, pub converted: u64, pub unchanged: u64, pub failed: u64, pub missing: u64, pub dropped: u64, pub new_chunk_bytes: u64 }

pub(crate) struct Desc { pub fields: Vec<(String, Vec<String>)> }

impl Desc {
    pub(crate) fn parse(text: &str) -> Desc {
        let mut fields: Vec<(String, Vec<String>)> = Vec::new();
        for l in text.lines() {
            if l.starts_with('%') && l.ends_with('%') && l.len() > 2 {
                fields.push((l.to_string(), vec![]));
            } else if !l.is_empty() {
                if let Some(f) = fields.last_mut() { f.1.push(l.to_string()); }
            }
        }
        Desc { fields }
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.fields.iter().find(|f| f.0 == k).and_then(|f| f.1.first()).map(|s| s.as_str())
    }
    /// Replace this tool's `%DATA%` lines, keeping any others.
    fn set_btrpkg(&mut self, lines: &[String]) {
        if let Some(f) = self.fields.iter_mut().find(|f| f.0 == "%DATA%") {
            f.1.retain(|l| !l.starts_with("btrpkg."));
            f.1.extend(lines.iter().cloned());
            if f.1.is_empty() { self.fields.retain(|f| f.0 != "%DATA%"); }
        } else if !lines.is_empty() {
            self.fields.push(("%DATA%".into(), lines.to_vec()));
        }
    }
    fn render(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.fields {
            s.push_str(k);
            s.push('\n');
            for l in v { s.push_str(l); s.push('\n'); }
            s.push('\n');
        }
        s
    }
}

pub(crate) enum Comp { Zstd, Gzip, None }

pub(crate) fn read_db(p: &Path) -> Result<(Comp, Vec<u8>)> {
    let b = fs::read(p).with_context(|| format!("read {}", p.display()))?;
    if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Ok((Comp::Zstd, zstd::stream::decode_all(&b[..])?))
    } else if b.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(&b[..]).read_to_end(&mut out)?;
        Ok((Comp::Gzip, out))
    } else {
        Ok((Comp::None, b))
    }
}

fn compress_db(c: &Comp, tar: Vec<u8>) -> Result<Vec<u8>> {
    Ok(match c {
        Comp::Zstd => zstd::bulk::compress(&tar, 19)?,
        Comp::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            e.write_all(&tar)?;
            e.finish()?
        }
        Comp::None => tar,
    })
}

fn gpgv(keyring: &Path, file: &Path) -> Result<()> {
    let sig = part_sig(file);
    ensure!(sig.exists(), "{}: no .sig", file.display());
    let out = std::process::Command::new("gpgv").arg("--keyring").arg(keyring).arg(&sig).arg(file)
        .output().context("run gpgv")?;
    ensure!(out.status.success(), "{}: OpenPGP signature check failed: {}", file.display(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

fn part_sig(p: &Path) -> PathBuf { let mut s = p.as_os_str().to_owned(); s.push(".sig"); PathBuf::from(s) }

pub(crate) struct RepoOpts<'a> {
    pub key: Option<(&'a sign::SecretKey, &'a str)>,
    /// OpenPGP keyring for the packages' signatures; None skips the check.
    pub gpg_keyring: Option<PathBuf>,
    pub drop_missing: bool,
    pub level: i32,
    pub small: u64,
    pub jobs: usize,
    /// read the database from here instead of DIR/REPO.db (a newer upstream database: DIR/REPO.db
    /// keeps serving the old one until the new one, with btrpkg data, replaces it)
    pub db_in: Option<PathBuf>,
    /// the btrparts directory, relative to DIR (clients resolve it against the server URL)
    pub parts: String,
}

/// Beside the architecture directories (`omarchy/x86_64`, `omarchy/aarch64` share
/// `omarchy/btrparts`).
pub(crate) const DEFAULT_REPO_PARTS: &str = "../btrparts";

/// Update DIR/REPO.db (following the usual REPO.db -> REPO.db.tar.zst symlink).
pub(crate) fn repo_update(dir: &Path, repo: &str, o: &RepoOpts) -> Result<RepoStats> {
    let parts = crate::parts_dir(dir, &o.parts)?;
    let link = dir.join(format!("{repo}.db"));
    let dbp = match (&o.db_in, fs::canonicalize(&link)) {
        (_, Ok(p)) => p,
        (Some(_), Err(_)) => {
            // first run from an upstream database: REPO.db -> REPO.db.tar.zst, like repo-add
            std::os::unix::fs::symlink(format!("{repo}.db.tar.zst"), &link)?;
            dir.join(format!("{repo}.db.tar.zst"))
        }
        (None, Err(e)) => return Err(e).with_context(|| format!("{repo}.db in {}", dir.display())),
    };
    let (comp, tarb) = read_db(o.db_in.as_deref().unwrap_or(&dbp))?;
    // (header, data) in the database's order
    let mut ents: Vec<(tar::Header, Vec<u8>)> = Vec::new();
    for e in tar::Archive::new(&tarb[..]).entries()? {
        let mut e = e?;
        let mut d = Vec::new();
        e.read_to_end(&mut d)?;
        ents.push((e.header().clone(), d));
    }
    let tmpdir = dir.join(".btrpkg-tmp");
    fs::create_dir_all(&tmpdir)?;
    let st = Mutex::new(RepoStats::default());
    let publish_lock = Mutex::new(());

    // what to do for each desc: Some(new desc text) or None (drop)
    let work: Vec<usize> = ents.iter().enumerate().filter(|(_, (h, _))| h.path_bytes().ends_with(b"/desc")).map(|(i, _)| i).collect();
    // Packages are converted `o.jobs` at a time (each conversion holds its package in memory and
    // compresses its chunks on all cores).
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let one = |i: usize| -> (usize, Option<Vec<u8>>) {
        let mut d = Desc::parse(&String::from_utf8_lossy(&ents[i].1));
        let (Some(fname), Some(want)) = (d.get("%FILENAME%").map(str::to_string), d.get("%SHA256SUM%").map(str::to_string)) else {
            return (i, Some(ents[i].1.clone()));
        };
        st.lock().unwrap().packages += 1;
        let src = dir.join(&fname);
        if !src.exists() {
            let mut s = st.lock().unwrap();
            s.missing += 1;
            if o.drop_missing { s.dropped += 1; return (i, None); }
            return (i, Some(ents[i].1.clone()));
        }
        let name = fname.trim_end_matches(".pkg.tar.zst").trim_end_matches(".pkg.tar.xz").to_string();
        let r = (|| -> Result<(Hash, u64, bool)> {
            let ip = dir.join("index").join(format!("{name}.btridx"));
            let srcp = dir.join("index").join(format!("{name}.src"));
            if fs::read_to_string(&srcp).is_ok_and(|s| s.trim() == want) && ip.exists() {
                let ib = fs::read(&ip)?;
                let idx = parse_index(&ib)?;
                return Ok((idx.hdr.id(), idx.hdr.transport_len(&idx.chunks), false));
            }
            let got = hex(&sha256(&fs::read(&src)?));
            ensure!(got == want, "{fname}: sha256 {got} doesn't match the database's {want}");
            if let Some(k) = &o.gpg_keyring { gpgv(k, &src)?; }
            let tmp = tmpdir.join(format!("{name}.pkg.btr"));
            convert(&src, &tmp, o.level, 19, o.small, 1, 1 << 20)?;
            let (_, ps) = {
                let _g = publish_lock.lock().unwrap();
                store::publish(&tmp, dir, &parts, o.key)
            }?;
            let _ = fs::remove_file(&tmp);
            let ib = fs::read(&ip)?;
            let idx = parse_index(&ib)?;
            store::write_atomic(&srcp, format!("{want}\n").as_bytes())?;
            st.lock().unwrap().new_chunk_bytes += ps.new_bytes;
            Ok((idx.hdr.id(), idx.hdr.transport_len(&idx.chunks), true))
        })();
        match r {
            Ok((id, size, converted)) => {
                let mut s = st.lock().unwrap();
                if converted { s.converted += 1 } else { s.unchanged += 1 }
                d.set_btrpkg(&[format!("btrpkg.index=index/{name}.btridx"), format!("btrpkg.id={}", hex(&id)), format!("btrpkg.size={size}"),
                    format!("btrpkg.parts={}", o.parts)]);
            }
            Err(e) => {
                eprintln!("btrpkg: {name}: {e:#} (left to pacman)");
                st.lock().unwrap().failed += 1;
                d.set_btrpkg(&[]);
            }
        }
        (i, Some(d.render().into_bytes()))
    };
    std::thread::scope(|s| {
        for _ in 0..o.jobs.max(1) {
            s.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(&i) = work.get(k) else { break };
                let r = one(i);
                results.lock().unwrap().push(r);
            });
        }
    });
    let results = results.into_inner().unwrap();
    let _ = fs::remove_dir(&tmpdir);

    // rewrite the database: same entries and order, changed descs, dropped packages left out
    let mut drop_dirs = std::collections::HashSet::new();
    for (i, r) in &results {
        let p = ents[*i].0.path_bytes().into_owned();
        match r {
            Some(d) => ents[*i].1 = d.clone(),
            None => { drop_dirs.insert(p[..p.len() - "desc".len()].to_vec()); }
        }
    }
    let mut b = tar::Builder::new(Vec::new());
    for (h, d) in &ents {
        let p = h.path_bytes();
        if drop_dirs.iter().any(|dd| p.starts_with(dd)) { continue; }
        let mut h = h.clone();
        h.set_size(d.len() as u64);
        h.set_cksum();
        b.append(&h, &d[..])?;
    }
    let newdb = compress_db(&comp, b.into_inner()?)?;
    store::write_atomic(&dbp, &newdb)?;
    let sig = part_sig(&dbp);
    if sig.exists() {
        eprintln!("btrpkg: removed {}: it signed the old database (sign the new one again)", sig.display());
        fs::remove_file(sig)?;
    }
    Ok(st.into_inner().unwrap())
}
