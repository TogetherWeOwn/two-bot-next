//! REST executor pacing/backoff/kick-status acceptance (TOG-12698).
//!
//! Pure, offline: pins the public `two_bot_core::action_outcomes` API only —
//! no database, no Discord client, no feature flags. Parity: `docs/parity.md`
//! §6 Discord REST row (110 ms pacing, 429 `retry-after + 250 ms`, 5xx
//! exponential backoff) and the kick retry budget; legacy `rest.ts` /
//! `kick.ts`. The retry loops and lane constants themselves live in the
//! executor (`two-bot-discord`, `PACE_INTERVAL_MS`/`KICK_INTERVAL_MS`); this
//! suite pins the pure helpers the executor calls. See
//! `docs/pacing-backoff-acceptance.md`.

use two_bot_core::action_outcomes::{
    backoff_ms, classify_kick_status, clear_send_bit, lockdown_overwrite, pace_wait_ms,
    parse_retry_after_secs, retry_after_ms, set_send_bit, unlock_overwrite, KickStatus,
    BACKOFF_BASE_MS, MAX_HTTP_TRIES, MAX_RETRY_AFTER_MS, RETRY_AFTER_PADDING_MS, SEND_MESSAGES_BIT,
};

// ---------------------------------------------------------------------------
// (1) pace_wait_ms: 0 once the interval has elapsed, exact remainder otherwise
// ---------------------------------------------------------------------------

#[test]
fn pace_wait_is_zero_once_the_interval_has_elapsed() {
    // Exact boundary: the floor already passed, so no wait.
    assert_eq!(pace_wait_ms(1_000, 110, 1_110), 0);
    assert_eq!(pace_wait_ms(1_000, 110, 1_200), 0);
    assert_eq!(pace_wait_ms(1_000, 350, 1_350), 0);
    assert_eq!(pace_wait_ms(1_000, 110, u64::MAX), 0);
    assert_eq!(pace_wait_ms(0, 0, 0), 0);
}

#[test]
fn pace_wait_returns_the_exact_remaining_ms() {
    assert_eq!(pace_wait_ms(1_000, 110, 1_000), 110);
    assert_eq!(pace_wait_ms(1_000, 110, 1_050), 60);
    // Kick lane (350 ms) uses the same helper with its own interval.
    assert_eq!(pace_wait_ms(1_000, 350, 1_100), 250);
    assert_eq!(pace_wait_ms(1_000, 350, 1_349), 1);
}

#[test]
fn pace_wait_never_wraps_at_the_u64_edges() {
    // u64 return: "never negative" holds by type; saturating arithmetic means
    // no wrap to a small wait that would fire early.
    assert_eq!(pace_wait_ms(u64::MAX, 110, u64::MAX), 0);
    assert_eq!(pace_wait_ms(u64::MAX, 110, 0), u64::MAX);
    assert_eq!(pace_wait_ms(0, u64::MAX, 0), u64::MAX);
    assert_eq!(pace_wait_ms(0, u64::MAX, u64::MAX), 0);
}

// ---------------------------------------------------------------------------
// (2) backoff_ms grows exponentially; retry_after_ms pads and clamps
// ---------------------------------------------------------------------------

#[test]
fn backoff_grows_exponentially_within_the_retry_budget() {
    assert_eq!(BACKOFF_BASE_MS, 500);
    assert_eq!(MAX_HTTP_TRIES, 5);
    let waits: Vec<u64> = (0..MAX_HTTP_TRIES).map(backoff_ms).collect();
    assert_eq!(waits, vec![500, 1_000, 2_000, 4_000, 8_000]);
    for pair in waits.windows(2) {
        assert_eq!(pair[1], pair[0] * 2, "attempt backoff must double");
    }
}

#[test]
fn backoff_saturates_instead_of_overflowing() {
    assert_eq!(backoff_ms(20), BACKOFF_BASE_MS * (1u64 << 20));
    assert_eq!(backoff_ms(u32::MAX), BACKOFF_BASE_MS * (1u64 << 20));
}

#[test]
fn retry_after_adds_250ms_padding_to_header_seconds() {
    assert_eq!(RETRY_AFTER_PADDING_MS, 250);
    assert_eq!(retry_after_ms(Some(1.0), None), 1_250);
    assert_eq!(retry_after_ms(Some(2.0), None), 2_250);
    assert_eq!(retry_after_ms(Some(0.5), None), 750);
}

#[test]
fn retry_after_body_wins_over_header() {
    assert_eq!(retry_after_ms(Some(5.0), Some(1.5)), 1_750);
    // Fractional seconds ceil, then padding.
    assert_eq!(retry_after_ms(Some(1.0), Some(6.457)), 6_707);
    // Missing header defaults to the legacy 1 s.
    assert_eq!(retry_after_ms(None, None), 1_250);
    // Non-finite body keeps the header.
    assert_eq!(retry_after_ms(Some(2.0), Some(f64::INFINITY)), 2_250);
}

#[test]
fn retry_after_clamps_at_sixty_seconds() {
    assert_eq!(MAX_RETRY_AFTER_MS, 60_000);
    assert_eq!(retry_after_ms(Some(59.5), None), 59_750);
    assert_eq!(retry_after_ms(Some(59.75), None), 60_000);
    assert_eq!(retry_after_ms(Some(60.0), None), 60_000);
    assert_eq!(retry_after_ms(Some(120.0), None), 60_000);
    // A malformed retry-after of a day must not park the run.
    assert_eq!(retry_after_ms(Some(86_400.0), None), 60_000);
}

#[test]
fn retry_after_refuses_negative_and_non_finite_values() {
    assert_eq!(retry_after_ms(Some(-3.0), None), 1_250);
    assert_eq!(retry_after_ms(Some(f64::NAN), None), 1_250);
    assert_eq!(retry_after_ms(Some(f64::INFINITY), None), 1_250);
    // A negative body does not override a good header with a negative wait.
    assert_eq!(retry_after_ms(Some(5.0), Some(-2.0)), 1_250);
}

// ---------------------------------------------------------------------------
// (3) classify_kick_status maps statuses to the documented variants
// ---------------------------------------------------------------------------

#[test]
fn classify_kick_status_maps_documented_statuses() {
    // Success arm is exactly 200/204 (documented on KickStatus::Removed).
    assert_eq!(classify_kick_status(200), KickStatus::Removed);
    assert_eq!(classify_kick_status(204), KickStatus::Removed);
    assert_eq!(classify_kick_status(404), KickStatus::AlreadyGone);
    assert_eq!(classify_kick_status(403), KickStatus::Forbidden);
    assert_eq!(classify_kick_status(401), KickStatus::Unauthorized);
    assert_eq!(classify_kick_status(429), KickStatus::RateLimited);
    for status in [500, 502, 503, 599] {
        assert_eq!(
            classify_kick_status(status),
            KickStatus::ServerError,
            "{status}"
        );
    }
}

#[test]
fn classify_kick_status_leaves_everything_else_other() {
    // The documented arms are tight: other 2xx/4xx codes are never retried.
    for status in [100, 201, 400, 418, 600] {
        assert_eq!(classify_kick_status(status), KickStatus::Other, "{status}");
    }
}

// ---------------------------------------------------------------------------
// (4) send-bit and lockdown helpers preserve unrelated bits
// ---------------------------------------------------------------------------

#[test]
fn send_bit_helpers_preserve_unrelated_bits() {
    assert_eq!(SEND_MESSAGES_BIT, 1 << 11);
    assert_eq!(SEND_MESSAGES_BIT, 2048);
    for mask in [0u64, 1, 8, 64, 1024, (1 << 11) | 1, u64::MAX] {
        assert_eq!(
            set_send_bit(mask) & !SEND_MESSAGES_BIT,
            mask & !SEND_MESSAGES_BIT,
            "set must preserve other bits of {mask:#x}"
        );
        assert_eq!(
            clear_send_bit(mask) | SEND_MESSAGES_BIT,
            mask | SEND_MESSAGES_BIT,
            "clear must preserve other bits of {mask:#x}"
        );
    }
    assert_eq!(set_send_bit(0), 2048);
    assert_eq!(set_send_bit(2048), 2048);
    assert_eq!(clear_send_bit(2048 | 8), 8);
    assert_eq!(clear_send_bit(8), 8);
    assert_eq!(clear_send_bit(u64::MAX), !2048);
}

#[test]
fn lockdown_overwrite_denies_send_and_preserves_other_bits() {
    assert_eq!(lockdown_overwrite(0, 0), (0, 2048));
    assert_eq!(lockdown_overwrite(2048 | 1024, 64), (1024, 64 | 2048));
    let (allow, deny) = lockdown_overwrite(u64::MAX, 0x00FF);
    assert_eq!(allow, !SEND_MESSAGES_BIT);
    assert_eq!(deny, 0x00FF | SEND_MESSAGES_BIT);
}

#[test]
fn unlock_overwrite_clears_only_the_send_bit() {
    assert_eq!(unlock_overwrite(2048 | 1024, 2048 | 64), (1024, 64));
    assert_eq!(unlock_overwrite(1024, 64), (1024, 64));
    assert_eq!(unlock_overwrite(u64::MAX, u64::MAX), (!2048, !2048));
}

#[test]
fn unlock_undoes_lockdown_deny_while_allow_stays_cleared() {
    // Legacy unlock-without-a-record: the lockdown's deny bit is dropped and
    // allow is left as lockdown wrote it (bit cleared).
    let (locked_allow, locked_deny) = lockdown_overwrite(2048 | 1024, 64);
    assert_eq!(unlock_overwrite(locked_allow, locked_deny), (1024, 64));
}

// ---------------------------------------------------------------------------
// (5) parse_retry_after_secs refuses garbage; negatives fall back downstream
// ---------------------------------------------------------------------------

#[test]
fn parse_retry_after_secs_refuses_garbage() {
    assert_eq!(parse_retry_after_secs(None), None);
    assert_eq!(parse_retry_after_secs(Some("")), None);
    assert_eq!(parse_retry_after_secs(Some("   ")), None);
    assert_eq!(parse_retry_after_secs(Some("soon")), None);
    assert_eq!(parse_retry_after_secs(Some("1s")), None);
    assert_eq!(parse_retry_after_secs(Some("retry-after: 2")), None);
    // Valid numerics still parse (surrounding whitespace tolerated).
    assert_eq!(parse_retry_after_secs(Some("1")), Some(1.0));
    assert_eq!(parse_retry_after_secs(Some(" 6.457 ")), Some(6.457));
    assert_eq!(parse_retry_after_secs(Some("0")), Some(0.0));
}

#[test]
fn negative_retry_after_values_fall_back_to_one_second_plus_padding() {
    // The parser is total over numeric text, negatives included; the refusal
    // lives in retry_after_ms, which falls back to the legacy 1 s + 250 ms.
    for raw in ["-1", "-0.5", "-120"] {
        let parsed = parse_retry_after_secs(Some(raw));
        assert!(matches!(parsed, Some(v) if v < 0.0), "{raw}");
        assert_eq!(
            retry_after_ms(parsed, None),
            1_000 + RETRY_AFTER_PADDING_MS,
            "{raw}"
        );
    }
}
