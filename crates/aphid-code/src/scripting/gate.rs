//! One call into a plugin at a time.
//!
//! A plugin is called from several threads: agent listeners run on the agent's
//! task, tool bodies on the blocking pool, ticks and commands on the hub, and
//! model replies on their own runtime. Rhai's `sync` values do not make that
//! safe — two closures that change the same captured map race, and Rhai turns
//! the race into an error rather than a wait. So every call into a plugin goes
//! through its gate first.
//!
//! The gate is re-entrant for the thread already inside it. A plugin that
//! calls a service it provides itself, or whose listener runs a nested
//! announcement that reaches it again, would otherwise wait for itself forever.

use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

/// A re-entrant lock with nothing inside it.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    held: Mutex<Held>,
    free: Condvar,
}

#[derive(Debug, Default)]
struct Held {
    owner: Option<ThreadId>,
    depth: usize,
}

impl Gate {
    /// Wait until no other thread is inside, then go in.
    pub(crate) fn enter(&self) -> Pass<'_> {
        let me = thread::current().id();
        let mut held = lock(&self.held);
        loop {
            match held.owner {
                None => {
                    held.owner = Some(me);
                    held.depth = 1;
                    break;
                }
                Some(owner) if owner == me => {
                    held.depth += 1;
                    break;
                }
                Some(_) => {
                    held = match self.free.wait(held) {
                        Ok(held) => held,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
            }
        }
        Pass { gate: self }
    }

    fn leave(&self) {
        let mut held = lock(&self.held);
        held.depth = held.depth.saturating_sub(1);
        if held.depth == 0 {
            held.owner = None;
            drop(held);
            self.free.notify_one();
        }
    }
}

/// Proof of being inside. Leaving is dropping it.
pub(crate) struct Pass<'a> {
    gate: &'a Gate,
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.gate.leave();
    }
}

/// A poisoned gate is still a gate: the panic that poisoned it is reported
/// where it happened, and the counter it guards is still right.
fn lock(held: &Mutex<Held>) -> std::sync::MutexGuard<'_, Held> {
    match held.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_same_thread_goes_in_twice() {
        let gate = Gate::default();
        let _outer = gate.enter();
        let _inner = gate.enter();
    }

    #[test]
    fn another_thread_waits_until_the_first_leaves() {
        let gate = Arc::new(Gate::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let inside = Arc::clone(&inside);
                let most = Arc::clone(&most);
                thread::spawn(move || {
                    for _ in 0..20 {
                        let _pass = gate.enter();
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_micros(200));
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("no panic");
        }

        assert_eq!(most.load(Ordering::SeqCst), 1);
    }
}
