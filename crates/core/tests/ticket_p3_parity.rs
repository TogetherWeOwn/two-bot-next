//! Bot P3 parity pins: tickets open/claim/close + transcript-purge (offline).
//!
//! Pins the ticket state machine, panel ensure, recovery routing and
//! transcript-retention math through the public `two_bot_core::tickets` API
//! only. Synthetic fixtures only: no Discord, network, database, staging or
//! handler/store/gateway wiring. Every DB-backed proof (concurrent winners,
//! rollback atomicity, inclusive retention delete, guild fencing) stays in
//! `crates/cutover/tests/tickets_store.rs` on the disposable test database.

use two_bot_core::tickets::{
    authorize, decide_open, format_transcript, is_unknown_channel, panel_needed, recovery_action,
    ticket_channel_name, OpenDecision, PanelMessage, RecoveryAction, Ticket, TicketAction,
    TicketError, TicketStatus, TranscriptMessage, COOLDOWN_SECONDS, INTERRUPTED_AFTER_MS,
    MAX_TRANSCRIPT_UTF16_UNITS, PURGE_INTERVAL_SECONDS, RECOVERY_INTERVAL_SECONDS, TICKET_CLAIM_ID,
    TICKET_CLOSE_ID, TICKET_OPEN_ID, TRANSCRIPT_RETENTION_MS,
};

fn reservation() -> Ticket {
    Ticket {
        id: "reservation".into(),
        guild_id: "guild".into(),
        channel_id: None,
        opener_id: "member".into(),
        claimed_by: None,
        status: TicketStatus::Creating,
        created_at: 0,
        closing_started_at: None,
        closed_at: None,
    }
}

fn open_ticket() -> Ticket {
    let mut ticket = reservation();
    ticket.record_channel("channel").unwrap();
    ticket.activate("channel").unwrap();
    ticket
}

#[test]
fn status_wire_values_roundtrip_and_malformed_is_none() {
    for status in [
        TicketStatus::Creating,
        TicketStatus::Open,
        TicketStatus::Closing,
        TicketStatus::CleanupPending,
        TicketStatus::Closed,
    ] {
        assert_eq!(TicketStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(TicketStatus::Creating.as_str(), "creating");
    assert_eq!(TicketStatus::Open.as_str(), "open");
    assert_eq!(TicketStatus::Closing.as_str(), "closing");
    assert_eq!(TicketStatus::CleanupPending.as_str(), "cleanup_pending");
    assert_eq!(TicketStatus::Closed.as_str(), "closed");
    for malformed in ["", "OPEN", "Closed", "deleted", "closing "] {
        assert_eq!(TicketStatus::parse(malformed), None);
    }
}

#[test]
fn timers_match_legacy_cadence() {
    assert_eq!(COOLDOWN_SECONDS, 300);
    assert_eq!(RECOVERY_INTERVAL_SECONDS, 300);
    assert_eq!(PURGE_INTERVAL_SECONDS, 3600);
    assert_eq!(INTERRUPTED_AFTER_MS, 15 * 60 * 1000);
    assert_eq!(TRANSCRIPT_RETENTION_MS, 90 * 24 * 60 * 60 * 1000);
}

#[test]
fn open_claim_close_happy_path_keeps_claim_in_transcript() {
    let mut ticket = reservation();
    // Close before open is a no-op fence, not a state change.
    assert_eq!(ticket.begin_close(10), Err(TicketError::InvalidTransition));
    assert_eq!(ticket.status, TicketStatus::Creating);

    ticket.record_channel("channel").unwrap();
    // A second channel id for the same reservation is a conflict.
    assert_eq!(
        ticket.record_channel("other"),
        Err(TicketError::InvalidTransition)
    );
    ticket.activate("channel").unwrap();
    assert_eq!(ticket.status, TicketStatus::Open);

    ticket.claim("staff").unwrap();
    assert_eq!(ticket.claimed_by.as_deref(), Some("staff"));
    // Second claim loses: first winner only.
    assert_eq!(ticket.claim("other"), Err(TicketError::AlreadyClaimed));
    assert_eq!(ticket.claimed_by.as_deref(), Some("staff"));

    ticket.begin_close(10).unwrap();
    assert_eq!(ticket.status, TicketStatus::Closing);
    assert_eq!(ticket.closing_started_at, Some(10));

    let transcript = ticket
        .capture_close(10, 20, format_transcript(vec![]))
        .unwrap();
    assert_eq!(transcript.ticket_id, "reservation");
    assert_eq!(transcript.guild_id, "guild");
    assert_eq!(transcript.channel_id, "channel");
    assert_eq!(transcript.opener_id, "member");
    assert_eq!(transcript.claimed_by.as_deref(), Some("staff"));
    assert_eq!(ticket.status, TicketStatus::CleanupPending);

    ticket.finish_cleanup(30).unwrap();
    assert_eq!(ticket.status, TicketStatus::Closed);
    // closed_at keeps the capture instant, not the cleanup instant.
    assert_eq!(ticket.closed_at, Some(20));
    // Terminal: no further transitions.
    assert_eq!(ticket.begin_close(40), Err(TicketError::InvalidTransition));
    assert_eq!(
        ticket.finish_cleanup(40),
        Err(TicketError::InvalidTransition)
    );
}

#[test]
fn open_decision_prefers_active_and_enforces_300s_cooldown_boundary() {
    assert_eq!(
        decide_open(None, None, 0, COOLDOWN_SECONDS),
        OpenDecision::Reserve
    );
    assert_eq!(
        decide_open(None, Some(0), 299_999, COOLDOWN_SECONDS),
        OpenDecision::Cooldown
    );
    assert_eq!(
        decide_open(None, Some(0), 300_000, COOLDOWN_SECONDS),
        OpenDecision::Reserve
    );
    let active = reservation();
    assert_eq!(
        decide_open(Some(&active), Some(0), 1, COOLDOWN_SECONDS),
        OpenDecision::Existing { channel_id: None }
    );
    let with_channel = open_ticket();
    assert_eq!(
        decide_open(Some(&with_channel), Some(0), i64::MAX, COOLDOWN_SECONDS),
        OpenDecision::Existing {
            channel_id: Some("channel".into())
        }
    );
}

#[test]
fn claim_and_close_reject_wrong_state_and_empty_staff() {
    let mut ticket = reservation();
    // Creating tickets cannot be claimed.
    assert_eq!(ticket.claim("staff"), Err(TicketError::AlreadyClaimed));
    let mut open = open_ticket();
    assert_eq!(open.claim(""), Err(TicketError::AlreadyClaimed));
    // Close needs a recorded channel.
    let mut no_channel = reservation();
    no_channel.status = TicketStatus::Open;
    assert_eq!(no_channel.begin_close(5), Err(TicketError::MissingChannel));
    // Closing twice is invalid; cleanup before capture is invalid.
    open.begin_close(10).unwrap();
    assert_eq!(open.begin_close(11), Err(TicketError::InvalidTransition));
    assert_eq!(open.finish_cleanup(12), Err(TicketError::InvalidTransition));
    // Capture with a stale token loses to the current close.
    assert_eq!(
        open.capture_close(11, 20, format_transcript(vec![])),
        Err(TicketError::StaleClose)
    );
    assert_eq!(open.status, TicketStatus::Closing);
}

#[test]
fn open_rollback_recovers_creation_and_control_failures_without_transcript() {
    // Failed channel creation with a recorded channel stays recoverable.
    let mut creating = reservation();
    creating.record_channel("channel").unwrap();
    creating.queue_open_rollback(7).unwrap();
    assert_eq!(creating.status, TicketStatus::CleanupPending);
    assert_eq!(creating.closed_at, Some(7));
    creating.finish_cleanup(9).unwrap();
    assert_eq!(creating.status, TicketStatus::Closed);
    assert_eq!(creating.closed_at, Some(7));

    // Failed control posting on an open ticket rolls back the same way.
    let mut open = open_ticket();
    open.queue_open_rollback(11).unwrap();
    assert_eq!(open.status, TicketStatus::CleanupPending);
    open.finish_cleanup(13).unwrap();
    assert_eq!(open.status, TicketStatus::Closed);

    // Rollback needs a recorded channel: nothing to delete otherwise.
    let mut bare = reservation();
    assert_eq!(
        bare.queue_open_rollback(3),
        Err(TicketError::MissingChannel)
    );
    // Rollback is only for pre-close work, never for closing/cleanup/closed.
    let mut closing = open_ticket();
    closing.begin_close(10).unwrap();
    assert_eq!(
        closing.queue_open_rollback(11),
        Err(TicketError::InvalidTransition)
    );
}

#[test]
fn button_ids_route_and_guild_staff_gates_hold() {
    assert_eq!(
        TicketAction::from_custom_id(TICKET_OPEN_ID),
        Some(TicketAction::Open)
    );
    assert_eq!(
        TicketAction::from_custom_id(TICKET_CLAIM_ID),
        Some(TicketAction::Claim)
    );
    assert_eq!(
        TicketAction::from_custom_id(TICKET_CLOSE_ID),
        Some(TicketAction::Close)
    );
    assert_eq!(TicketAction::from_custom_id("two:tickets:bogus"), None);

    // Open is member-wide inside the configured guild; claim/close are staff.
    assert!(authorize(Some("guild"), "guild", TicketAction::Open, &[], 0, "staff").is_ok());
    for action in [TicketAction::Claim, TicketAction::Close] {
        assert_eq!(
            authorize(Some("guild"), "guild", action, &[], 0, "staff"),
            Err(TicketError::StaffOnly)
        );
        assert!(authorize(
            Some("guild"),
            "guild",
            action,
            &["staff".into()],
            0,
            "staff"
        )
        .is_ok());
        // Administrator bit implies staff even without the role.
        assert!(authorize(Some("guild"), "guild", action, &[], 1 << 3, "staff").is_ok());
    }
    // Wrong or missing guild never reaches role checks.
    for action in [TicketAction::Open, TicketAction::Claim, TicketAction::Close] {
        assert_eq!(
            authorize(None, "guild", action, &["staff".into()], u64::MAX, "staff"),
            Err(TicketError::WrongGuild)
        );
        assert_eq!(
            authorize(
                Some("other"),
                "guild",
                action,
                &["staff".into()],
                u64::MAX,
                "staff"
            ),
            Err(TicketError::WrongGuild)
        );
    }
}

#[test]
fn panel_ensure_trusts_only_this_bot_open_button() {
    // Empty history needs a panel.
    assert!(panel_needed("bot", &[]));
    // Another author's open button is not ours.
    assert!(panel_needed(
        "bot",
        &[PanelMessage {
            author_id: "other".into(),
            custom_ids: vec![TICKET_OPEN_ID.into()],
        }]
    ));
    // Our own message without the open button is not a panel.
    assert!(panel_needed(
        "bot",
        &[PanelMessage {
            author_id: "bot".into(),
            custom_ids: vec![TICKET_CLOSE_ID.into()],
        }]
    ));
    // Our open panel suppresses a repost even beside other traffic.
    assert!(!panel_needed(
        "bot",
        &[
            PanelMessage {
                author_id: "other".into(),
                custom_ids: vec![TICKET_OPEN_ID.into()],
            },
            PanelMessage {
                author_id: "bot".into(),
                custom_ids: vec![TICKET_OPEN_ID.into()],
            },
        ]
    ));
}

#[test]
fn channel_names_match_legacy_and_only_10003_is_absence() {
    assert_eq!(
        ticket_channel_name("A very unsafe Username!!!"),
        "ticket-a-very-unsafe-username"
    );
    assert_eq!(ticket_channel_name("!!!"), "ticket-member");
    assert_eq!(ticket_channel_name(""), "ticket-member");
    assert_eq!(
        ticket_channel_name(&"x".repeat(40)).len(),
        "ticket-".len() + 24
    );
    assert!(is_unknown_channel(Some(10003)));
    assert!(!is_unknown_channel(Some(50013)));
    assert!(!is_unknown_channel(Some(10003 + 1)));
    assert!(!is_unknown_channel(None));
}

#[test]
fn recovery_leaves_fresh_live_and_channel_less_work_alone() {
    // Fresh creating reservation: not yet interrupted.
    assert_eq!(
        recovery_action(&reservation(), INTERRUPTED_AFTER_MS - 1, false),
        RecoveryAction::None
    );
    // Open ticket without a channel has no controls to reattach.
    let mut open_no_channel = reservation();
    open_no_channel.status = TicketStatus::Open;
    assert_eq!(
        recovery_action(&open_no_channel, INTERRUPTED_AFTER_MS, false),
        RecoveryAction::None
    );
    // Closing without a token is malformed: no recovery call.
    let mut token_less = open_ticket();
    token_less.status = TicketStatus::Closing;
    assert_eq!(
        recovery_action(&token_less, INTERRUPTED_AFTER_MS * 2, false),
        RecoveryAction::None
    );
    // Fresh close is still in flight.
    let mut closing = open_ticket();
    closing.begin_close(100).unwrap();
    assert_eq!(
        recovery_action(&closing, 100 + INTERRUPTED_AFTER_MS - 1, false),
        RecoveryAction::None
    );
    // Cleanup without a channel has nothing to retry.
    let mut cleanup_no_channel = reservation();
    cleanup_no_channel.status = TicketStatus::CleanupPending;
    assert_eq!(
        recovery_action(&cleanup_no_channel, INTERRUPTED_AFTER_MS * 2, true),
        RecoveryAction::None
    );
    // Closed is terminal: never recovered.
    let mut closed = open_ticket();
    closing_reopen_cycle(&mut closed);
    assert_eq!(
        recovery_action(&closed, i64::MAX, true),
        RecoveryAction::None
    );
}

/// Drives a ticket to `Closed` through capture + cleanup for terminal tests.
fn closing_reopen_cycle(ticket: &mut Ticket) {
    ticket.begin_close(10).unwrap();
    ticket
        .capture_close(10, 20, format_transcript(vec![]))
        .unwrap();
    ticket.finish_cleanup(30).unwrap();
    assert_eq!(ticket.status, TicketStatus::Closed);
}

#[test]
fn recovery_recovers_only_stale_interrupted_work() {
    // Stale creating without a channel: search by topic.
    assert_eq!(
        recovery_action(&reservation(), INTERRUPTED_AFTER_MS, false),
        RecoveryAction::FindCreatingChannel {
            topic: "two-ticket:reservation".into()
        }
    );
    // Stale creating with a channel: delete the orphan.
    let mut orphan = reservation();
    orphan.record_channel("channel").unwrap();
    assert_eq!(
        recovery_action(&orphan, INTERRUPTED_AFTER_MS, false),
        RecoveryAction::DeleteInterruptedCreate {
            channel_id: "channel".into()
        }
    );
    // Live open ticket: reattach controls.
    let open = open_ticket();
    assert_eq!(
        recovery_action(&open, 0, false),
        RecoveryAction::ReattachOpenControls {
            channel_id: "channel".into()
        }
    );
    // Stale closing splits on transcript evidence: reopen vs saved-close.
    let mut closing = open_ticket();
    closing.begin_close(10).unwrap();
    assert_eq!(
        recovery_action(&closing, 10 + INTERRUPTED_AFTER_MS, false),
        RecoveryAction::RestoreOpenerThenReopen {
            channel_id: "channel".into(),
            started_at: 10
        }
    );
    assert_eq!(
        recovery_action(&closing, 10 + INTERRUPTED_AFTER_MS, true),
        RecoveryAction::RecoverSavedClose { started_at: 10 }
    );
    // Cleanup pending retries the delete.
    let mut cleanup = open_ticket();
    cleanup.begin_close(10).unwrap();
    cleanup
        .capture_close(10, 20, format_transcript(vec![]))
        .unwrap();
    assert_eq!(
        recovery_action(&cleanup, 40, true),
        RecoveryAction::RetryCleanup {
            channel_id: "channel".into()
        }
    );
}

#[test]
fn reopen_fences_late_transcripts_and_saved_close_never_reopens() {
    let mut ticket = open_ticket();
    ticket.begin_close(10).unwrap();
    // Wrong token cannot reopen.
    assert_eq!(
        ticket.reopen_interrupted(11, false),
        Err(TicketError::StaleClose)
    );
    // A saved transcript blocks reopen: recover instead.
    assert_eq!(
        ticket.reopen_interrupted(10, true),
        Err(TicketError::InvalidTransition)
    );
    ticket.reopen_interrupted(10, false).unwrap();
    assert_eq!(ticket.status, TicketStatus::Open);
    assert_eq!(ticket.closing_started_at, None);
    // The old token is dead after reopen.
    assert_eq!(
        ticket.capture_close(10, 30, format_transcript(vec![])),
        Err(TicketError::StaleClose)
    );
    // A new close with a saved body recovers without reopening.
    ticket.begin_close(20).unwrap();
    ticket.recover_saved_close(20, 30).unwrap();
    assert_eq!(ticket.status, TicketStatus::CleanupPending);
    assert_eq!(ticket.closed_at, Some(30));
}

#[test]
fn reconciled_close_confirms_purged_body_and_never_reopens() {
    // Purge reconciles a legacy crash row to cleanup while deleting the body;
    // the row itself is the capture marker.
    let mut ticket = open_ticket();
    ticket.begin_close(10).unwrap();
    ticket.recover_saved_close(10, 20).unwrap();
    ticket.confirm_reconciled_close(10).unwrap();
    // A purged body can never reopen the captured close.
    assert_eq!(
        ticket.reopen_interrupted(10, false),
        Err(TicketError::StaleClose)
    );
    // Wrong token or wrong state never confirms.
    let mut other = open_ticket();
    other.begin_close(10).unwrap();
    assert_eq!(
        other.confirm_reconciled_close(11),
        Err(TicketError::StaleClose)
    );
    assert_eq!(
        other.confirm_reconciled_close(10),
        Err(TicketError::StaleClose)
    );
    // An unsaved close can complete without a transcript once the channel is
    // proven gone, but a captured close can never take that path.
    let mut unsaved = open_ticket();
    unsaved.begin_close(10).unwrap();
    assert_eq!(
        unsaved.abandon_unsaved_close(11, 20),
        Err(TicketError::StaleClose)
    );
    unsaved.abandon_unsaved_close(10, 20).unwrap();
    assert_eq!(unsaved.status, TicketStatus::Closed);
    let mut saved = open_ticket();
    saved.begin_close(10).unwrap();
    saved
        .capture_close(10, 30, format_transcript(vec![]))
        .unwrap();
    assert_eq!(
        saved.abandon_unsaved_close(10, 40),
        Err(TicketError::StaleClose)
    );
}

#[test]
fn purge_after_is_capture_plus_90_days_and_expiry_is_inclusive() {
    let mut ticket = open_ticket();
    ticket.begin_close(10).unwrap();
    let transcript = ticket
        .capture_close(10, 1_000, format_transcript(vec![]))
        .unwrap();
    assert_eq!(transcript.purge_after, 1_000 + TRANSCRIPT_RETENTION_MS);
    // Retention is a privacy ceiling fixed at capture: cleanup does not move it.
    ticket.finish_cleanup(2_000).unwrap();
    assert_eq!(transcript.purge_after, 1_000 + TRANSCRIPT_RETENTION_MS);
    // Store expiry is `purge_after <= now` (inclusive): the exact instant the
    // deadline passes, the body is already expired.
    let purge_after = transcript.purge_after;
    let expired = |now: i64| purge_after <= now;
    assert!(expired(purge_after));
    assert!(expired(purge_after + 1));
    assert!(!expired(purge_after - 1));
}

#[test]
fn transcript_orders_chronologically_and_keeps_attachments_and_count() {
    let snapshot = format_transcript(vec![
        TranscriptMessage {
            created_at: 1000,
            author_tag: "later".into(),
            content: "world".into(),
            attachment_urls: vec!["https://example.test/file".into()],
        },
        TranscriptMessage {
            created_at: 0,
            author_tag: "first".into(),
            content: "hello".into(),
            attachment_urls: vec![],
        },
    ]);
    assert_eq!(snapshot.message_count, 2);
    assert_eq!(
        snapshot.content,
        "[1970-01-01T00:00:00.000Z] first: hello\n\
         [1970-01-01T00:00:01.000Z] later: world https://example.test/file"
    );
    assert_eq!(format_transcript(vec![]).message_count, 0);
}

#[test]
fn transcript_truncation_is_unicode_safe_and_keeps_total_count() {
    let snapshot = format_transcript(vec![TranscriptMessage {
        created_at: 0,
        author_tag: "a".into(),
        content: "😀".repeat(110_000),
        attachment_urls: vec![],
    }]);
    assert_eq!(snapshot.message_count, 1);
    assert!(snapshot.content.ends_with("\n[transcript truncated]"));
    let retained = snapshot
        .content
        .strip_suffix("\n[transcript truncated]")
        .unwrap();
    assert!(retained.encode_utf16().count() <= MAX_TRANSCRIPT_UTF16_UNITS);
}
