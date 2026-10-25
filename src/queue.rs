//! A blocking LIFO work queue that knows when all work is done.
//!
//! A worker that pops an item is "active" until it calls `finish`, which
//! also enqueues any follow-up work. `pop` returns None only once the queue
//! is empty and no worker is active, since an active worker may still
//! produce more items. LIFO keeps the traversal roughly depth-first, which
//! bounds the number of pending items.

use std::sync::{Condvar, Mutex};

pub struct Queue<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

struct State<T> {
    items: Vec<T>,
    active: usize,
}

impl<T> Queue<T> {
    pub fn new() -> Queue<T> {
        let state = State { items: Vec::new(), active: 0 };
        Queue { state: Mutex::new(state), ready: Condvar::new() }
    }

    /// Enqueues work from outside a worker; call before workers start.
    pub fn push(&self, item: T) {
        self.state.lock().unwrap().items.push(item);
    }

    pub fn pop(&self) -> Option<T> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(item) = s.items.pop() {
                s.active += 1;
                return Some(item);
            }
            if s.active == 0 {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    /// Marks the caller's popped item done and enqueues its follow-up work.
    pub fn finish(&self, items: impl Iterator<Item = T>) {
        let mut s = self.state.lock().unwrap();
        let before = s.items.len();
        s.items.extend(items);
        s.active -= 1;
        let added = s.items.len() - before;
        let idle = s.items.is_empty() && s.active == 0;
        drop(s);

        match added {
            0 if idle => self.ready.notify_all(),
            0 => {}
            1 => self.ready.notify_one(),
            _ => self.ready.notify_all(),
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
                    while let Some(n) = queue.pop() {
                        count.fetch_add(1, Ordering::Relaxed);
                        let next: Vec<u32> = if n < 2 { vec![] } else { vec![n - 1, n - 2] };
                        queue.finish(next.into_iter());
                    }
                });
            }
        });
        // Nodes in a fib call tree of n=15: 2*fib(16)-1 = 1973.
        assert_eq!(count.load(Ordering::Relaxed), 1973);
    }
}
