//! Thin wrappers over the Linux syscalls the scanner needs. All `unsafe`
//! lives here.

use std::ffi::{CStr, c_char};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// (st_dev, st_ino): identifies a file for hard-link de-duplication.
pub type Key = (u64, u64);

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub key: Key,
    pub nlink: u64,
    /// Disk usage in bytes (st_blocks is always in 512-byte units).
    pub bytes: u64,
    pub is_dir: bool,
}

impl Stat {
    fn new(st: &libc::stat) -> Stat {
        Stat {
            key: (st.st_dev, st.st_ino),
            nlink: st.st_nlink,
            bytes: st.st_blocks as u64 * 512,
            is_dir: st.st_mode & libc::S_IFMT == libc::S_IFDIR,
        }
    }
}

pub fn lstat(path: &CStr) -> io::Result<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::lstat(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::new(unsafe { st.assume_init_ref() }))
}

pub fn stat_at(dir: BorrowedFd, name: &CStr) -> io::Result<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    let flags = libc::AT_SYMLINK_NOFOLLOW;
    if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat::new(unsafe { st.assume_init_ref() }))
}

/// O_NOFOLLOW guards against a directory being swapped for a symlink between
/// the parent's fstatat and this open.
pub fn open_dir(path: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Mirrors gnulib fts's dirent_inode_sort_may_be_useful: GNU du sorts large
/// directories by inode except on these file systems, and we must do the
/// same to reproduce its output order.
pub fn inode_sort_useful(dir: BorrowedFd) -> bool {
    const CIFS: i64 = 0xFF53_4D42;
    const NFS: i64 = 0x6969;
    const TMPFS: i64 = 0x0102_1994;

    let mut fs = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(dir.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return true;
    }
    let kind = unsafe { fs.assume_init_ref() }.f_type;
    !matches!(kind, CIFS | NFS | TMPFS)
}

/// strerror without Rust's " (os error N)" suffix, to match coreutils.
/// Not thread-safe in theory; only called from the reporting thread.
pub fn strerror(errno: i32) -> String {
    let msg = unsafe { CStr::from_ptr(libc::strerror(errno) as *const c_char) };
    msg.to_string_lossy().into_owned()
}

pub struct RawEntry<'a> {
    pub ino: u64,
    pub name: &'a CStr,
}

/// Reads directory entries with getdents64 into a caller-owned buffer, so
/// workers reuse one buffer for every directory instead of allocating a DIR.
pub struct DirStream<'a> {
    fd: BorrowedFd<'a>,
    buf: &'a mut [u8],
    len: usize,
    off: usize,
}

impl<'a> DirStream<'a> {
    pub fn new(fd: BorrowedFd<'a>, buf: &'a mut [u8]) -> DirStream<'a> {
        DirStream { fd, buf, len: 0, off: 0 }
    }

    /// Returns the next entry other than "." and "..".
    pub fn next(&mut self) -> io::Result<Option<RawEntry<'_>>> {
        // linux_dirent64: d_ino u64, d_off i64, d_reclen u16, d_type u8, d_name.
        let start = loop {
            if self.off >= self.len && !self.fill()? {
                return Ok(None);
            }
            let start = self.off;
            let rec = &self.buf[start..self.len];
            self.off += u16::from_ne_bytes([rec[16], rec[17]]) as usize;
            if !matches!(&rec[19..22], [b'.', 0, _] | [b'.', b'.', 0]) {
                break start;
            }
        };
        let rec = &self.buf[start..self.off];
        let ino = u64::from_ne_bytes(rec[0..8].try_into().unwrap());
        let name = CStr::from_bytes_until_nul(&rec[19..])
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        Ok(Some(RawEntry { ino, name }))
    }

    fn fill(&mut self) -> io::Result<bool> {
        let (fd, ptr, cap) = (self.fd.as_raw_fd(), self.buf.as_mut_ptr(), self.buf.len());
        let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, ptr, cap) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        self.len = n as usize;
        self.off = 0;
        Ok(n > 0)
    }
}
