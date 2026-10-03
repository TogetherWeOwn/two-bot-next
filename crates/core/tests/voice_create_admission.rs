//! Hermetic acceptance cases for the pure room-create admission core.
//!
//! Every case pins legacy behavior from `TogetherWeOwn/two-bot@bffccf3e`
//! (`tempVoice/store.ts`, `tempVoice/service.ts`, `tempVoice/config.ts`):
//! the refusal order, the strict 60 s window, cooldown-0 disabling, burst
//! counting accepted reservations (never live rooms or the attempt itself),
//! the config bounds, and the stable codes plus legacy user-facing messages.

use two_bot_core::voice_create_admission::{
    decide_admission, AcceptedReservation, AdmissionConfigError, AdmissionDecision, AdmissionError,
    AdmissionRequest, CreateAdmissionConfig, RefusalReason, CREATE_BURST_PER_GUILD,
    CREATE_BURST_PER_USER, CREATE_BURST_WINDOW_SECS, MAX_CREATE_COOLDOWN_SECS, MAX_ROOMS_PER_GUILD,
    MAX_ROOMS_PER_USER,
};

const NOW: i64 = 1_700_000_000;
const USER: u64 = 7;
const OTHER: u64 = 9;

fn config() -> CreateAdmissionConfig {
    CreateAdmissionConfig::new(2, 4, 30).expect("valid fixture config")
}

fn request<'a>(
    owned_by_user: u32,
    rooms_in_guild: u32,
    last_created_at_secs: Option<i64>,
    accepted_reservations: &'a [AcceptedReservation],
) -> AdmissionRequest<'a> {
    AdmissionRequest {
        user_id: USER,
        owned_by_user,
        rooms_in_guild,
        last_created_at_secs,
        accepted_reservations,
    }
}

fn reservation(user_id: u64, age_secs: i64) -> AcceptedReservation {
    AcceptedReservation {
        user_id,
        created_at_secs: NOW - age_secs,
    }
}

#[test]
fn allows_a_fresh_first_create() {
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &[]), NOW),
        Ok(AdmissionDecision::Allow)
    );
}

#[test]
fn refusal_order_is_user_cap_guild_cap_cooldown_user_burst_guild_burst() {
    // Every gate tripped at once: the user cap wins.
    let history = vec![reservation(USER, 10); CREATE_BURST_PER_USER as usize];
    let history = fill_guild(history);
    assert_eq!(
        decide_admission(&config(), &request(2, 4, Some(NOW - 1), &history), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserCap
        })
    );
    // Guild cap beats the later gates.
    assert_eq!(
        decide_admission(&config(), &request(0, 4, Some(NOW - 1), &history), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildCap
        })
    );
    // Cooldown beats both burst gates.
    assert_eq!(
        decide_admission(&config(), &request(0, 0, Some(NOW - 1), &history), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::Cooldown
        })
    );
    // User burst beats guild burst.
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &history), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserBurst
        })
    );
    // Only the guild burst tripped.
    let guild_only = fill_guild(vec![reservation(USER, 10)]);
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &guild_only), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildBurst
        })
    );
}

/// Extend a per-user-burst history to exactly the guild burst limit with
/// reservations from another member.
fn fill_guild(mut history: Vec<AcceptedReservation>) -> Vec<AcceptedReservation> {
    while history.len() < CREATE_BURST_PER_GUILD as usize {
        history.push(reservation(OTHER, 10));
    }
    history
}

#[test]
fn caps_count_reservations_not_just_live_rooms() {
    // At the caps minus one the attempt is still allowed ...
    assert_eq!(
        decide_admission(&config(), &request(1, 3, None, &[]), NOW),
        Ok(AdmissionDecision::Allow)
    );
    // ... and equality refuses, since the cap is a live-plus-reserved count.
    assert_eq!(
        decide_admission(&config(), &request(2, 0, None, &[]), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserCap
        })
    );
    assert_eq!(
        decide_admission(&config(), &request(0, 4, None, &[]), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildCap
        })
    );
}

#[test]
fn cooldown_boundary_is_exact_and_zero_disables() {
    // Waiting exactly the cooldown passes; one second less refuses.
    assert_eq!(
        decide_admission(&config(), &request(0, 0, Some(NOW - 30), &[]), NOW),
        Ok(AdmissionDecision::Allow)
    );
    assert_eq!(
        decide_admission(&config(), &request(0, 0, Some(NOW - 29), &[]), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::Cooldown
        })
    );
    // A member with no previous create is never throttled.
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &[]), NOW),
        Ok(AdmissionDecision::Allow)
    );
    // Cooldown 0 disables the check even for a create one second ago.
    let no_cooldown = CreateAdmissionConfig::new(2, 4, 0).expect("cooldown 0 is valid");
    assert_eq!(
        decide_admission(&no_cooldown, &request(0, 0, Some(NOW - 1), &[]), NOW),
        Ok(AdmissionDecision::Allow)
    );
}

#[test]
fn burst_window_is_strict_and_counts_accepted_reservations() {
    // A reservation exactly on the window edge has already left it.
    let edge = vec![reservation(USER, CREATE_BURST_WINDOW_SECS)];
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &edge), NOW),
        Ok(AdmissionDecision::Allow)
    );
    // One second inside still counts. Two such reservations stay under the
    // per-user limit of three; three trip it.
    let two_inside = vec![reservation(USER, CREATE_BURST_WINDOW_SECS - 1); 2];
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &two_inside), NOW),
        Ok(AdmissionDecision::Allow)
    );
    let three_inside = vec![reservation(USER, CREATE_BURST_WINDOW_SECS - 1); 3];
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &three_inside), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserBurst
        })
    );
    // Deleted rooms free no burst slot: a reservation whose room is gone
    // (not reflected in the live counts) still consumes its window slot.
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &three_inside), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserBurst
        })
    );
    // The attempt itself is never counted: one slot short of the limit plus
    // this attempt stays allowed.
    let almost = vec![reservation(OTHER, 5); (CREATE_BURST_PER_GUILD - 1) as usize];
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &almost), NOW),
        Ok(AdmissionDecision::Allow)
    );
    // Future-stamped reservations count: they are inside the window, and the
    // caller — not the core — owns clock sanity.
    let mut future = vec![reservation(USER, 5); (CREATE_BURST_PER_USER - 1) as usize];
    future.push(AcceptedReservation {
        user_id: USER,
        created_at_secs: NOW + 60,
    });
    assert_eq!(
        decide_admission(&config(), &request(0, 0, None, &future), NOW),
        Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserBurst
        })
    );
}

#[test]
fn config_bounds_match_legacy_ranges() {
    assert_eq!(
        CreateAdmissionConfig::new(0, 4, 30),
        Err(AdmissionConfigError::MaxPerUser)
    );
    assert_eq!(
        CreateAdmissionConfig::new(MAX_ROOMS_PER_USER + 1, 4, 30),
        Err(AdmissionConfigError::MaxPerUser)
    );
    assert_eq!(
        CreateAdmissionConfig::new(2, 0, 30),
        Err(AdmissionConfigError::MaxPerGuild)
    );
    assert_eq!(
        CreateAdmissionConfig::new(2, MAX_ROOMS_PER_GUILD + 1, 30),
        Err(AdmissionConfigError::MaxPerGuild)
    );
    assert_eq!(
        CreateAdmissionConfig::new(2, 4, MAX_CREATE_COOLDOWN_SECS + 1),
        Err(AdmissionConfigError::Cooldown)
    );
    // Every inclusive edge of the legacy ranges is accepted.
    CreateAdmissionConfig::new(1, 1, 0).expect("lower edges valid");
    CreateAdmissionConfig::new(
        MAX_ROOMS_PER_USER,
        MAX_ROOMS_PER_GUILD,
        MAX_CREATE_COOLDOWN_SECS,
    )
    .expect("upper edges valid");
    assert_eq!(
        CreateAdmissionConfig::default(),
        CreateAdmissionConfig::new(1, 40, 30).expect("legacy defaults valid")
    );
}

#[test]
fn stable_codes_and_legacy_messages_are_pinned() {
    let singular = CreateAdmissionConfig::new(1, 40, 30).expect("valid config");
    let plural = CreateAdmissionConfig::new(3, 40, 45).expect("valid config");
    assert_eq!(RefusalReason::UserCap.code(), "user_cap");
    assert_eq!(RefusalReason::GuildCap.code(), "guild_cap");
    assert_eq!(RefusalReason::Cooldown.code(), "cooldown");
    assert_eq!(RefusalReason::UserBurst.code(), "user_burst");
    assert_eq!(RefusalReason::GuildBurst.code(), "guild_burst");
    assert_eq!(
        RefusalReason::UserCap.user_message(&singular),
        "You already have a temporary voice channel."
    );
    assert_eq!(
        RefusalReason::UserCap.user_message(&plural),
        "You already have 3 temporary voice channels."
    );
    assert_eq!(
        RefusalReason::GuildCap.user_message(&singular),
        "This server has reached its temporary voice channel limit. Try again shortly."
    );
    assert_eq!(
        RefusalReason::Cooldown.user_message(&plural),
        "Please wait 45 seconds between creating channels."
    );
    assert_eq!(
        RefusalReason::UserBurst.user_message(&singular),
        "Temporary voice rate limit: 3 creates per 60 seconds. Please try again shortly."
    );
    assert_eq!(
        RefusalReason::GuildBurst.user_message(&singular),
        "This server's temporary voice rate limit is 10 creates per 60 seconds. Please try again shortly."
    );
}

#[test]
fn zero_user_id_is_refused() {
    let mut bad = request(0, 0, None, &[]);
    bad.user_id = 0;
    assert_eq!(
        decide_admission(&config(), &bad, NOW),
        Err(AdmissionError::InvalidUserId)
    );
}

#[test]
fn decision_helpers_report_the_verdict() {
    assert!(AdmissionDecision::Allow.allowed());
    assert_eq!(AdmissionDecision::Allow.refusal(), None);
    let denied = AdmissionDecision::Deny {
        reason: RefusalReason::Cooldown,
    };
    assert!(!denied.allowed());
    assert_eq!(denied.refusal(), Some(RefusalReason::Cooldown));
}
