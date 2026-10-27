//! The scan's output: one flat record per directory, linked by index.

use std::fmt;
use std::ops::Index;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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

/// Directories per page; a page is about 500 KB.
const PAGE: usize = 4096;
/// Caps a tree at 2^28 directories, far more than fit in memory anyway.
const PAGES: usize = 1 << 16;

/// One directory's place in the tree: bookkeeping updated while it and
/// its descendants are scanned, plus the finished record.
struct Slot {
    parent: Option<usize>,
    /// Children whose subtrees are incomplete, plus one until the
    /// directory itself has been scanned.
    pending: AtomicUsize,
    /// Bytes in the subtree, excluding `Item::Link`s.
    total: AtomicU64,
    /// Whether the subtree holds links or errors, whose effect depends on
    /// the order they are visited in.
    ordered: AtomicBool,
    dir: OnceLock<Dir>,
}

impl Slot {
    fn new(parent: Option<usize>) -> Slot {
        let (pending, total, ordered) = (AtomicUsize::new(1), AtomicU64::new(0), AtomicBool::new(false));
        Slot { parent, pending, total, ordered, dir: OnceLock::new() }
    }
}

/// Directory records by id, each written once by whichever worker finishes
/// that directory. Pages are allocated on demand, so writers never block on
/// one growing vector and nothing has to be gathered after the scan.
pub struct Tree {
    pages: Vec<OnceLock<Box<[OnceLock<Slot>]>>>,
}

impl Tree {
    pub fn new() -> Tree {
        Tree { pages: (0..PAGES).map(|_| OnceLock::new()).collect() }
    }

    /// Registers a directory as soon as it is discovered, before any
    /// worker can finish it.
    pub fn create(&self, id: usize, parent: Option<usize>) {
        let page = self.pages[id / PAGE].get_or_init(|| (0..PAGE).map(|_| OnceLock::new()).collect());
        assert!(page[id % PAGE].set(Slot::new(parent)).is_ok(), "directory {id} created twice");
        if let Some(parent) = parent {
            self.slot(parent).pending.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Stores a scanned directory and, once its subtree is complete, folds
    /// its total into its ancestors. Totals are thus ready when the scan
    /// ends, and the report only walks subtrees whose result depends on
    /// traversal order.
    pub fn finish(&self, id: usize, dir: Dir) {
        let slot = self.slot(id);
        slot.total.fetch_add(dir.bytes, Ordering::Relaxed);
        if dir.items.iter().any(|i| !matches!(i, Item::Dir(_))) {
            slot.ordered.store(true, Ordering::Relaxed);
        }
        assert!(slot.dir.set(dir).is_ok(), "directory {id} scanned twice");

        let mut id = id;
        loop {
            let slot = self.slot(id);
            // AcqRel pairs with the same decrement in other workers, so the
            // thread completing a subtree sees every child's contribution.
            if slot.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            let Some(parent) = slot.parent else { return };
            let up = self.slot(parent);
            up.total.fetch_add(slot.total.load(Ordering::Relaxed), Ordering::Relaxed);
            if slot.ordered.load(Ordering::Relaxed) {
                up.ordered.store(true, Ordering::Relaxed);
            }
            id = parent;
        }
    }

    /// Subtree bytes excluding links; complete once the scan is done.
    pub fn total(&self, id: usize) -> u64 {
        self.slot(id).total.load(Ordering::Relaxed)
    }

    /// Whether the subtree must be walked in order to be summed correctly.
    pub fn ordered(&self, id: usize) -> bool {
        self.slot(id).ordered.load(Ordering::Relaxed)
    }

    fn slot(&self, id: usize) -> &Slot {
        let page = self.pages[id / PAGE].get().expect("directory created");
        page[id % PAGE].get().expect("directory created")
    }
}

impl Index<usize> for Tree {
    type Output = Dir;

    fn index(&self, id: usize) -> &Dir {
        self.slot(id).dir.get().expect("directory scanned")
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
