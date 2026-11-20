//! Filesystem operations for `install`, relative to a root directory fd.
//!
//! Every path is resolved with openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS): nothing is created,
//! changed or linked through a symlink, whether the package made it or it was on disk before, and
//! nothing outside the root is reachable. pacman refuses to extract through a symlink too
//! (libarchive's ARCHIVE_EXTRACT_SECURE_SYMLINKS).
//! Files are written under a temporary name in their directory and renamed into place; a failed
//! entry removes its temporary file.
use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

#[repr(C)]
struct OpenHow { flags: u64, mode: u64, resolve: u64 }
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

fn cstr(b: &[u8]) -> Result<CString> { Ok(CString::new(b).context("NUL in path")?) }

fn check(r: libc::c_int, what: &str, path: &[u8]) -> Result<()> {
    if r < 0 {
        let e = io::Error::last_os_error();
        let hint = if e.raw_os_error() == Some(libc::ELOOP) { " (a symlink in the path)" } else { "" };
        bail!("{what} {}: {e}{hint}", String::from_utf8_lossy(path));
    }
    Ok(())
}

/// openat2 beneath `dir`, never through a symlink.
pub fn open_beneath(dir: RawFd, path: &[u8], flags: i32) -> io::Result<OwnedFd> {
    let c = CString::new(path).map_err(io::Error::other)?;
    let how = OpenHow { flags: (flags | libc::O_CLOEXEC) as u64, mode: 0, resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS };
    loop {
        let r = unsafe { libc::syscall(libc::SYS_openat2, dir, c.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>()) };
        if r >= 0 { return Ok(unsafe { OwnedFd::from_raw_fd(r as RawFd) }); }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EAGAIN) { return Err(e); }
    }
}

pub struct Root { fd: OwnedFd, n: std::cell::Cell<u64> }

pub fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&c| c == b'/') { Some(i) => (&path[..i], &path[i + 1..]), None => (b"", path) }
}

impl Root {
    pub fn open(p: &Path) -> Result<Root> {
        std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
        let c = cstr(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()))?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()).with_context(|| format!("open {}", p.display())); }
        Ok(Root { fd: unsafe { OwnedFd::from_raw_fd(fd) }, n: std::cell::Cell::new(0) })
    }

    /// A directory below the root (empty = the root), made when missing and `create` is set.
    pub fn dir(&self, dir: &[u8], create: bool) -> Result<OwnedFd> {
        let flags = libc::O_PATH | libc::O_DIRECTORY;
        let dir = if dir.is_empty() { b"." as &[u8] } else { dir };
        match open_beneath(self.fd.as_raw_fd(), dir, flags) {
            Ok(fd) => return Ok(fd),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) && create => {}
            Err(e) => {
                let hint = if e.raw_os_error() == Some(libc::ELOOP) { " (a symlink in the path)" } else { "" };
                return Err(e).with_context(|| format!("directory {}{hint}", String::from_utf8_lossy(dir)));
            }
        }
        let mut cur = open_beneath(self.fd.as_raw_fd(), b".", flags)?;
        for comp in dir.split(|&c| c == b'/') {
            let c = cstr(comp)?;
            match open_beneath(cur.as_raw_fd(), comp, flags) {
                Ok(fd) => { cur = fd; continue; }
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => return Err(e).with_context(|| format!("directory {}", String::from_utf8_lossy(dir))),
            }
            let r = unsafe { libc::mkdirat(cur.as_raw_fd(), c.as_ptr(), 0o755) };
            if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                check(r, "mkdir", dir)?;
            }
            cur = open_beneath(cur.as_raw_fd(), comp, flags).with_context(|| format!("directory {}", String::from_utf8_lossy(dir)))?;
        }
        Ok(cur)
    }

    /// A fresh temporary name for an entry in a directory.
    fn tmp_name(&self) -> CString {
        self.n.set(self.n.get() + 1);
        CString::new(format!(".btrpkg-new-{}-{}", std::process::id(), self.n.get())).unwrap()
    }

    /// Create `path` through `make(parent fd, temporary name)`, then rename it into place. The
    /// temporary file is removed if anything fails.
    pub fn place(&self, path: &[u8], make: impl FnOnce(RawFd, &CString) -> Result<()>) -> Result<()> {
        let (parent, name) = split(path);
        let pfd = self.dir(parent, true)?;
        let tmp = self.tmp_name();
        let name = cstr(name)?;
        let r = make(pfd.as_raw_fd(), &tmp).and_then(|()| {
            check(unsafe { libc::renameat(pfd.as_raw_fd(), tmp.as_ptr(), pfd.as_raw_fd(), name.as_ptr()) }, "rename to", path)
        });
        if r.is_err() {
            unsafe {
                if libc::unlinkat(pfd.as_raw_fd(), tmp.as_ptr(), 0) != 0 {
                    libc::unlinkat(pfd.as_raw_fd(), tmp.as_ptr(), libc::AT_REMOVEDIR);
                }
            }
        }
        r
    }

    /// Make a directory (an existing one is fine, anything else there is an error).
    pub fn mkdir(&self, path: &[u8]) -> Result<()> {
        let (parent, name) = split(path);
        let pfd = self.dir(parent, true)?;
        let c = cstr(name)?;
        if unsafe { libc::mkdirat(pfd.as_raw_fd(), c.as_ptr(), 0o700) } == 0 { return Ok(()); }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) { return Err(e).with_context(|| format!("mkdir {}", String::from_utf8_lossy(path))); }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(pfd.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) }, "stat", path)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            bail!("{} exists and is not a directory", String::from_utf8_lossy(path));
        }
        Ok(())
    }

    /// Open an existing directory for setting its attributes.
    pub fn open_dir(&self, path: &[u8]) -> Result<OwnedFd> {
        open_beneath(self.fd.as_raw_fd(), path, libc::O_RDONLY | libc::O_DIRECTORY)
            .with_context(|| format!("open {}", String::from_utf8_lossy(path)))
    }

    /// Parent fd and name of an existing entry, for linking to it.
    pub fn existing(&self, path: &[u8]) -> Result<(OwnedFd, CString)> {
        let (parent, name) = split(path);
        Ok((self.dir(parent, false)?, cstr(name)?))
    }
}

/// Owner, mode, xattrs and times on an open file or directory (owner first: chown clears
/// setuid bits and capabilities).
pub fn set_attrs_fd(fd: RawFd, uid: u32, gid: u32, mode: u32, xattrs: &[(Vec<u8>, Vec<u8>)], mtime: i64, path: &[u8]) -> Result<()> {
    unsafe {
        // as tar does: owners are restored only when running as root
        if libc::geteuid() == 0 { check(libc::fchown(fd, uid, gid), "chown", path)?; }
        check(libc::fchmod(fd, mode & 0o7777), "chmod", path)?;
        for (k, v) in xattrs {
            let kc = cstr(k)?;
            check(libc::fsetxattr(fd, kc.as_ptr(), v.as_ptr() as *const _, v.len(), 0), "setxattr", path)?;
        }
        let ts = [libc::timespec { tv_sec: mtime, tv_nsec: 0 }, libc::timespec { tv_sec: mtime, tv_nsec: 0 }];
        check(libc::futimens(fd, ts.as_ptr()), "utimens", path)?;
    }
    Ok(())
}

/// The same for a symlink, by parent fd and name (no mode: symlinks have none).
pub fn set_attrs_link(pfd: RawFd, name: &CString, uid: u32, gid: u32, xattrs: &[(Vec<u8>, Vec<u8>)], mtime: i64, path: &[u8]) -> Result<()> {
    unsafe {
        if libc::geteuid() == 0 { check(libc::fchownat(pfd, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW), "chown", path)?; }
        if !xattrs.is_empty() {
            // no fd-based call for a symlink's xattrs: go through the parent's /proc fd link
            let p = cstr(format!("/proc/self/fd/{pfd}/{}", name.to_string_lossy()).as_bytes())?;
            for (k, v) in xattrs {
                let kc = cstr(k)?;
                check(libc::lsetxattr(p.as_ptr(), kc.as_ptr(), v.as_ptr() as *const _, v.len(), 0), "setxattr", path)?;
            }
        }
        let ts = [libc::timespec { tv_sec: mtime, tv_nsec: 0 }, libc::timespec { tv_sec: mtime, tv_nsec: 0 }];
        check(libc::utimensat(pfd, name.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW), "utimens", path)?;
    }
    Ok(())
}

pub fn check_rc(r: libc::c_int, what: &str, path: &[u8]) -> Result<()> { check(r, what, path) }
pub fn c(b: &[u8]) -> Result<CString> { cstr(b) }
