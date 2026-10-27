//! Parallel directory scan. Workers pull directories off a shared queue,
//! stat every entry, and store one `Dir` record per directory in the
//! `Tree`, which sums subtrees as they complete. Anything whose effect
//! depends on traversal order (hard links, errors) is kept as an item for
//! the report pass to resolve in GNU's order.
//!
//! A large directory is split into chunks of entries that are stat'd by
//! several workers at once and reassembled in order, so one directory with
//! many files cannot leave the other workers idle.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crate::queue::Queue;
use crate::sys::{self, DirStream, Key, Stat};
use crate::tree::{self, Dir, ErrorKind, Item, ScanError, Tree};

/// gnulib fts reads at most this many entries before processing them, and
/// sorts each such batch by inode when it exceeds SORT_THRESHOLD. Both
/// affect output order, so we copy them.
const BATCH: usize = 100_000;
const SORT_THRESHOLD: usize = 10_000;

/// Directories with at least SPLIT_MIN entries are stat'd CHUNK entries
/// per job. A chunk costs an extra open of the directory, so it should be
/// well above that; a job this size takes well under a millisecond.
const CHUNK: usize = 512;
const SPLIT_MIN: usize = 2 * CHUNK;

enum Job {
    Dir { id: usize, path: Vec<u8>, depth: usize, stat: Stat },
    /// Stats the NUL-terminated `names` of part `index` of a split directory.
    Chunk { split: Arc<Split>, index: usize, names: Vec<u8> },
}

/// A directory being scanned by several workers.
struct Split {
    id: usize,
    path: Vec<u8>,
    depth: usize,
    stat: Stat,
    parts: Mutex<Parts>,
}

struct Parts {
    done: Vec<Option<Dir>>,
    /// Chunks not yet finished, plus one while the reader is still reading.
    pending: usize,
    /// A readdir failure, reported after everything that was read.
    error: Option<ScanError>,
}

impl Split {
    fn new(id: usize, path: Vec<u8>, depth: usize, stat: Stat) -> Split {
        let parts = Parts { done: Vec::new(), pending: 1, error: None };
        Split { id, path, depth, stat, parts: Mutex::new(parts) }
    }

    /// Registers `count` more chunks; returns the index of the first.
    fn add(&self, count: usize) -> usize {
        let mut p = self.parts.lock().unwrap();
        p.pending += count;
        let first = p.done.len();
        p.done.resize_with(first + count, || None);
        first
    }

    fn complete(&self, index: usize, part: Dir) -> Option<Dir> {
        let mut p = self.parts.lock().unwrap();
        p.done[index] = Some(part);
        self.release(p)
    }

    /// Drops the reader's hold; `error` is the readdir failure, if any.
    fn finish_reading(&self, error: Option<ScanError>) -> Option<Dir> {
        let mut p = self.parts.lock().unwrap();
        p.error = error;
        self.release(p)
    }

    /// Returns the assembled directory once the last hold is released.
    fn release(&self, mut p: MutexGuard<Parts>) -> Option<Dir> {
        p.pending -= 1;
        if p.pending > 0 {
            return None;
        }
        let mut dir = Dir::new(self.depth, &self.stat);
        for part in p.done.iter_mut() {
            let mut part = part.take().expect("every chunk completed");
            dir.bytes += part.bytes;
            dir.items.append(&mut part.items);
        }
        if let Some(e) = p.error.take() {
            dir.items.push(Item::Error(e));
        }
        Some(dir)
    }
}

/// Per-worker buffers, reused across directories.
struct Scratch {
    buf: Vec<u8>,
    names: Vec<u8>,
    entries: Vec<(u64, usize)>,
    children: Vec<Job>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch { buf: vec![0; 64 * 1024], names: Vec::new(), entries: Vec::new(), children: Vec::new() }
    }
}

pub struct Scanner {
    queue: Queue<Job>,
    next_id: AtomicUsize,
    /// Single-link files that must still be de-duplicated because they were
    /// also named on the command line.
    tracked: HashSet<Key>,
    max_depth: usize,
    tree: Tree,
}

impl Scanner {
    pub fn new(tracked: HashSet<Key>, max_depth: usize) -> Scanner {
        Scanner { queue: Queue::new(), next_id: AtomicUsize::new(0), tracked, max_depth, tree: Tree::new() }
    }

    pub fn add_root(&self, path: Vec<u8>, stat: Stat) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tree.create(id, None);
        self.queue.push(Job::Dir { id, path, depth: 0, stat });
        id
    }

    /// Scans everything reachable from the roots. The calling thread works
    /// too, and returns as soon as the queue is drained, without waiting
    /// for the other workers to exit; everything they stored is complete
    /// by then, since a worker stores its result before it asks for more.
    pub fn run(self: &Arc<Self>, threads: usize) {
        for _ in 1..threads {
            let scanner = Arc::clone(self);
            if thread::Builder::new().spawn(move || scanner.work()).is_err() {
                break;
            }
        }
        self.work();
    }

    /// Indexed by the ids handed out by `add_root` and `Item::Dir`.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    fn work(&self) {
        sys::unshare_fd_table();
        let mut scratch = Scratch::new();
        let mut next = self.queue.pop();
        while let Some(job) = next {
            let out = match job {
                Job::Dir { id, path, depth, stat } => self.scan_dir(id, path, depth, stat, &mut scratch),
                Job::Chunk { split, index, names } => self.scan_chunk(&split, index, &names, &mut scratch),
            };
            if let Some((id, dir)) = out {
                self.tree.finish(id, dir);
            }
            next = self.queue.next(&mut scratch.children);
        }
    }

    /// Returns the finished record, or None if the directory was split and
    /// another worker will finish it.
    fn scan_dir(&self, id: usize, path: Vec<u8>, depth: usize, stat: Stat, scratch: &mut Scratch) -> Option<(usize, Dir)> {
        let mut dir = Dir::new(depth, &stat);
        let Scratch { buf, names, entries, children } = scratch;
        let fd = match open_dir(&path) {
            Ok(fd) => fd,
            Err(e) => return self.unreadable(id, dir, path, e),
        };
        let mut stream = DirStream::new(fd.as_fd(), buf);
        let more = match read_batch(&mut stream, fd.as_fd(), names, entries) {
            Ok(more) => more,
            Err(e) => return self.unreadable(id, dir, path, e),
        };
        if more || entries.len() >= SPLIT_MIN {
            let split = Arc::new(Split::new(id, path, depth, stat));
            let dir = self.scan_split(&split, &mut stream, more, names, entries)?;
            return Some((id, self.finish(dir, split.path.clone())));
        }
        for &(_, start) in entries.iter() {
            let name = CStr::from_bytes_until_nul(&names[start..]).unwrap();
            self.visit(fd.as_fd(), id, &path, name, &mut dir, children);
        }
        Some((id, self.finish(dir, path)))
    }

    fn unreadable(&self, id: usize, mut dir: Dir, path: Vec<u8>, e: io::Error) -> Option<(usize, Dir)> {
        dir.items.push(Item::Error(ScanError::new(ErrorKind::ReadDir, path.clone(), e)));
        Some((id, self.finish(dir, path)))
    }

    /// Shares the batch already in `names`/`entries` and every further
    /// batch of `stream` as chunk jobs, then drops the reader's hold.
    fn scan_split(
        &self,
        split: &Arc<Split>,
        stream: &mut DirStream,
        mut more: bool,
        names: &mut Vec<u8>,
        entries: &mut Vec<(u64, usize)>,
    ) -> Option<Dir> {
        let error = loop {
            self.share_chunks(split, names, entries);
            if !more {
                break None;
            }
            more = match read_batch(stream, stream.fd(), names, entries) {
                Ok(more) => more,
                Err(e) => break Some(ScanError::new(ErrorKind::ReadDir, split.path.clone(), e)),
            };
        };
        split.finish_reading(error)
    }

    /// Hands one batch of a split directory to the queue in CHUNK-sized jobs.
    fn share_chunks(&self, split: &Arc<Split>, names: &[u8], entries: &[(u64, usize)]) {
        let chunks: Vec<_> = entries.chunks(CHUNK).collect();
        let first = split.add(chunks.len());
        let mut jobs = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut buf = Vec::with_capacity(chunk.len() * 16);
            for &(_, start) in chunk {
                let name = CStr::from_bytes_until_nul(&names[start..]).unwrap();
                buf.extend_from_slice(name.to_bytes_with_nul());
            }
            jobs.push(Job::Chunk { split: split.clone(), index: first + i, names: buf });
        }
        self.queue.share(&mut jobs);
    }

    fn scan_chunk(&self, split: &Split, index: usize, names: &[u8], scratch: &mut Scratch) -> Option<(usize, Dir)> {
        // A part carries only its entries' bytes; the directory's own are
        // counted once, at assembly.
        let mut part = Dir::new(split.depth, &split.stat);
        part.bytes = 0;
        match open_dir(&split.path) {
            Ok(fd) => {
                for name in names.split_inclusive(|&b| b == 0) {
                    let name = CStr::from_bytes_with_nul(name).unwrap();
                    self.visit(fd.as_fd(), split.id, &split.path, name, &mut part, &mut scratch.children);
                }
            }
            Err(e) => part.items.push(Item::Error(ScanError::new(ErrorKind::ReadDir, split.path.clone(), e))),
        }
        let dir = split.complete(index, part)?;
        Some((split.id, self.finish(dir, split.path.clone())))
    }

    /// Directories below --max-depth are never printed, so drop their paths.
    fn finish(&self, mut dir: Dir, path: Vec<u8>) -> Dir {
        if dir.depth <= self.max_depth {
            dir.path = path;
        }
        dir
    }

    /// Stats one entry of directory `parent` (at `path`) into `dir`, which
    /// is either the directory's own record or one chunk of it.
    fn visit(&self, fd: BorrowedFd, parent: usize, path: &[u8], name: &CStr, dir: &mut Dir, children: &mut Vec<Job>) {
        let stat = match sys::stat_at(fd, name) {
            Ok(stat) => stat,
            Err(e) => {
                let path = tree::join(path, name.to_bytes());
                dir.items.push(Item::Error(ScanError::new(ErrorKind::Access, path, e)));
                return;
            }
        };
        if stat.is_dir {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            self.tree.create(id, Some(parent));
            let path = tree::join(path, name.to_bytes());
            dir.items.push(Item::Dir(id));
            children.push(Job::Dir { id, path, depth: dir.depth + 1, stat });
            return;
        }
        if stat.nlink > 1 || (!self.tracked.is_empty() && self.tracked.contains(&stat.key)) {
            dir.items.push(Item::Link { key: stat.key, bytes: stat.bytes });
            return;
        }
        dir.bytes += stat.bytes;
    }
}

fn open_dir(path: &[u8]) -> io::Result<OwnedFd> {
    let cpath = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    sys::open_dir(&cpath)
}

/// Fills `entries` with up to BATCH (inode, offset into `names`) pairs, in
/// the order GNU du would visit them. Returns whether the directory may
/// have more entries.
fn read_batch(stream: &mut DirStream, fd: BorrowedFd, names: &mut Vec<u8>, entries: &mut Vec<(u64, usize)>) -> io::Result<bool> {
    names.clear();
    entries.clear();
    let mut more = true;
    while entries.len() < BATCH {
        let Some(entry) = stream.next()? else {
            more = false;
            break;
        };
        entries.push((entry.ino, names.len()));
        names.extend_from_slice(entry.name.to_bytes_with_nul());
    }
    if entries.len() > SORT_THRESHOLD && sys::inode_sort_useful(fd) {
        entries.sort_unstable_by_key(|&(ino, _)| ino);
    }
    Ok(more)
}
