//! Waiting for the screen: change and stability detection over graphics
//! updates. Pure logic; the caller supplies all `Instant`s.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    Changed,
    Stable,
    Timeout,
}

/// Counts graphics updates and remembers when the last one arrived.
#[derive(Debug, Default, Clone)]
pub struct FrameWatch {
    count: u64,
    last: Option<Instant>,
}

impl FrameWatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_update(&mut self, now: Instant) {
        self.count += 1;
        self.last = Some(now);
    }

    /// Current update counter; pass it back to `changed_since`/`evaluate`.
    pub fn mark(&self) -> u64 {
        self.count
    }

    pub fn changed_since(&self, mark: u64) -> bool {
        self.count > mark
    }

    /// Time since the last update (`Duration::MAX` if none ever arrived).
    pub fn quiet_for(&self, now: Instant) -> Duration {
        match self.last {
            Some(t) => now.saturating_duration_since(t),
            None => Duration::MAX,
        }
    }

    /// `None` means keep waiting.
    pub fn evaluate(
        &self,
        mark: u64,
        want_change: bool,
        stable_ms: Option<u64>,
        started: Instant,
        timeout: Duration,
        now: Instant,
    ) -> Option<WaitResult> {
        let changed = self.changed_since(mark);
        let quiet = stable_ms.map(|ms| self.quiet_for(now) >= Duration::from_millis(ms));
        let done = match (want_change, quiet) {
            (true, Some(q)) if changed && q => Some(WaitResult::Stable),
            (true, None) if changed => Some(WaitResult::Changed),
            (false, Some(true)) => Some(WaitResult::Stable),
            _ => None,
        };
        done.or_else(|| {
            (now.saturating_duration_since(started) >= timeout).then_some(WaitResult::Timeout)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn change_detected() {
        let t0 = Instant::now();
        let mut w = FrameWatch::new();
        let mark = w.mark();
        let to = Duration::from_secs(5);
        assert_eq!(w.evaluate(mark, true, None, t0, to, ms(t0, 10)), None);
        w.on_update(ms(t0, 20));
        assert!(w.changed_since(mark));
        assert_eq!(
            w.evaluate(mark, true, None, t0, to, ms(t0, 30)),
            Some(WaitResult::Changed)
        );
    }

    #[test]
    fn stable_after_quiet_period() {
        let t0 = Instant::now();
        let mut w = FrameWatch::new();
        let mark = w.mark();
        let to = Duration::from_secs(5);
        w.on_update(ms(t0, 100));
        assert_eq!(w.quiet_for(ms(t0, 300)), Duration::from_millis(200));
        assert_eq!(
            w.evaluate(mark, false, Some(500), t0, to, ms(t0, 300)),
            None
        );
        assert_eq!(
            w.evaluate(mark, false, Some(500), t0, to, ms(t0, 600)),
            Some(WaitResult::Stable)
        );
    }

    #[test]
    fn change_then_stable() {
        let t0 = Instant::now();
        let mut w = FrameWatch::new();
        let mark = w.mark();
        let to = Duration::from_secs(5);
        // Quiet but no change yet: keep waiting.
        assert_eq!(
            w.evaluate(mark, true, Some(100), t0, to, ms(t0, 1000)),
            None
        );
        w.on_update(ms(t0, 1100));
        assert_eq!(
            w.evaluate(mark, true, Some(100), t0, to, ms(t0, 1150)),
            None
        );
        w.on_update(ms(t0, 1200));
        assert_eq!(
            w.evaluate(mark, true, Some(100), t0, to, ms(t0, 1250)),
            None
        );
        assert_eq!(
            w.evaluate(mark, true, Some(100), t0, to, ms(t0, 1300)),
            Some(WaitResult::Stable)
        );
    }

    #[test]
    fn timeout_when_nothing_happens() {
        let t0 = Instant::now();
        let w = FrameWatch::new();
        let to = Duration::from_secs(2);
        assert_eq!(w.evaluate(w.mark(), true, None, t0, to, ms(t0, 1999)), None);
        assert_eq!(
            w.evaluate(w.mark(), true, None, t0, to, ms(t0, 2000)),
            Some(WaitResult::Timeout)
        );
    }

    #[test]
    fn stable_only_without_change_requirement() {
        let t0 = Instant::now();
        let w = FrameWatch::new();
        let to = Duration::from_secs(5);
        // No update ever: already stable.
        assert_eq!(
            w.evaluate(w.mark(), false, Some(300), t0, to, ms(t0, 0)),
            Some(WaitResult::Stable)
        );
    }
}
