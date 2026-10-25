//! Parallel directory scan. Workers pull directories off a shared queue,
//! stat every entry, and emit one `Dir` record per directory. No sizes are
//! propagated here; the report pass does that serially, which keeps output
//! order and hard-link attribution identical to a sequential du.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crate::queue::Queue;
use crate::sys::{self, DirStream, Key, Stat};
use crate::tree::{self, Dir, ErrorKind, Item, ScanError};

/// gnulib fts reads at most this many entries before processing them, and
/// sorts each such batch by inode when it exceeds SORT_THRESHOLD. Both
/// affect output order, so we copy them.
const BATCH: usize = 100_000;
const SORT_THRESHOLD: usize = 10_000;

struct Job {
    id: usize,
    path: Vec<u8>,
    depth: usize,
    stat: Stat,
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
}

impl Scanner {
    pub fn new(tracked: HashSet<Key>, max_depth: usize) -> Scanner {
        Scanner { queue: Queue::new(), next_id: AtomicUsize::new(0), tracked, max_depth }
    }

    pub fn add_root(&self, path: Vec<u8>, stat: Stat) -> usize {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.queue.push(Job { id, path, depth: 0, stat });
        id
    }

    /// Scans everything reachable from the roots; the result is indexed by
    /// the ids handed out by `add_root` and `Item::Dir`.
    pub fn run(self, threads: usize) -> Vec<Dir> {
        let batches: Vec<Vec<(usize, Dir)>> = thread::scope(|s| {
            let workers: Vec<_> = (0..threads).map(|_| s.spawn(|| self.work())).collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });

        let mut dirs: Vec<Option<Dir>> = Vec::new();
        dirs.resize_with(self.next_id.into_inner(), || None);
        for (id, dir) in batches.into_iter().flatten() {
            dirs[id] = Some(dir);
        }
        dirs.into_iter().map(|d| d.expect("every queued directory is scanned")).collect()
    }

    fn work(&self) -> Vec<(usize, Dir)> {
        let mut scratch = Scratch::new();
        let mut done = Vec::new();
        while let Some(job) = self.queue.pop() {
            let id = job.id;
            let dir = self.scan(job, &mut scratch);
            self.queue.finish(scratch.children.drain(..));
            done.push((id, dir));
        }
        done
    }

    fn scan(&self, job: Job, scratch: &mut Scratch) -> Dir {
        let mut dir = Dir::new(job.depth, &job.stat);
        if let Err(e) = self.read(&job.path, &mut dir, scratch) {
            dir.items.push(Item::Error(ScanError::new(ErrorKind::ReadDir, job.path.clone(), e)));
        }
        if job.depth <= self.max_depth {
            dir.path = job.path;
        }
        dir
    }

    fn read(&self, path: &[u8], dir: &mut Dir, scratch: &mut Scratch) -> io::Result<()> {
        let cpath = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let fd = sys::open_dir(&cpath)?;
        let mut stream = DirStream::new(fd.as_fd(), &mut scratch.buf);
        loop {
            let more = read_batch(&mut stream, &mut scratch.names, &mut scratch.entries)?;
            let entries = &mut scratch.entries;
            if entries.len() > SORT_THRESHOLD && sys::inode_sort_useful(fd.as_fd()) {
                entries.sort_unstable_by_key(|&(ino, _)| ino);
            }
            for &(_, start) in entries.iter() {
                let name = CStr::from_bytes_until_nul(&scratch.names[start..]).unwrap();
                self.visit(fd.as_fd(), path, name, dir, &mut scratch.children);
            }
            if !more {
                return Ok(());
            }
        }
    }

    fn visit(&self, fd: BorrowedFd, path: &[u8], name: &CStr, dir: &mut Dir, children: &mut Vec<Job>) {
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
            let path = tree::join(path, name.to_bytes());
            dir.items.push(Item::Dir(id));
            children.push(Job { id, path, depth: dir.depth + 1, stat });
            return;
        }
        if stat.nlink > 1 || (!self.tracked.is_empty() && self.tracked.contains(&stat.key)) {
            dir.items.push(Item::Link { key: stat.key, bytes: stat.bytes });
            return;
        }
        dir.bytes += stat.bytes;
    }
}

/// Fills `entries` with up to BATCH (inode, offset into `names`) pairs.
/// Returns whether the directory may have more entries.
fn read_batch(stream: &mut DirStream, names: &mut Vec<u8>, entries: &mut Vec<(u64, usize)>) -> io::Result<bool> {
    names.clear();
    entries.clear();
    while entries.len() < BATCH {
        let Some(entry) = stream.next()? else {
            return Ok(false);
        };
        entries.push((entry.ino, names.len()));
        names.extend_from_slice(entry.name.to_bytes_with_nul());
    }
    Ok(true)
}
