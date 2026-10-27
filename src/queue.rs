//! A blocking LIFO work queue that knows when all work is done.
//!
//! A worker that pops an item is "active" until it calls `next`, which
//! also enqueues any follow-up work. `pop` and `next` return None only once
//! the queue is empty and no worker is active, since an active worker may
//! still produce more items. LIFO keeps the traversal roughly depth-first,
//! which bounds the number of pending items.

use std::sync::{Condvar, Mutex, MutexGuard};

pub struct Queue<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

struct State<T> {
    items: Vec<T>,
    active: usize,
    waiting: usize,
}

impl<T> Queue<T> {
    pub fn new() -> Queue<T> {
        let state = State { items: Vec::new(), active: 0, waiting: 0 };
        Queue { state: Mutex::new(state), ready: Condvar::new() }
    }

    /// Enqueues work from outside a worker; call before workers start.
    pub fn push(&self, item: T) {
        self.state.lock().unwrap().items.push(item);
    }

    pub fn pop(&self) -> Option<T> {
        let s = self.state.lock().unwrap();
        self.wait_for_item(s)
    }

    /// Marks the caller's popped item done, enqueues its follow-up work,
    /// and returns the next item to process. The caller keeps the last of
    /// its own children, so a busy worker only takes the lock to share
    /// work, never to fetch it.
    pub fn next(&self, children: &mut Vec<T>) -> Option<T> {
        let keep = children.pop();
        let mut s = self.state.lock().unwrap();
        let wake = children.len().min(s.waiting);
        s.items.append(children);
        if keep.is_some() {
            let all = wake == s.waiting;
            drop(s);
            self.notify(wake, all);
            return keep;
        }
        s.active -= 1;
        self.wait_for_item(s)
    }

    /// Enqueues extra work from a worker that is not done with its own item.
    pub fn share(&self, items: &mut Vec<T>) {
        let mut s = self.state.lock().unwrap();
        let wake = items.len().min(s.waiting);
        let all = wake == s.waiting;
        s.items.append(items);
        drop(s);
        self.notify(wake, all);
    }

    fn wait_for_item(&self, mut s: MutexGuard<State<T>>) -> Option<T> {
        loop {
            if let Some(item) = s.items.pop() {
                s.active += 1;
                return Some(item);
            }
            if s.active == 0 {
                drop(s);
                self.ready.notify_all();
                return None;
            }
            s.waiting += 1;
            s = self.ready.wait(s).unwrap();
            s.waiting -= 1;
        }
    }

    /// Every notify is a futex syscall, so wake only as many workers as
    /// there are new items, and none when nobody is waiting.
    fn notify(&self, wake: usize, all: bool) {
        match wake {
            0 => {}
            _ if all => self.ready.notify_all(),
            n => (0..n).for_each(|_| self.ready.notify_one()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn processes_generated_work_then_stops() {
        // Each item n spawns n-1 and n-2 (a Fibonacci-shaped tree).
        let queue = Queue::new();
        let count = AtomicUsize::new(0);
        queue.push(15u32);
        thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let mut next = queue.pop();
                    while let Some(n) = next {
                        count.fetch_add(1, Ordering::Relaxed);
                        let mut children = if n < 2 { vec![] } else { vec![n - 1, n - 2] };
                        next = queue.next(&mut children);
                    }
                });
            }
        });
        // Nodes in a fib call tree of n=15: 2*fib(16)-1 = 1973.
        assert_eq!(count.load(Ordering::Relaxed), 1973);
    }
}
