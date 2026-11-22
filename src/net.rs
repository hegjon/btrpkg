//! Where `pull` reads a store from: a directory, or an HTTP(S) URL fetched with libcurl's multi
//! interface (the same library pacman downloads with). Over HTTP/2 many requests share one
//! connection; over HTTP/1.1 each request in flight needs its own connection.
use anyhow::{bail, ensure, Context, Result};
use curl::easy::{Easy2, Handler, HttpVersion, WriteError};
use curl::multi::{Easy2Handle, Multi};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub enum Source {
    Dir(PathBuf),
    /// base URL without trailing slash; `h2c`: HTTP/2 over plain http (prior knowledge)
    Http { base: String, h2c: bool, parallel: usize },
}

/// Process-wide HTTP settings from the command line (`--http`, `--cacert`).
#[derive(Default, Debug, Clone)]
pub struct NetOptions {
    /// "h1", "h2" or "h3"; None: HTTP/2 over https, HTTP/1.1 over http (h2c with --h2c)
    pub version: Option<String>,
    /// CA certificates to trust instead of the system's
    pub cainfo: Option<String>,
}
static OPTIONS: std::sync::OnceLock<NetOptions> = std::sync::OnceLock::new();
pub fn set_options(o: NetOptions) -> Result<()> {
    if let Some(v) = &o.version { ensure!(matches!(v.as_str(), "h1" | "h2" | "h3"), "--http wants h1, h2 or h3"); }
    OPTIONS.set(o).map_err(|_| anyhow::anyhow!("network options set twice"))
}
fn options() -> &'static NetOptions { OPTIONS.get_or_init(NetOptions::default) }

#[derive(Default, Debug, Clone)]
pub struct NetStats {
    pub requests: u64,
    pub bytes: u64,
    pub retries: u64,
    /// objects fetched without their trailing zeros (HTTP Range), and the zero bytes not sent
    pub ranged: u64,
    pub saved: u64,
}

struct Collector { buf: Vec<u8>, max: usize }
impl Handler for Collector {
    fn write(&mut self, data: &[u8]) -> std::result::Result<usize, WriteError> {
        if self.buf.len() + data.len() > self.max { return Ok(0); } // aborts the transfer
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
}

const ATTEMPTS: u32 = 3;
const BATCH_BYTES: usize = 16 << 20;

/// Chunks a directory store doesn't have (store paths), from `get_many` on a directory.
#[derive(Debug)]
pub struct AbsentChunks(pub Vec<String>);
impl std::fmt::Display for AbsentChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} chunks missing from the directory (first: {})", self.0.len(), self.0.first().map(|s| s.as_str()).unwrap_or("-"))
    }
}
impl std::error::Error for AbsentChunks {}

/// Read REL from directory store D: no symlinks anywhere below D, regular files only (the
/// directory may have been written by a less trusted process).
fn read_local(d: &std::path::Path, rel: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(d)?;
    let fd = crate::fsx::open_beneath(dir.as_raw_fd(), rel.as_bytes(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC)?;
    let mut f = std::fs::File::from(fd);
    if !f.metadata()?.is_file() { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a regular file")); }
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    Ok(v)
}

/// Where a store's btrparts are, relative to it, when nothing says otherwise: inside the store.
/// A repository can put them elsewhere (`btrpkg.parts` in its database, e.g. `../btrparts`
/// beside the architecture directories, shared by all of them). Directory stores written on
/// the client (pacman's download directory) always use this.
pub const DEFAULT_PARTS: &str = "btrparts";

impl Source {
    /// The btrparts of this store: REL resolved against it. REL is a relative path
    /// (`btrparts`, `../btrparts`); `..` is resolved here and can't go above the URL's host or
    /// the directory's root. Btrparts are checked against the signed index that names them, so
    /// where they come from is no matter of trust.
    pub fn parts(&self, rel: &str) -> Result<Source> {
        ensure!(!rel.is_empty() && !rel.starts_with('/') && !rel.contains("://")
            && rel.bytes().all(|b| b.is_ascii_graphic() && b != b'\\' && b != b'?' && b != b'#' && b != b'%'),
            "bad btrparts path {rel:?}");
        let resolve = |mut segs: Vec<String>| -> Result<Vec<String>> {
            for c in rel.split('/') {
                match c {
                    "" | "." => {}
                    ".." => { ensure!(segs.pop().is_some(), "btrparts path {rel:?} leaves the store's root"); }
                    x => segs.push(x.to_string()),
                }
            }
            Ok(segs)
        };
        Ok(match self {
            Source::Dir(d) => {
                // lexically, like the URL: relative to the working directory, `..` in the
                // directory's own path resolved too
                let d = if d.is_absolute() { d.clone() } else { std::env::current_dir()?.join(d) };
                let mut segs: Vec<String> = Vec::new();
                for c in d.components() {
                    match c {
                        std::path::Component::Normal(s) => segs.push(s.to_string_lossy().into_owned()),
                        std::path::Component::ParentDir => { segs.pop(); }
                        _ => {}
                    }
                }
                let mut p = PathBuf::from("/");
                for s in resolve(segs)? { p.push(s); }
                Source::Dir(p)
            }
            Source::Http { base, h2c, parallel } => {
                let after = base.find("://").map(|i| i + 3).unwrap_or(0);
                let (origin, path) = match base[after..].find('/') { Some(i) => base.split_at(after + i), None => (base.as_str(), "") };
                let segs = resolve(path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect())?;
                let base = if segs.is_empty() { origin.to_string() } else { format!("{origin}/{}", segs.join("/")) };
                Source::Http { base, h2c: *h2c, parallel: *parallel }
            }
        })
    }

    /// For messages: the base URL or directory.
    pub fn describe(&self) -> String {
        match self { Source::Dir(d) => d.display().to_string(), Source::Http { base, .. } => base.clone() }
    }

    pub fn parse(s: &str, h2c: bool, parallel: Option<usize>) -> Source {
        if s.starts_with("http://") || s.starts_with("https://") {
            // HTTP/2 and 3 multiplex; HTTP/1.1 opens a connection per request in flight
            let tls = s.starts_with("https://");
            let h2c = h2c || (!tls && options().version.as_deref() == Some("h2"));
            let h2 = match options().version.as_deref() { Some("h1") => false, Some(_) => true, None => h2c || tls };
            Source::Http { base: s.trim_end_matches('/').to_string(), h2c, parallel: parallel.unwrap_or(if h2 { 64 } else { 8 }) }
        } else {
            // pacman's Server lines use file:// for local repositories
            Source::Dir(PathBuf::from(s.strip_prefix("file://").unwrap_or(s)))
        }
    }

    fn easy(&self, rel: &str, max: usize) -> Result<Easy2<Collector>> {
        let Source::Http { base, h2c, .. } = self else { unreachable!() };
        let mut e = Easy2::new(Collector { buf: Vec::new(), max });
        e.url(&format!("{base}/{rel}"))?;
        let tls = base.starts_with("https://");
        e.http_version(match (options().version.as_deref(), tls) {
            (Some("h1"), _) => HttpVersion::V11,
            (Some("h3"), true) => HttpVersion::V3,
            _ if *h2c => HttpVersion::V2PriorKnowledge,
            (_, true) => HttpVersion::V2TLS,
            _ => HttpVersion::V11,
        })?;
        if let Some(ca) = &options().cainfo { e.cainfo(ca)?; }
        e.pipewait(true)?;
        e.connect_timeout(Duration::from_secs(10))?;
        // give up on a transfer slower than 1 KB/s for 30 s (pacman's own rule is 1 B/s for 10 s)
        e.low_speed_limit(1024)?;
        e.low_speed_time(Duration::from_secs(30))?;
        e.useragent(concat!("btrpkg/", env!("CARGO_PKG_VERSION")))?;
        Ok(e)
    }

    /// One object of at most `max` bytes; None if it doesn't exist.
    pub fn get(&self, rel: &str, max: usize, st: &mut NetStats) -> Result<Option<Vec<u8>>> {
        match self {
            Source::Dir(d) => match read_local(d, rel) {
                Ok(b) => { ensure!(b.len() <= max, "{rel}: too large"); Ok(Some(b)) }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("read {rel}")),
            },
            Source::Http { .. } => {
                let mut last = None;
                for _ in 0..ATTEMPTS {
                    let e = self.easy(rel, max)?;
                    e.perform().map_err(anyhow::Error::from).ok();
                    st.requests += 1;
                    let code = e.response_code().unwrap_or(0);
                    match code {
                        200 => { st.bytes += e.get_ref().buf.len() as u64; return Ok(Some(e.get_ref().buf.clone())); }
                        404 | 410 => return Ok(None),
                        c => { last = Some(c); st.retries += 1; std::thread::sleep(Duration::from_millis(300)); }
                    }
                }
                bail!("{rel}: HTTP {}", last.unwrap_or(0))
            }
        }
    }

    /// Many objects of known exact length, as (path, length, data length): bytes after the data
    /// length are zeros (a raw btrpart's padding after the end of its file), so over HTTP only
    /// the data is asked for (`Range`) and the zeros are put back here. A server that ignores the
    /// range sends everything, which is fine too. Callers check every object against its SHA-256
    /// (its name), so a wrong reconstruction can't get through. `f` gets the objects in batches
    /// of about 16 MB, in no particular order, as (item index, bytes).
    pub fn get_many(&self, items: &[(String, usize, usize)], st: &mut NetStats, mut f: impl FnMut(Vec<(usize, Vec<u8>)>) -> Result<()>) -> Result<()> {
        match self {
            Source::Dir(d) => {
                let absent: Vec<String> = items.iter()
                    .filter(|(rel, _, _)| std::fs::symlink_metadata(d.join(rel)).is_err())
                    .map(|(rel, _, _)| rel.clone()).collect();
                if !absent.is_empty() { return Err(AbsentChunks(absent).into()); }
                for (b, chunk) in items.chunks(256).enumerate() {
                    let got = chunk.par_iter().enumerate().map(|(k, (rel, len, _))| -> Result<(usize, Vec<u8>)> {
                        let v = read_local(d, rel).with_context(|| format!("read {rel}"))?;
                        ensure!(v.len() == *len, "{rel}: wrong length");
                        Ok((b * 256 + k, v))
                    }).collect::<Result<Vec<_>>>()?;
                    st.requests += got.len() as u64;
                    st.bytes += got.iter().map(|(_, v)| v.len() as u64).sum::<u64>();
                    f(got)?;
                }
                Ok(())
            }
            Source::Http { parallel, .. } => {
                let mut multi = Multi::new();
                multi.pipelining(false, true)?;
                let multiplexed = match options().version.as_deref() {
                    Some("h1") => false,
                    Some(_) => true,
                    None => matches!(self, Source::Http { base, h2c, .. } if *h2c || base.starts_with("https://")),
                };
                multi.set_max_host_connections(if multiplexed { 4 } else { *parallel })?;
                let mut next = 0usize;
                let mut active: HashMap<usize, (Easy2Handle<Collector>, usize, u32)> = HashMap::new();
                let mut retry: Vec<(usize, u32)> = Vec::new();
                let mut token = 0usize;
                let mut batch: Vec<(usize, Vec<u8>)> = Vec::new();
                let mut batch_bytes = 0usize;
                loop {
                    while active.len() < *parallel && (next < items.len() || !retry.is_empty()) {
                        let (i, attempt) = retry.pop().unwrap_or_else(|| { next += 1; (next - 1, 0) });
                        let (rel, len, data) = &items[i];
                        let mut e = self.easy(rel, *len)?;
                        if data < len && *data > 0 { e.range(&format!("0-{}", data - 1))?; }
                        let mut h = multi.add2(e)?;
                        token += 1;
                        h.set_token(token)?;
                        active.insert(token, (h, i, attempt));
                    }
                    if active.is_empty() { break; }
                    multi.perform()?;
                    let mut done = Vec::new();
                    multi.messages(|m| { if let Some(r) = m.result() { done.push((m.token().unwrap_or(0), r.is_ok())); } });
                    for (t, ok) in done {
                        let (h, i, attempt) = active.remove(&t).context("unknown transfer")?;
                        let mut e = multi.remove2(h)?;
                        st.requests += 1;
                        let code = e.response_code().unwrap_or(0);
                        let mut body = std::mem::take(&mut e.get_mut().buf);
                        let (_, len, data) = items[i];
                        let fits = (code == 200 && body.len() == len) || (code == 206 && data < len && body.len() == data);
                        if ok && fits {
                            st.bytes += body.len() as u64;
                            if code == 206 { st.ranged += 1; st.saved += (len - data) as u64; }
                            body.resize(len, 0);
                            batch_bytes += body.len();
                            batch.push((i, body));
                        } else if attempt + 1 < ATTEMPTS && code != 404 {
                            st.retries += 1;
                            retry.push((i, attempt + 1));
                        } else {
                            bail!("{}: HTTP {code}{}", items[i].0, if ok { format!(", {} bytes", body.len()) } else { ", transfer failed".into() });
                        }
                    }
                    if batch_bytes >= BATCH_BYTES {
                        f(std::mem::take(&mut batch))?;
                        batch_bytes = 0;
                    }
                    if !active.is_empty() { multi.wait(&mut [], Duration::from_millis(200))?; }
                }
                if !batch.is_empty() { f(batch)?; }
                Ok(())
            }
        }
    }
}
