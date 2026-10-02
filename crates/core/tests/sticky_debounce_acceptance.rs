//! Sticky debounce and activity-gate acceptance (TOG-12542).
//!
//! Pure, offline: pins the public `two_bot_core::sticky` API only — no
//! database, no Discord, no feature flags. Parity: `docs/parity.md` §1
//! #20–21 (`/sticky`, `/sticky-remove`) and the §4 automation re-post path;
//! legacy `onChannelActivity` order claim → post → record → delete previous.
//! The atomic claim race itself is covered by the ignored live store suite
//! (`sticky_store_live.rs`); see `docs/sticky-debounce-acceptance.md`.

use two_bot_core::sticky::{
    activity_eligible, claim_blocks, decide_activity, normalize_debounce, repost_due,
    ActivityDecision, StickyError, StickyState, CLAIM_EXPIRY_SECONDS, DEFAULT_DEBOUNCE_SECONDS,
    MAX_DEBOUNCE_SECONDS, MIN_DEBOUNCE_SECONDS,
};

const T0: i64 = 1_700_000_000_000;

fn sticky(debounce_seconds: u64, last: Option<(&str, i64)>) -> StickyState {
    StickyState {
        guild_id: "g1".into(),
        channel_id: "c1".into(),
        body: "Read the rules".into(),
        debounce_seconds,
        enabled: true,
        last_message_id: last.map(|(id, _)| id.to_string()),
        last_posted_at_ms: last.map(|(_, at)| at),
    }
}

/// Human activity in the sticky's own guild and channel.
fn human(state: Option<&StickyState>, now_ms: i64) -> ActivityDecision {
    decide_activity(state, false, true, true, now_ms)
}

// ---------------------------------------------------------------------------
// (1) normalize_debounce: omitted → 5, range 1–300
// ---------------------------------------------------------------------------

#[test]
fn debounce_omitted_defaults_to_five_seconds() {
    assert_eq!(normalize_debounce(None), Ok(5));
    assert_eq!(DEFAULT_DEBOUNCE_SECONDS, 5);
}

#[test]
fn debounce_refuses_zero_and_three_hundred_one() {
    assert_eq!(normalize_debounce(Some(0)), Err(StickyError::BadDebounce));
    assert_eq!(normalize_debounce(Some(301)), Err(StickyError::BadDebounce));
    assert_eq!(
        StickyError::BadDebounce.to_string(),
        "Debounce must be between 1 and 300 seconds."
    );
}

#[test]
fn debounce_accepts_inclusive_bounds_and_refuses_outside() {
    assert_eq!((MIN_DEBOUNCE_SECONDS, MAX_DEBOUNCE_SECONDS), (1, 300));
    assert_eq!(normalize_debounce(Some(1)), Ok(1));
    assert_eq!(normalize_debounce(Some(5)), Ok(5));
    assert_eq!(normalize_debounce(Some(300)), Ok(300));
    for refused in [-1, i64::MIN, 302, i64::MAX] {
        assert_eq!(
            normalize_debounce(Some(refused)),
            Err(StickyError::BadDebounce),
            "debounce {refused} must be refused"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) repost_due: fires only once the debounce has elapsed
// ---------------------------------------------------------------------------

#[test]
fn repost_due_when_never_posted() {
    assert!(repost_due(None, T0, 5));
    assert!(repost_due(None, T0, 300));
}

#[test]
fn repost_due_only_after_debounce_seconds_elapse() {
    for debounce in [1_u64, 5, 300] {
        let window = debounce as i64 * 1000;
        assert!(
            !repost_due(Some(T0), T0, debounce),
            "{debounce}s: same instant"
        );
        assert!(
            !repost_due(Some(T0), T0 + window - 1, debounce),
            "{debounce}s: one millisecond early"
        );
        assert!(
            repost_due(Some(T0), T0 + window, debounce),
            "{debounce}s: exact boundary posts (legacy last_posted_at <= cutoff)"
        );
        assert!(repost_due(Some(T0), T0 + window + 1, debounce));
    }
}

#[test]
fn repost_holds_under_clock_skew() {
    assert!(!repost_due(Some(T0), T0 - 1, 5));
    assert!(!repost_due(Some(T0), T0 - 3_600_000, 5));
}

// ---------------------------------------------------------------------------
// (3) claim_blocks: a live claim refuses a second claimant
// ---------------------------------------------------------------------------

#[test]
fn no_claim_never_blocks() {
    assert!(!claim_blocks(None, T0));
}

#[test]
fn second_claim_inside_window_is_refused() {
    assert_eq!(CLAIM_EXPIRY_SECONDS, 60);
    assert!(claim_blocks(Some(T0), T0));
    assert!(claim_blocks(Some(T0), T0 + 1));
    assert!(claim_blocks(Some(T0), T0 + 59_999));
}

#[test]
fn claim_goes_stale_at_sixty_seconds() {
    assert!(!claim_blocks(Some(T0), T0 + 60_000));
    assert!(!claim_blocks(Some(T0), T0 + 600_000));
}

#[test]
fn future_dated_claim_conservatively_blocks() {
    assert!(claim_blocks(Some(T0 + 5_000), T0));
}

// ---------------------------------------------------------------------------
// (4) activity_eligible: bots and guild/channel mismatches never reach the check
// ---------------------------------------------------------------------------

#[test]
fn activity_gate_truth_table() {
    for author_is_bot in [false, true] {
        for guild_matches in [false, true] {
            for channel_present in [false, true] {
                let expected = !author_is_bot && guild_matches && channel_present;
                assert_eq!(
                    activity_eligible(author_is_bot, guild_matches, channel_present),
                    expected,
                    "bot={author_is_bot} guild={guild_matches} channel={channel_present}"
                );
            }
        }
    }
}

#[test]
fn ineligible_activity_is_ignored_even_when_repost_is_due() {
    let due = sticky(5, None);
    for (bot, guild, channel, why) in [
        (true, true, true, "bot author (including our own re-post)"),
        (false, false, true, "foreign or absent guild"),
        (false, true, false, "absent channel"),
    ] {
        assert_eq!(
            decide_activity(Some(&due), bot, guild, channel, T0),
            ActivityDecision::Ignore,
            "{why}"
        );
    }
}

#[test]
fn eligible_activity_without_enabled_sticky_is_no_sticky() {
    assert_eq!(human(None, T0), ActivityDecision::NoSticky);
    let mut disabled = sticky(5, None);
    disabled.enabled = false;
    assert_eq!(human(Some(&disabled), T0), ActivityDecision::NoSticky);
}

// ---------------------------------------------------------------------------
// (5) decide_activity end state: old then fresh, post before delete
// ---------------------------------------------------------------------------

/// Minimal channel model: ordered message ids, oldest first. Drives a
/// `Repost` plan in the documented wire order (post → record → delete
/// previous) so the visible end state can be asserted.
struct Channel {
    messages: Vec<String>,
    next_id: u32,
}

impl Channel {
    fn with(messages: &[&str]) -> Self {
        Self {
            messages: messages.iter().map(|m| (*m).to_string()).collect(),
            next_id: 0,
        }
    }

    /// Apply one decision; `post_ok = false` simulates a failed Discord post.
    /// Returns the updated sticky row (unchanged unless the post landed).
    fn apply(
        &mut self,
        state: &StickyState,
        decision: ActivityDecision,
        now_ms: i64,
        post_ok: bool,
    ) -> StickyState {
        let ActivityDecision::Repost {
            body,
            previous_message_id,
        } = decision
        else {
            return state.clone();
        };
        assert_eq!(body, state.body, "re-post carries the stored body");
        // Post the replacement first; a failure stops before any delete.
        if !post_ok {
            return state.clone();
        }
        self.next_id += 1;
        let fresh = format!("fresh-{}", self.next_id);
        self.messages.push(fresh.clone());
        // Record the replacement, then best-effort delete the previous copy.
        let mut recorded = state.clone();
        recorded.last_message_id = Some(fresh);
        recorded.last_posted_at_ms = Some(now_ms);
        if let Some(previous) = previous_message_id {
            self.messages.retain(|m| *m != previous);
        }
        recorded
    }
}

#[test]
fn repost_plan_targets_the_previous_sticky_for_deletion() {
    let state = sticky(5, Some(("old", T0)));
    assert_eq!(
        human(Some(&state), T0 + 5_000),
        ActivityDecision::Repost {
            body: "Read the rules".into(),
            previous_message_id: Some("old".into()),
        }
    );
    let first = sticky(5, None);
    assert_eq!(
        human(Some(&first), T0),
        ActivityDecision::Repost {
            body: "Read the rules".into(),
            previous_message_id: None,
        }
    );
}

#[test]
fn successful_repost_leaves_fresh_sticky_at_the_bottom_and_old_gone() {
    let state = sticky(5, Some(("old", T0)));
    let mut channel = Channel::with(&["old", "chat-1", "chat-2"]);
    let now = T0 + 5_000;
    let after = channel.apply(&state, human(Some(&state), now), now, true);

    assert_eq!(channel.messages, ["chat-1", "chat-2", "fresh-1"]);
    assert_eq!(after.last_message_id.as_deref(), Some("fresh-1"));
    assert_eq!(after.last_posted_at_ms, Some(now));
}

#[test]
fn failed_post_never_drops_the_existing_sticky() {
    let state = sticky(5, Some(("old", T0)));
    let mut channel = Channel::with(&["old", "chat-1"]);
    let now = T0 + 5_000;
    let after = channel.apply(&state, human(Some(&state), now), now, false);

    assert_eq!(channel.messages, ["old", "chat-1"], "old sticky survives");
    assert_eq!(after, state, "nothing recorded; the next activity retries");
    assert!(matches!(
        human(Some(&after), now + 1),
        ActivityDecision::Repost { previous_message_id: Some(ref id), .. } if id == "old"
    ));
}

#[test]
fn burst_coalesces_until_window_then_chains_old_then_fresh() {
    let mut state = sticky(5, Some(("old", T0)));
    let mut channel = Channel::with(&["old"]);

    let now = T0 + 5_000;
    state = channel.apply(&state, human(Some(&state), now), now, true);
    assert_eq!(channel.messages, ["fresh-1"]);

    // The bot's own re-post never retriggers.
    assert_eq!(
        decide_activity(Some(&state), true, true, true, now),
        ActivityDecision::Ignore
    );

    // Human chatter inside the fresh window is held; nothing moves.
    for offset in [0, 1, 2_500, 4_999] {
        assert_eq!(human(Some(&state), now + offset), ActivityDecision::Hold);
        channel.messages.push(format!("chat+{offset}"));
    }

    // Window elapsed: the fresh copy becomes the previous one, replaced again.
    let later = now + 5_000;
    state = channel.apply(&state, human(Some(&state), later), later, true);
    assert_eq!(
        channel.messages,
        ["chat+0", "chat+1", "chat+2500", "chat+4999", "fresh-2"]
    );
    assert_eq!(state.last_message_id.as_deref(), Some("fresh-2"));
}
