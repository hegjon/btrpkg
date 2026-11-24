//! Cache housekeeping for btrpkg cache files: `cache-clean` keeps the newest N versions of each
//! package's `.pkg.btr` (and its `.sig`), like `paccache -rk N` does for `.pkg.tar.*`.
//! Versions are compared like pacman's vercmp (libalpm's alpm_pkg_vercmp / rpmvercmp).
use crate::store;
use anyhow::Result;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// rpmvercmp: compare two version strings (no epoch or release), a line-by-line port of
/// libalpm's version.c.
fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b { return Ordering::Equal; }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut one, mut two) = (0usize, 0usize); // start of the current segment
    let (mut p1, mut p2) = (0usize, 0usize);   // end of the previous segment
    let at = |s: &[u8], i: usize| -> u8 { if i < s.len() { s[i] } else { 0 } };
    while at(a, one) != 0 && at(b, two) != 0 {
        while at(a, one) != 0 && !at(a, one).is_ascii_alphanumeric() { one += 1; }
        while at(b, two) != 0 && !at(b, two).is_ascii_alphanumeric() { two += 1; }
        if at(a, one) == 0 || at(b, two) == 0 { break; }
        // different separator lengths: the one with fewer is older
        if one - p1 != two - p2 { return if one - p1 < two - p2 { Ordering::Less } else { Ordering::Greater }; }
        p1 = one; p2 = two;
        let isnum = at(a, p1).is_ascii_digit();
        if isnum {
            while at(a, p1).is_ascii_digit() { p1 += 1; }
            while at(b, p2).is_ascii_digit() { p2 += 1; }
        } else {
            while at(a, p1).is_ascii_alphabetic() { p1 += 1; }
            while at(b, p2).is_ascii_alphabetic() { p2 += 1; }
        }
        if one == p1 { return Ordering::Less; } // arbitrary, as in libalpm
        if two == p2 { return if isnum { Ordering::Greater } else { Ordering::Less }; }
        let (mut s1, mut s2) = (&a[one..p1], &b[two..p2]);
        if isnum {
            while s1.first() == Some(&b'0') { s1 = &s1[1..]; }
            while s2.first() == Some(&b'0') { s2 = &s2[1..]; }
            match s1.len().cmp(&s2.len()) { Ordering::Equal => {}, o => return o }
        }
        match s1.cmp(s2) { Ordering::Equal => {}, o => return o }
        one = p1; two = p2;
    }
    let (c1, c2) = (at(a, one), at(b, two));
    if c1 == 0 && c2 == 0 { return Ordering::Equal; }
    if (c1 == 0 && !c2.is_ascii_alphabetic()) || c1.is_ascii_alphabetic() { Ordering::Less } else { Ordering::Greater }
}

/// epoch:version-release
fn evr(s: &str) -> (&str, &str, Option<&str>) {
    let (epoch, rest) = match s.find(':') {
        Some(k) if s[..k].bytes().all(|c| c.is_ascii_digit()) => (if k == 0 { "0" } else { &s[..k] }, &s[k + 1..]),
        _ => ("0", s),
    };
    match rest.rfind('-') { Some(k) => (epoch, &rest[..k], Some(&rest[k + 1..])), None => (epoch, rest, None) }
}

/// pacman's vercmp.
pub(crate) fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b { return Ordering::Equal; }
    let (ea, va, ra) = evr(a);
    let (eb, vb, rb) = evr(b);
    rpmvercmp(ea, eb).then_with(|| rpmvercmp(va, vb)).then_with(|| match (ra, rb) {
        (Some(x), Some(y)) => rpmvercmp(x, y),
        _ => Ordering::Equal,
    })
}

#[derive(Default, Debug)]
pub(crate) struct CleanStats { pub kept: usize, pub removed: usize, pub bytes: u64 }

/// Keep the newest `keep` versions of every package's .pkg.btr in `dir`; remove the others and
/// their .sig. Space freed is the files' allocated size, which overstates it for cache files
/// that share extents with installed files.
pub(crate) fn cache_clean(dir: &Path, keep: usize, dry_run: bool) -> Result<CleanStats> {
    let mut by_pkg: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new(); // name -> (version, stem)
    for e in fs::read_dir(dir)?.flatten() {
        let n = e.file_name();
        let Some(stem) = n.to_str().and_then(store::cache_name) else { continue };
        if !e.file_type().is_ok_and(|t| t.is_file()) { continue; }
        let Some((name, ver)) = store::split_stem(stem) else { continue };
        by_pkg.entry(name.to_string()).or_default().push((ver.to_string(), stem.to_string()));
    }
    let mut st = CleanStats::default();
    for (_, mut v) in by_pkg {
        v.sort_by(|a, b| vercmp(&b.0, &a.0)); // newest first
        for (k, (_, stem)) in v.iter().enumerate() {
            if k < keep { st.kept += 1; continue; }
            for p in [dir.join(store::cache_rel(stem)), dir.join(format!("{}.sig", store::cache_rel(stem)))] {
                if fs::symlink_metadata(&p).is_ok() {
                    println!("{}{}", if dry_run { "would remove " } else { "removed " }, p.display());
                }
                crate::gc::remove(&p, dry_run, &mut st.bytes)?;
            }
            st.removed += 1;
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vercmp_like_pacman() {
        // pairs from pacman's test/util/vercmptest.sh: (a, b, expected sign of vercmp(a, b))
        let cases: &[(&str, &str, i32)] = &[
            ("1.5.0", "1.5.0", 0), ("1.5.1", "1.5.0", 1), ("1.5.1", "1.5", 1),
            ("1.5.0-1", "1.5.0-1", 0), ("1.5.0-1", "1.5.0-2", -1), ("1.5.0-1", "1.5.1-1", -1), ("1.5.0-2", "1.5.1-1", -1),
            ("1.5-1", "1.5", 0), ("1.1-1", "1.1", 0), ("1.0-1", "1.1", -1), ("1.1-1", "1.0", 1),
            ("1.5b-1", "1.5-1", -1), ("1.5b", "1.5", -1), ("1.5b-1", "1.5", -1), ("1.5b", "1.5.1", -1),
            ("1.0a", "1.0alpha", -1), ("1.0alpha", "1.0b", -1), ("1.0b", "1.0beta", -1), ("1.0beta", "1.0rc", -1), ("1.0rc", "1.0", -1),
            ("1.5.a", "1.5", 1), ("1.5.b", "1.5.a", 1), ("1.5.1", "1.5.b", 1),
            ("1.5.b-1", "1.5.b", 0), ("1.5-1", "1.5.b", -1),
            ("2.0", "2_0", 0), ("2.0_a", "2_0.a", 0), ("2.0a", "2.0.a", -1), ("2___a", "2_a", 1),
            ("0:1.0", "0:1.0", 0), ("0:1.0", "0:1.1", -1), ("1:1.0", "0:1.0", 1), ("1:1.0", "0:1.1", 1), ("1:1.0", "2:1.1", -1),
            ("0:1.0", "1.0", 0), ("0:1.0", "1.1", -1), ("0:1.1", "1.0", 1), ("1:1.0", "1.0", 1), ("1:1.0", "1.1", 1), ("1:1.1", "1.1", 1),
            ("1.0", "1.0-1", 0), ("4.0.0.r8025.gb972bd8-1", "4.0.0.r8103.ge4635a4-1", -1),
            ("2026.11.9-1", "2026.11.17-1", -1),
        ];
        for (a, b, want) in cases {
            let got = match vercmp(a, b) { Ordering::Less => -1, Ordering::Equal => 0, Ordering::Greater => 1 };
            assert_eq!(got, *want, "vercmp({a}, {b})");
            let back = match vercmp(b, a) { Ordering::Less => -1, Ordering::Equal => 0, Ordering::Greater => 1 };
            assert_eq!(back, -*want, "vercmp({b}, {a})");
        }
    }
}
