//! One-click join attribution notes (port of two-bot `src/core/expectedJoins.ts`).
//!
//! A `guild.add_member` join consumes no invite, so the invite tracker would
//! file it `unknown`. The fix is a note taken BEFORE the add call — "expect a
//! join for this member within 30s, source `web:one_click`" — consumed by the
//! join handler. Deliberately in-memory: add call and gateway event are
//! seconds apart in one process; dying between them falls back to the honest
//! `unknown` the tracker already reports.

use std::collections::HashMap;

use crate::Snowflake;

/// TTL for an expectation note (legacy `DEFAULT_TTL_SECONDS`).
pub const EXPECTED_JOIN_TTL_SECONDS: u64 = 30;

/// The one source value this mechanism stamps (`WEB_ONE_CLICK_SOURCE`).
pub const WEB_ONE_CLICK_SOURCE: &str = "web:one_click";

/// Pending "expect this member" notes, keyed by guild + member.
/// The clock is injectable millis so tests can age entries.
pub struct ExpectedJoins {
    notes: HashMap<(Snowflake, Snowflake), (String, i64)>,
    ttl_ms: i64,
    now_ms: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for ExpectedJoins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExpectedJoins")
            .field("notes", &self.notes)
            .field("ttl_ms", &self.ttl_ms)
            .finish()
    }
}

impl ExpectedJoins {
    #[must_use]
    pub fn new() -> Self {
        Self {
            notes: HashMap::new(),
            ttl_ms: EXPECTED_JOIN_TTL_SECONDS as i64 * 1000,
            now_ms: Box::new(crate::funnel::now_millis_for_test),
        }
    }

    /// Test seam: custom TTL and clock.
    #[must_use]
    pub fn with_clock(ttl_seconds: u64, now_ms: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self {
            notes: HashMap::new(),
            ttl_ms: ttl_seconds as i64 * 1000,
            now_ms: Box::new(now_ms),
        }
    }

    fn now(&self) -> i64 {
        (self.now_ms)()
    }

    fn sweep(&mut self, t: i64) {
        self.notes.retain(|_, (_, at)| t - *at < self.ttl_ms);
    }

    /// Note that a join for this member is about to happen. Call BEFORE the
    /// add call: the gateway can deliver the join before the REST response
    /// comes back, and a note taken afterwards would lose exactly the joins
    /// it exists to attribute.
    pub fn expect(&mut self, guild_id: Snowflake, member_id: Snowflake, source: String) {
        let t = self.now();
        self.sweep(t);
        self.notes.insert((guild_id, member_id), (source, t));
    }

    /// The source for a join, if one was expected — consuming the note.
    /// `None` means unattributed-by-note; attribution proceeds by invite
    /// diff as normal. Expired notes read as absent.
    pub fn consume(&mut self, guild_id: Snowflake, member_id: Snowflake) -> Option<String> {
        let t = self.now();
        self.sweep(t);
        let (source, at) = self.notes.remove(&(guild_id, member_id))?;
        (t - at < self.ttl_ms).then_some(source)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.notes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }
}

impl Default for ExpectedJoins {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    fn clock(start: i64) -> (Arc<AtomicI64>, impl Fn() -> i64 + Send + Sync + 'static) {
        let t = Arc::new(AtomicI64::new(start));
        let c = t.clone();
        (t, move || c.load(Ordering::SeqCst))
    }

    #[test]
    fn consume_returns_and_clears_note() {
        let (_t, now) = clock(1_000_000);
        let mut e = ExpectedJoins::with_clock(30, now);
        e.expect(1, 2, WEB_ONE_CLICK_SOURCE.to_owned());
        assert_eq!(e.len(), 1);
        assert_eq!(e.consume(1, 2).as_deref(), Some(WEB_ONE_CLICK_SOURCE));
        assert!(e.consume(1, 2).is_none());
    }

    #[test]
    fn expired_note_reads_absent() {
        let (t, now) = clock(0);
        let mut e = ExpectedJoins::with_clock(30, now);
        e.expect(1, 2, WEB_ONE_CLICK_SOURCE.to_owned());
        t.store(30_001, Ordering::SeqCst);
        assert!(e.consume(1, 2).is_none());
    }

    #[test]
    fn unknown_member_consumes_nothing() {
        let (_t, now) = clock(0);
        let mut e = ExpectedJoins::with_clock(30, now);
        assert!(e.consume(9, 9).is_none());
        assert!(e.is_empty());
    }
}
