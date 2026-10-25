//! The scan's output: one flat record per directory, linked by index.

use std::fmt;

use crate::sys::{self, Key, Stat};

#[derive(Debug)]
pub enum ErrorKind {
    Access,
    ReadDir,
}

#[derive(Debug)]
pub struct ScanError {
    pub kind: ErrorKind,
    pub path: Vec<u8>,
    pub errno: i32,
}

impl ScanError {
    pub fn new(kind: ErrorKind, path: Vec<u8>, err: std::io::Error) -> ScanError {
        let errno = err.raw_os_error().unwrap_or(libc::EIO);
        ScanError { kind, path, errno }
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Access => "cannot access",
            ErrorKind::ReadDir => "cannot read directory",
        };
        let path = String::from_utf8_lossy(&self.path);
        write!(f, "{what} '{path}': {}", sys::strerror(self.errno))
    }
}

/// Everything in a directory whose effect on the output depends on
/// traversal order, kept in readdir order. Plain files carry no order
/// dependence, so they are summed straight into `Dir::bytes` instead.
#[derive(Debug)]
pub enum Item {
    Dir(usize),
    /// A file that may be reachable more than once (hard links, or a file
    /// also named as an argument); counted only on its first visit.
    Link { key: Key, bytes: u64 },
    Error(ScanError),
}

#[derive(Debug)]
pub struct Dir {
    /// Empty for directories below --max-depth, which are never printed.
    pub path: Vec<u8>,
    pub depth: usize,
    pub key: Key,
    pub bytes: u64,
    pub items: Vec<Item>,
}

impl Dir {
    pub fn new(depth: usize, stat: &Stat) -> Dir {
        Dir { path: Vec::new(), depth, key: stat.key, bytes: stat.bytes, items: Vec::new() }
    }
}

/// A command-line operand, resolved before the scan.
pub enum Root {
    Missing(ScanError),
    File { path: Vec<u8>, stat: Stat },
    Dir(usize),
}

/// Trims repeated trailing slashes on an operand down to one, as gnulib's
/// fts_open does: "a//" becomes "a/", "///" becomes "/", but "//" stays.
pub fn trim_operand(mut path: Vec<u8>) -> Vec<u8> {
    if path.len() > 2 && path.ends_with(b"/") {
        while path.len() > 1 && path[path.len() - 2] == b'/' {
            path.pop();
        }
    }
    path
}

/// Joins like fts: a single trailing slash on the parent is dropped, so
/// "a/" yields "a/b" and "/" yields "/b".
pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let parent = parent.strip_suffix(b"/").unwrap_or(parent);
    let mut path = Vec::with_capacity(parent.len() + 1 + name.len());
    path.extend_from_slice(parent);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_paths() {
        type Case<'a> = (&'a str, &'a [u8], &'a [u8], &'a [u8]);
        let cases: &[Case] = &[
            ("plain parent", b"a", b"b", b"a/b"),
            ("trailing slash dropped", b"a/", b"b", b"a/b"),
            ("only one slash dropped", b"a//", b"b", b"a//b"),
            ("root", b"/", b"usr", b"/usr"),
            ("dot", b".", b"x", b"./x"),
        ];
        for (name, parent, child, want) in cases {
            assert_eq!(join(parent, child), *want, "{name}");
        }
    }

    #[test]
    fn trim_operands() {
        let cases: &[(&str, &[u8], &[u8])] = &[
            ("no slash", b"a", b"a"),
            ("one slash kept", b"a/", b"a/"),
            ("repeated trimmed to one", b"a///", b"a/"),
            ("root", b"/", b"/"),
            ("double slash root kept", b"//", b"//"),
            ("triple slash root", b"///", b"/"),
            ("inner slashes untouched", b"a//b", b"a//b"),
        ];
        for (name, path, want) in cases {
            assert_eq!(trim_operand(path.to_vec()), *want, "{name}");
        }
    }
}
