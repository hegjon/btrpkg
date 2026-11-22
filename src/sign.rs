//! Ed25519 signatures over the index header, in OpenBSD signify's file formats.
//!
//! The signed message is the 256-byte header. It starts with the magic `BTRPKG\0\0` and the
//! format version, which keeps a btrpkg signature from being valid for anything else, and its
//! last 32 bytes hash the rest, which holds the hash of every section; the chunk table holds the
//! hash of every chunk. So one signature covers the whole package.
//!
//! Files (base64 lines after an "untrusted comment:" line, as signify writes them):
//!   NAME.pub   "Ed" | key number (8) | public key (32)
//!   NAME.sec   "Ed" "BK" | KDF rounds u32 BE (0 = not encrypted) | salt (16) | checksum (8) |
//!              key number (8) | secret key (64: seed, public key)
//!   X.sig      "Ed" | key number (8) | signature (64)
//! So `signify -V -p NAME.pub -x X.sig -m HEADER` verifies a btrpkg signature, where HEADER is
//! the first 256 bytes of the index.
//!
//! A keyring is every `*.pub` in the key directories, minus the key numbers listed (hex, one per
//! line, `#` comments) in a `revoked` file in any of them.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sha2::Digest;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_KEY_DIRS: [&str; 2] = ["/etc/btrpkg/keys", "/usr/share/btrpkg/keys"];
pub type KeyNum = [u8; 8];

fn b64() -> base64::engine::GeneralPurpose { base64::engine::general_purpose::STANDARD }

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    let n = unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, N, 0) };
    ensure!(n == N as isize, "getrandom failed");
    Ok(b)
}

/// The decoded payload of a signify file (the line after the comment).
fn read_blob(text: &str, what: &str) -> Result<(String, Vec<u8>)> {
    let mut lines = text.lines();
    let comment = lines.next().and_then(|l| l.strip_prefix("untrusted comment: ")).with_context(|| format!("{what}: no comment line"))?;
    let data = b64().decode(lines.next().with_context(|| format!("{what}: no key line"))?.trim()).with_context(|| format!("{what}: bad base64"))?;
    ensure!(data.len() >= 2 && &data[..2] == b"Ed", "{what}: not an Ed25519 signify file");
    Ok((comment.to_string(), data))
}

fn write_blob(comment: &str, data: &[u8]) -> String {
    format!("untrusted comment: {comment}\n{}\n", b64().encode(data))
}

pub struct SecretKey { pub keynum: KeyNum, key: SigningKey }

impl SecretKey {
    pub fn load(p: &Path) -> Result<SecretKey> {
        let what = p.display().to_string();
        let (_, d) = read_blob(&fs::read_to_string(p).with_context(|| format!("read {what}"))?, &what)?;
        ensure!(d.len() == 2 + 2 + 4 + 16 + 8 + 8 + 64 && &d[2..4] == b"BK", "{what}: not a signify secret key");
        ensure!(d[4..8] == [0; 4], "{what}: encrypted keys are not supported (make one with btrpkg keygen)");
        // "Ed" "BK" rounds(4) salt(16) checksum(8) keynum(8) seckey(64)
        let sk: [u8; 64] = d[40..104].try_into().unwrap();
        ensure!(sha2::Sha512::digest(sk)[..8] == d[24..32], "{what}: checksum mismatch");
        let key = SigningKey::from_bytes(&sk[..32].try_into().unwrap());
        ensure!(key.verifying_key().as_bytes()[..] == sk[32..], "{what}: public half doesn't match");
        Ok(SecretKey { keynum: d[32..40].try_into().unwrap(), key })
    }

    /// A signify signature file for `msg`.
    pub fn sign(&self, msg: &[u8], keyname: &str) -> String {
        let sig = self.key.sign(msg);
        let mut d = b"Ed".to_vec();
        d.extend_from_slice(&self.keynum);
        d.extend_from_slice(&sig.to_bytes());
        write_blob(&format!("verify with {keyname}.pub"), &d)
    }
}

/// Write NAME.sec and NAME.pub (refuses to overwrite). Returns the key number.
pub fn keygen(prefix: &Path) -> Result<KeyNum> {
    let name = prefix.file_name().context("key name")?.to_string_lossy().to_string();
    let seed: [u8; 32] = random()?;
    let keynum: KeyNum = random()?;
    let key = SigningKey::from_bytes(&seed);
    let mut sk = [0u8; 64];
    sk[..32].copy_from_slice(&seed);
    sk[32..].copy_from_slice(key.verifying_key().as_bytes());
    let mut sec = b"EdBK".to_vec();
    sec.extend_from_slice(&0u32.to_be_bytes());
    sec.extend_from_slice(&random::<16>()?);
    sec.extend_from_slice(&sha2::Sha512::digest(sk)[..8]);
    sec.extend_from_slice(&keynum);
    sec.extend_from_slice(&sk);
    let mut pubk = b"Ed".to_vec();
    pubk.extend_from_slice(&keynum);
    pubk.extend_from_slice(key.verifying_key().as_bytes());
    let with = |ext: &str| { let mut s = prefix.as_os_str().to_owned(); s.push(ext); PathBuf::from(s) };
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    for (path, mode, text) in [(with(".sec"), 0o600, write_blob(&format!("btrpkg secret key {name}"), &sec)),
                               (with(".pub"), 0o644, write_blob(&format!("btrpkg public key {name}"), &pubk))] {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    Ok(keynum)
}

#[derive(Clone)]
pub struct Keyring { keys: HashMap<KeyNum, (VerifyingKey, PathBuf)>, revoked: Vec<KeyNum> }

impl Keyring {
    /// Every `*.pub` in `dirs` (missing directories are skipped) minus revoked key numbers.
    pub fn load(dirs: &[PathBuf]) -> Result<Keyring> {
        let mut keys = HashMap::new();
        let mut revoked = Vec::new();
        for d in dirs {
            let Ok(rd) = fs::read_dir(d) else { continue };
            let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for p in paths {
                if p.file_name().is_some_and(|n| n == "revoked") {
                    for l in fs::read_to_string(&p)?.lines() {
                        let l = l.split('#').next().unwrap().trim();
                        if l.is_empty() { continue; }
                        revoked.push(unhex8(l).with_context(|| format!("{}: bad key number {l:?}", p.display()))?);
                    }
                } else if p.extension().is_some_and(|e| e == "pub") {
                    let what = p.display().to_string();
                    let (_, d) = read_blob(&fs::read_to_string(&p)?, &what)?;
                    ensure!(d.len() == 42, "{what}: not a signify public key");
                    let vk = VerifyingKey::from_bytes(&d[10..42].try_into().unwrap()).with_context(|| format!("{what}: bad key"))?;
                    keys.entry(d[2..10].try_into().unwrap()).or_insert((vk, p.clone()));
                }
            }
        }
        Ok(Keyring { keys, revoked })
    }

    pub fn is_empty(&self) -> bool { self.keys.is_empty() }

    /// Check a signify signature over `msg`. Returns the path of the key that made it.
    pub fn verify(&self, msg: &[u8], sig_text: &str) -> Result<&Path> {
        let (_, d) = read_blob(sig_text, "signature")?;
        ensure!(d.len() == 74, "not a signify signature");
        let keynum: KeyNum = d[2..10].try_into().unwrap();
        if self.revoked.contains(&keynum) { bail!("signed with revoked key {}", hex8(&keynum)); }
        let Some((vk, path)) = self.keys.get(&keynum) else { bail!("signed with unknown key {}", hex8(&keynum)) };
        let sig = ed25519_dalek::Signature::from_bytes(&d[10..74].try_into().unwrap());
        vk.verify_strict(msg, &sig).map_err(|_| anyhow::anyhow!("bad signature (key {})", hex8(&keynum)))?;
        Ok(path)
    }
}

pub fn hex8(k: &KeyNum) -> String { k.iter().map(|b| format!("{b:02x}")).collect() }

fn unhex8(s: &str) -> Result<KeyNum> {
    ensure!(s.len() == 16 && s.bytes().all(|c| c.is_ascii_hexdigit()), "want 16 hex digits");
    let mut k = [0u8; 8];
    for (i, b) in k.iter_mut().enumerate() { *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?; }
    Ok(k)
}

/// What an index must pass before anything of it is written.
pub enum Trust {
    /// Benchmarks and tests only.
    Unverified,
    /// The header hash, from a source already trusted (a signed repo database).
    Id(crate::format::Hash),
    /// A signature by a key in the keyring.
    Signed(Keyring, String),
    /// Every one of these.
    All(Vec<Trust>),
}

impl Trust {
    pub fn check(&self, header: &[u8]) -> Result<()> {
        match self {
            Trust::Unverified => Ok(()),
            Trust::Id(want) => {
                ensure!(header.len() == crate::format::HDR && header[224..] == want[..], "index ID mismatch: not the package the repository lists");
                Ok(())
            }
            Trust::Signed(k, sig) => k.verify(header, sig).map(|_| ()).context("index signature"),
            Trust::All(v) => v.iter().try_for_each(|t| t.check(header)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_revoke() {
        let t = crate::tests::TempDir::new("sign");
        let a = keygen(&t.0.join("a")).unwrap();
        keygen(&t.0.join("b")).unwrap();
        assert!(keygen(&t.0.join("a")).is_err(), "overwrote a key");
        let sk = SecretKey::load(&t.0.join("a.sec")).unwrap();
        assert_eq!(sk.keynum, a);
        let msg = [7u8; 256];
        let sig = sk.sign(&msg, "a");

        let kdir = t.0.join("keys");
        fs::create_dir_all(&kdir).unwrap();
        // unknown key: empty keyring
        assert!(Keyring::load(&[kdir.clone()]).unwrap().verify(&msg, &sig).is_err());
        fs::copy(t.0.join("a.pub"), kdir.join("a.pub")).unwrap();
        let k = Keyring::load(&[kdir.clone(), t.0.join("missing")]).unwrap();
        assert!(k.verify(&msg, &sig).is_ok());
        // tampered message, tampered signature
        let mut m2 = msg;
        m2[100] ^= 1;
        assert!(k.verify(&m2, &sig).is_err());
        let (c, mut d) = read_blob(&sig, "").unwrap();
        d[40] ^= 1;
        assert!(k.verify(&msg, &write_blob(&c, &d)).is_err());
        // signed by b, keyring only has a
        let sb = SecretKey::load(&t.0.join("b.sec")).unwrap().sign(&msg, "b");
        assert!(k.verify(&msg, &sb).unwrap_err().to_string().contains("unknown key"));
        // revoked
        fs::write(kdir.join("revoked"), format!("# compromised\n{}\n", hex8(&a))).unwrap();
        let k = Keyring::load(&[kdir.clone()]).unwrap();
        assert!(k.verify(&msg, &sig).unwrap_err().to_string().contains("revoked"));
    }
}
