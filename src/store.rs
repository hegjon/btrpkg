//! Content-addressed chunk store: `btrpkg publish` and `btrpkg pull`.
//!
//! Store layout (static files, servable by any HTTP server; `pull` reads it as a directory):
//!   chunks/ab/<sha256>   chunk bytes exactly as written to disk (zstd frame or raw),
//!                        named by the sha256 of those bytes
//!   raw/ab/<sha256>      "comp encoded_len chunk-hash": maps the sha256 of a chunk's
//!                        uncompressed data to the chunk already stored for it, so a changed
//!                        compressor (different frame bytes) doesn't create new chunks
//!   index/NAME.btridx    header, manifest, chunk table with a chunk hash per entry, sha256
//!                        of all that (stand-in for a signature)
//! Publish writes chunks before the index, so a published index never refers to a missing chunk.
use super::*;

const IDX_MAGIC: &[u8; 8] = b"BTRIDX\0\0";
const IDX_ENT: usize = TABLE_ENT + 32;

#[derive(Clone, Copy)]
struct IChunk { c: Chunk, hash: [u8; 32] }

struct Index { meta: Vec<u8>, meta_raw_len: u64, image_len: u64, chunks: Vec<IChunk>, digest: [u8; 32] }

fn encode_index(meta: &[u8], meta_raw_len: u64, image_len: u64, chunks: &[IChunk]) -> Vec<u8> {
    let mut b = Vec::with_capacity(HDR + meta.len() + chunks.len() * IDX_ENT + 32);
    b.extend_from_slice(IDX_MAGIC);
    b.extend_from_slice(&VERSION.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&(meta.len() as u64).to_le_bytes());
    b.extend_from_slice(&(chunks.len() as u64).to_le_bytes());
    b.extend_from_slice(&image_len.to_le_bytes());
    b.extend_from_slice(&meta_raw_len.to_le_bytes());
    b.resize(HDR, 0);
    b.extend_from_slice(meta);
    for ic in chunks {
        b.extend_from_slice(&encode_table(&[ic.c]));
        b.extend_from_slice(&ic.hash);
    }
    let d = Sha256::digest(&b);
    b.extend_from_slice(&d);
    b
}

fn decode_index(b: &[u8]) -> Result<Index> {
    ensure!(b.len() >= HDR + 32 && &b[..8] == IDX_MAGIC, "not a btrpkg index");
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    ensure!(u32_at(8) == VERSION, "unsupported index version {}", u32_at(8));
    let (meta_len, nchunks, image_len, meta_raw_len) = (u64_at(16), u64_at(24), u64_at(32), u64_at(40));
    ensure!(meta_len < 1 << 32 && nchunks < 1 << 32 && meta_raw_len < 1 << 34, "index sizes out of range");
    let t0 = HDR + meta_len as usize;
    let tlen = nchunks as usize * IDX_ENT;
    ensure!(b.len() == t0 + tlen + 32, "index has the wrong size");
    let (body, digest) = b.split_at(b.len() - 32);
    ensure!(Sha256::digest(body).as_slice() == digest, "index digest mismatch");
    let ents: Vec<&[u8]> = b[t0..t0 + tlen].chunks_exact(IDX_ENT).collect();
    let table: Vec<u8> = ents.iter().flat_map(|e| e[..TABLE_ENT].iter().copied()).collect();
    let chunks = decode_table(&table, image_len)?
        .into_iter()
        .zip(&ents)
        .map(|(c, e)| IChunk { c, hash: e[TABLE_ENT..].try_into().unwrap() })
        .collect();
    Ok(Index { meta: b[HDR..t0].to_vec(), meta_raw_len, image_len, chunks, digest: digest.try_into().unwrap() })
}

fn hpath(store: &Path, kind: &str, h: &[u8]) -> PathBuf {
    let x = hex(h);
    store.join(kind).join(&x[..2]).join(x)
}

fn unhex(s: &str) -> Result<[u8; 32]> {
    ensure!(s.len() == 64, "bad hash");
    let mut o = [0u8; 32];
    for (i, b) in o.iter_mut().enumerate() { *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?; }
    Ok(o)
}

fn write_atomic(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() { fs::create_dir_all(d)?; }
    let tmp = { let mut t = p.as_os_str().to_owned(); t.push(format!(".tmp{}", std::process::id())); PathBuf::from(t) };
    fs::write(&tmp, data)?;
    fs::rename(&tmp, p)?;
    Ok(())
}

#[derive(Default, Debug)]
struct PublishStats { chunks: u64, new_chunks: u64, new_bytes: u64, reused_chunks: u64, reused_bytes: u64, remapped: u64, raw_entries: u64, index_bytes: u64 }

/// Publish a .pkg.btr into STORE. Prints what had to be uploaded.
pub(crate) fn publish(input: &Path, store: &Path) -> Result<()> {
    let b = fs::read(input).with_context(|| format!("open {}", input.display()))?;
    let hdr = Header::decode(&b)?;
    ensure!(hdr.flags == 0, "publish takes a .pkg.btr transport file (btrpkg convert)");
    let m_end = HDR + hdr.meta_len as usize;
    let t_end = m_end + hdr.table_len() as usize;
    ensure!(b.len() >= t_end + 32, "truncated package");
    let (body, digest) = b.split_at(b.len() - 32);
    ensure!(Sha256::digest(body).as_slice() == digest, "digest mismatch: package is corrupt");
    let chunks = decode_table(&b[m_end..t_end], hdr.image_len)?;
    let mut pos = t_end;
    let mut items = Vec::with_capacity(chunks.len());
    for c in chunks {
        let e = c.encoded_len as usize;
        ensure!(pos + e <= body.len(), "truncated package");
        items.push((c, &b[pos..pos + e]));
        pos += e;
    }
    ensure!(pos == body.len(), "trailing data in package");

    // Hash each chunk's uncompressed data and its stored bytes (in parallel).
    let hashed = items
        .par_iter()
        .map(|&(c, d)| -> Result<(Chunk, &[u8], [u8; 32], [u8; 32])> {
            let raw: [u8; 32] = if c.compression == COMP_ZSTD {
                let plain = zstd::bulk::decompress(d, c.unencoded_len as usize)?;
                ensure!(plain.len() == c.unencoded_len as usize, "chunk decompressed to the wrong size");
                Sha256::digest(&plain).into()
            } else {
                Sha256::digest(d).into()
            };
            Ok((c, d, raw, Sha256::digest(d).into()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut st = PublishStats::default();
    let mut out = Vec::with_capacity(hashed.len());
    for (c, d, raw, h) in hashed {
        st.chunks += 1;
        let rp = hpath(store, "raw", &raw);
        let ic = match fs::read_to_string(&rp) {
            Ok(s) => {
                // Same uncompressed data published before: reuse that chunk, even if this
                // build compressed it to different bytes.
                let f: Vec<&str> = s.split_whitespace().collect();
                ensure!(f.len() == 3, "bad raw map entry {}", rp.display());
                let hash = unhex(f[2])?;
                ensure!(hpath(store, "chunks", &hash).exists(), "raw map points at a missing chunk");
                let c2 = Chunk { compression: f[0].parse()?, encoded_len: f[1].parse()?, ..c };
                if hash != h { st.remapped += 1; }
                st.reused_chunks += 1;
                st.reused_bytes += c2.encoded_len as u64;
                IChunk { c: c2, hash }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cp = hpath(store, "chunks", &h);
                if cp.exists() {
                    st.reused_chunks += 1;
                    st.reused_bytes += d.len() as u64;
                } else {
                    write_atomic(&cp, d)?;
                    st.new_chunks += 1;
                    st.new_bytes += d.len() as u64;
                }
                write_atomic(&rp, format!("{} {} {}\n", c.compression, c.encoded_len, hex(&h)).as_bytes())?;
                st.raw_entries += 1;
                IChunk { c, hash: h }
            }
            Err(e) => return Err(e.into()),
        };
        out.push(ic);
    }
    let idx = encode_index(&b[HDR..m_end], hdr.meta_raw_len, hdr.image_len, &out);
    st.index_bytes = idx.len() as u64;
    let name = input.file_name().context("no file name")?.to_string_lossy();
    let name = name.trim_end_matches(".btrpkg").trim_end_matches(".pkg.btr");
    write_atomic(&store.join("index").join(format!("{name}.btridx")), &idx)?;
    eprintln!("publish: {st:?}");
    println!("upload\t{name}\t{}\t{}\t{}\t{}", st.new_chunks, st.new_bytes, st.index_bytes, st.raw_entries);
    Ok(())
}

#[derive(Default, Debug)]
#[allow(dead_code)]
struct PullStats { chunks: u64, fetched_chunks: u64, fetched_bytes: u64, reused_chunks: u64, reused_bytes: u64, index_bytes: u64, encoded_chunks: u64, raw_chunks: u64, fallback_chunks: u64 }

/// Pull NAME from STORE into CACHE. Chunks that an earlier pulled package in the same
/// directory already has (found through its CACHE.btridx) are reflinked, the rest are
/// read from the store, checked against their hash and written as encoded extents.
pub(crate) fn pull(store: &Path, name: &str, cache: &Path, use_encoded: bool, index_sha256: Option<&str>) -> Result<()> {
    let ib = fs::read(store.join("index").join(format!("{name}.btridx"))).with_context(|| format!("index for {name}"))?;
    if let Some(want) = index_sha256 {
        ensure!(hex(&Sha256::digest(&ib)).eq_ignore_ascii_case(want), "index sha256 mismatch");
    }
    let idx = decode_index(&ib)?;
    let mut st = PullStats { index_bytes: ib.len() as u64, chunks: idx.chunks.len() as u64, ..Default::default() };

    // What the cache already has: (chunk hash, compression, length) -> (cache file, offset).
    let dir = cache.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut srcs: Vec<File> = Vec::new();
    let mut local: HashMap<([u8; 32], u8, u32), (usize, u64)> = HashMap::new();
    for ent in fs::read_dir(dir)? {
        let p = ent?.path();
        let Some(cf) = p.to_str().and_then(|s| s.strip_suffix(".btridx")).map(PathBuf::from) else { continue };
        if cf == cache { continue; }
        let (Ok(ob), Ok(f)) = (fs::read(&p), File::open(&cf)) else { continue };
        let Ok(old) = decode_index(&ob) else { continue };
        let mut hb = [0u8; HDR];
        if f.read_exact_at(&mut hb, 0).is_err() { continue; }
        let Ok(h) = Header::decode(&hb) else { continue };
        if h.flags & FLAG_MATERIALIZED == 0 || h.image_len != old.image_len { continue; }
        srcs.push(f);
        for ic in &old.chunks {
            local.entry((ic.hash, ic.c.compression, ic.c.unencoded_len)).or_insert((srcs.len() - 1, h.image_base + ic.c.image_off));
        }
    }

    let tmp = cache.with_extension("part");
    let _ = fs::remove_file(&tmp);
    let out = OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp)?;
    if use_encoded { set_nocomp(&out); }
    let table = encode_table(&idx.chunks.iter().map(|ic| ic.c).collect::<Vec<_>>());
    let image_base = align((HDR + idx.meta.len() + table.len() + 32) as u64);

    let mut run = || -> Result<()> {
        let mut can_clone = true;
        let mut encoded_ok = use_encoded;
        let mut dctx = zstd::bulk::Decompressor::new()?;
        let mut missing = Vec::new();
        for ic in &idx.chunks {
            let off = image_base + ic.c.image_off;
            if can_clone {
                if let Some(&(s, soff)) = local.get(&(ic.hash, ic.c.compression, ic.c.unencoded_len)) {
                    match clone_range_at(&srcs[s], soff, ic.c.unencoded_len as u64, &out, off) {
                        Ok(()) => { st.reused_chunks += 1; st.reused_bytes += ic.c.encoded_len as u64; continue; }
                        Err(e) => { eprintln!("btrpkg: reflink unavailable ({e}); downloading instead"); can_clone = false; }
                    }
                }
            }
            missing.push(*ic);
        }
        // "Download": read missing chunks from the store and check them, in parallel batches.
        for batch in missing.chunks(256) {
            let got = batch
                .par_iter()
                .map(|ic| -> Result<(IChunk, Vec<u8>)> {
                    let d = fs::read(hpath(store, "chunks", &ic.hash)).with_context(|| format!("chunk {}", hex(&ic.hash)))?;
                    ensure!(d.len() == ic.c.encoded_len as usize && Sha256::digest(&d).as_slice() == ic.hash,
                        "chunk {} is corrupt", hex(&ic.hash));
                    Ok((*ic, d))
                })
                .collect::<Result<Vec<_>>>()?;
            for (ic, d) in got {
                st.fetched_chunks += 1;
                st.fetched_bytes += d.len() as u64;
                let off = image_base + ic.c.image_off;
                if ic.c.compression == COMP_NONE {
                    out.write_all_at(&d, off)?;
                    st.raw_chunks += 1;
                    continue;
                }
                if encoded_ok {
                    match encoded_write(&out, off, ic.c.unencoded_len as u64, &d) {
                        Ok(()) => { st.encoded_chunks += 1; continue; }
                        Err(e) if st.encoded_chunks == 0 => {
                            eprintln!("btrpkg: encoded write unavailable ({e}); decompressing instead");
                            encoded_ok = false;
                        }
                        Err(e) => return Err(e).context("encoded write"),
                    }
                }
                let plain = dctx.decompress(&d, ic.c.unencoded_len as usize)?;
                ensure!(plain.len() == ic.c.unencoded_len as usize, "chunk decompressed to the wrong size");
                out.write_all_at(&plain, off)?;
                st.fallback_chunks += 1;
            }
        }
        // Same cache-file layout as `fetch`, so `install` works unchanged.
        let hdr = Header {
            flags: FLAG_MATERIALIZED,
            meta_len: idx.meta.len() as u64,
            nchunks: idx.chunks.len() as u64,
            image_len: idx.image_len,
            image_base,
            meta_raw_len: idx.meta_raw_len,
        };
        let mut pre = hdr.encode().to_vec();
        pre.extend_from_slice(&idx.meta);
        pre.extend_from_slice(&table);
        pre.extend_from_slice(&idx.digest);
        out.write_all_at(&pre, 0)?;
        out.set_len(image_base + idx.image_len)?;
        Ok(())
    };
    if let Err(e) = run() {
        drop(out);
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    drop(out);
    fs::rename(&tmp, cache)?;
    let side = { let mut t = cache.as_os_str().to_owned(); t.push(".btridx"); PathBuf::from(t) };
    write_atomic(&side, &ib)?;
    eprintln!("pull: {st:?}");
    println!("download\t{name}\t{}\t{}\t{}\t{}\t{}", st.index_bytes, st.fetched_chunks, st.fetched_bytes, st.reused_chunks, st.reused_bytes);
    Ok(())
}
