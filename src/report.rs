//! Serial pass over the scanned tree: sums sizes post-order, de-duplicates
//! in GNU traversal order, and prints lines and errors.

use std::collections::HashSet;
use std::io::{self, Write};

use crate::human;
use crate::sys::Key;
use crate::tree::{Dir, Item, Root, ScanError};

pub struct Options {
    pub human: bool,
    pub max_depth: usize,
    /// GNU du de-duplicates every file and directory, not just hard links,
    /// when given more than one operand, so overlapping operands such as
    /// `du a a/b` count each file once.
    pub dedup_all: bool,
}

pub struct Report<'a, W: Write> {
    tree: &'a [Dir],
    opts: Options,
    seen: HashSet<Key>,
    out: W,
    failed: bool,
}

struct Frame {
    id: usize,
    next: usize,
    total: u64,
}

impl<'a, W: Write> Report<'a, W> {
    pub fn new(tree: &'a [Dir], opts: Options, out: W) -> Report<'a, W> {
        Report { tree, opts, seen: HashSet::new(), out, failed: false }
    }

    pub fn root(&mut self, root: &Root) -> io::Result<()> {
        match root {
            Root::Missing(e) => self.error(e),
            Root::File { path, stat } => {
                if !self.first_visit(stat.key, self.opts.dedup_all || stat.nlink > 1) {
                    return Ok(());
                }
                self.line(stat.bytes, path)
            }
            Root::Dir(id) => {
                if !self.first_visit(self.tree[*id].key, self.opts.dedup_all) {
                    return Ok(());
                }
                self.walk(*id)
            }
        }
    }

    /// Flushes output; returns false if any error was reported.
    pub fn finish(mut self) -> io::Result<bool> {
        self.out.flush()?;
        Ok(!self.failed)
    }

    /// Post-order walk with an explicit stack, so deep trees cannot
    /// overflow the call stack.
    fn walk(&mut self, root: usize) -> io::Result<()> {
        let tree = self.tree;
        let mut stack = vec![Frame { id: root, next: 0, total: tree[root].bytes }];
        while let Some(top) = stack.last_mut() {
            let dir = &tree[top.id];
            let Some(item) = dir.items.get(top.next) else {
                let done = stack.pop().unwrap();
                if dir.depth <= self.opts.max_depth {
                    self.line(done.total, &dir.path)?;
                }
                if let Some(parent) = stack.last_mut() {
                    parent.total += done.total;
                }
                continue;
            };
            top.next += 1;
            match item {
                Item::Dir(id) => {
                    if self.first_visit(tree[*id].key, self.opts.dedup_all) {
                        stack.push(Frame { id: *id, next: 0, total: tree[*id].bytes });
                    }
                }
                Item::Link { key, bytes } => {
                    if self.seen.insert(*key) {
                        top.total += bytes;
                    }
                }
                Item::Error(e) => self.error(e)?,
            }
        }
        Ok(())
    }

    fn first_visit(&mut self, key: Key, track: bool) -> bool {
        !track || self.seen.insert(key)
    }

    fn line(&mut self, bytes: u64, path: &[u8]) -> io::Result<()> {
        let size = if self.opts.human { human::human(bytes) } else { human::blocks(bytes) };
        self.out.write_all(size.as_bytes())?;
        self.out.write_all(b"\t")?;
        self.out.write_all(path)?;
        self.out.write_all(b"\n")
    }

    /// Flushes stdout first so errors interleave with output exactly as
    /// they would from a sequential du.
    fn error(&mut self, e: &ScanError) -> io::Result<()> {
        self.failed = true;
        self.out.flush()?;
        eprintln!("du: {e}");
        Ok(())
    }
}
