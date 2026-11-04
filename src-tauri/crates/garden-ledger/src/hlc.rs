//! Hybrid logical clock: strictly increasing timestamps from a possibly non-monotone wall clock.

use crate::frame::Hlc;

/// A hybrid logical clock. Every value it returns is strictly greater than the previous one.
#[derive(Clone, Debug)]
pub struct HlcClock {
    last: Hlc,
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

/// The successor of `h` within the same millisecond, rolling to the next millisecond on overflow.
fn bump(ms: u64, count: u32) -> Hlc {
    match count.checked_add(1) {
        Some(c) => Hlc { ms, count: c },
        None => Hlc {
            ms: ms.saturating_add(1),
            count: 0,
        },
    }
}

impl HlcClock {
    /// A clock that has issued nothing yet.
    pub fn new() -> Self {
        HlcClock {
            last: Hlc { ms: 0, count: 0 },
        }
    }

    /// The last value issued (or zero).
    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Issue a timestamp for a local event at wall-clock `wall_ms`.
    pub fn now(&mut self, wall_ms: u64) -> Hlc {
        let next = if wall_ms > self.last.ms {
            Hlc {
                ms: wall_ms,
                count: 0,
            }
        } else {
            bump(self.last.ms, self.last.count)
        };
        self.last = next;
        next
    }

    /// Fold in a timestamp seen from a remote event and issue one that follows both it and
    /// everything this clock issued before.
    pub fn observe(&mut self, remote: Hlc, wall_ms: u64) -> Hlc {
        let ms = wall_ms.max(self.last.ms).max(remote.ms);
        let next = if ms == self.last.ms && ms == remote.ms {
            bump(ms, self.last.count.max(remote.count))
        } else if ms == self.last.ms {
            bump(ms, self.last.count)
        } else if ms == remote.ms {
            bump(ms, remote.count)
        } else {
            Hlc { ms, count: 0 }
        };
        self.last = next;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_strictly_increasing_with_stalled_wall() {
        let mut c = HlcClock::new();
        let a = c.now(100);
        let b = c.now(100);
        let d = c.now(50);
        assert!(a < b && b < d);
        assert_eq!(a, Hlc { ms: 100, count: 0 });
        assert_eq!(d, Hlc { ms: 100, count: 2 });
    }

    #[test]
    fn observe_follows_remote() {
        let mut c = HlcClock::new();
        let a = c.now(10);
        let r = Hlc { ms: 500, count: 7 };
        let b = c.observe(r, 20);
        assert!(b > a && b > r);
        let d = c.observe(Hlc { ms: 1, count: 0 }, 30);
        assert!(d > b);
    }
}
