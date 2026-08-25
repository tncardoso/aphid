//! Identity for a loaded component instance.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};

/// A fiber's identity. Drawn fresh and **never reused**.
///
/// That is not bookkeeping hygiene, it is what makes a single comparison
/// enough. [`Target`](super::fiber::Target) digests the uids of the fibers
/// providing each declared key, so a provider that was replaced cannot be
/// mistaken for the one it replaced even when the two provide equal values.
/// Reuse a uid and that comparison starts lying.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Uid(NonZeroU32);

static NEXT: AtomicU32 = AtomicU32::new(1);

impl Uid {
    /// # Panics
    ///
    /// Past four billion fibers in one process, which would mean something is
    /// mounting in a loop.
    pub(crate) fn fresh() -> Uid {
        let raw = NEXT.fetch_add(1, Ordering::Relaxed);
        Uid(NonZeroU32::new(raw).expect("at most u32::MAX fibers per process"))
    }

    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl std::fmt::Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}
