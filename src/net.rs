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

#[derive(Default, Debug, Clone)]
pub struct NetStats { pub requests: u64, pub bytes: u64, pub retries: u64 }

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

impl Source {
    pub fn parse(s: &str, h2c: bool, parallel: Option<usize>) -> Source {
        if s.starts_with("http://") || s.starts_with("https://") {
            // HTTP/2 multiplexes; HTTP/1.1 opens a connection per request in flight
            let h2 = h2c || s.starts_with("https://");
            Source::Http { base: s.trim_end_matches('/').to_string(), h2c, parallel: parallel.unwrap_or(if h2 { 64 } else { 8 }) }
        } else {
            Source::Dir(PathBuf::from(s))
        }
    }

    fn easy(&self, rel: &str, max: usize) -> Result<Easy2<Collector>> {
        let Source::Http { base, h2c, .. } = self else { unreachable!() };
        let mut e = Easy2::new(Collector { buf: Vec::new(), max });
        e.url(&format!("{base}/{rel}"))?;
        e.http_version(if *h2c { HttpVersion::V2PriorKnowledge } else if base.starts_with("https://") { HttpVersion::V2TLS } else { HttpVersion::V11 })?;
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
            Source::Dir(d) => match std::fs::read(d.join(rel)) {
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

    /// Many objects of known exact length. `f` gets them in batches of about 16 MB, in no
    /// particular order, as (item index, bytes).
    pub fn get_many(&self, items: &[(String, usize)], st: &mut NetStats, mut f: impl FnMut(Vec<(usize, Vec<u8>)>) -> Result<()>) -> Result<()> {
        match self {
            Source::Dir(d) => {
                for (b, chunk) in items.chunks(256).enumerate() {
                    let got = chunk.par_iter().enumerate().map(|(k, (rel, len))| -> Result<(usize, Vec<u8>)> {
                        let v = std::fs::read(d.join(rel)).with_context(|| format!("read {rel}"))?;
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
                multi.set_max_host_connections(if matches!(self, Source::Http { base, h2c, .. } if *h2c || base.starts_with("https://")) { 4 } else { *parallel })?;
                let mut next = 0usize;
                let mut active: HashMap<usize, (Easy2Handle<Collector>, usize, u32)> = HashMap::new();
                let mut retry: Vec<(usize, u32)> = Vec::new();
                let mut token = 0usize;
                let mut batch: Vec<(usize, Vec<u8>)> = Vec::new();
                let mut batch_bytes = 0usize;
                loop {
                    while active.len() < *parallel && (next < items.len() || !retry.is_empty()) {
                        let (i, attempt) = retry.pop().unwrap_or_else(|| { next += 1; (next - 1, 0) });
                        let mut h = multi.add2(self.easy(&items[i].0, items[i].1)?)?;
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
                        let body = std::mem::take(&mut e.get_mut().buf);
                        if ok && code == 200 && body.len() == items[i].1 {
                            st.bytes += body.len() as u64;
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
