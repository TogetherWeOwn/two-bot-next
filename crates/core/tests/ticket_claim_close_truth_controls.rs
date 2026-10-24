//! Protected acceptance controls for truthful ticket claim/close replies.
//! These exercise the public domain API, without Discord or a database.
use two_bot_core::tickets::{
    authorize, decide_open, format_transcript, recovery_action, OpenDecision, RecoveryAction,
    Ticket, TicketAction, TicketError, TicketStatus, COOLDOWN_SECONDS, INTERRUPTED_AFTER_MS,
};

fn open_ticket() -> Ticket {
    Ticket {
        id: "ticket".into(),
        guild_id: "100".into(),
        channel_id: Some("600".into()),
        opener_id: "500".into(),
        claimed_by: None,
        status: TicketStatus::Open,
        created_at: 0,
        closing_started_at: None,
        closed_at: None,
    }
}

#[test]
fn truthful_copy_does_not_grant_members_or_foreign_staff_new_powers() {
    for action in [TicketAction::Claim, TicketAction::Close] {
        assert_eq!(
            authorize(Some("100"), "100", action, &[], 0, "300"),
            Err(TicketError::StaffOnly)
        );
        for (roles, permissions) in [(vec!["300".into()], 0), (vec![], 1 << 4), (vec![], 1 << 3)] {
            assert_eq!(
                authorize(Some("100"), "100", action, &roles, permissions, "300"),
                Ok(())
            );
            assert_eq!(
                authorize(Some("999"), "100", action, &roles, permissions, "300"),
                Err(TicketError::WrongGuild)
            );
        }
    }
}

#[test]
fn claim_does_not_reset_cooldown_or_allow_a_second_active_ticket() {
    let mut ticket = open_ticket();
    ticket.claim("501").unwrap();
    assert_eq!(ticket.created_at, 0);
    assert!(matches!(
        decide_open(Some(&ticket), Some(0), 300_000, COOLDOWN_SECONDS),
        OpenDecision::Existing { channel_id: Some(channel), .. } if channel == "600"
    ));
    assert!(matches!(
        decide_open(None, Some(0), 299_999, COOLDOWN_SECONDS),
        OpenDecision::Cooldown { .. }
    ));
    assert_eq!(
        decide_open(None, Some(0), 300_000, COOLDOWN_SECONDS),
        OpenDecision::Reserve
    );
}

#[test]
fn claim_survives_unsaved_close_recovery_and_stale_capture_stays_fenced() {
    let mut ticket = open_ticket();
    ticket.claim("501").unwrap();
    ticket.begin_close(10).unwrap();
    assert_eq!(
        recovery_action(&ticket, 10 + INTERRUPTED_AFTER_MS, false),
        RecoveryAction::RestoreOpenerThenReopen {
            channel_id: "600".into(),
            started_at: 10
        }
    );
    ticket.reopen_interrupted(10, false).unwrap();
    assert_eq!(ticket.status, TicketStatus::Open);
    assert_eq!(ticket.claimed_by.as_deref(), Some("501"));
    assert_eq!(
        ticket.capture_close(10, 30, format_transcript(vec![])),
        Err(TicketError::StaleClose)
    );
}

#[test]
fn saved_close_preserves_claimant_and_ninety_day_ceiling_through_cleanup() {
    let mut ticket = open_ticket();
    ticket.claim("501").unwrap();
    ticket.begin_close(10).unwrap();
    let transcript = ticket
        .capture_close(10, 20, format_transcript(vec![]))
        .unwrap();
    assert_eq!(transcript.claimed_by.as_deref(), Some("501"));
    assert_eq!(transcript.purge_after, 7_776_000_020);
    assert_eq!(
        recovery_action(&ticket, 30, true),
        RecoveryAction::RetryCleanup {
            channel_id: "600".into()
        }
    );
    assert!(ticket.reopen_interrupted(10, true).is_err());
    ticket.finish_cleanup(30).unwrap();
    assert_eq!(ticket.status, TicketStatus::Closed);
    assert_eq!(ticket.claimed_by.as_deref(), Some("501"));
    assert_eq!(recovery_action(&ticket, 40, true), RecoveryAction::None);
}
