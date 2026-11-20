//! Slice 3d: runtime preview handle + config-clone API (spec §5.0; T3/T15
//! core halves). Offline only: no Discord, no database, no clock beyond
//! explicit receipt timestamps.
//!
//! Proves the §5.0 stop-and-redesign gate is met without a parallel config
//! path: the handler reaches the shared `AutomodRuntime` for both the
//! effective config clone and the read-only repeat probe, and previews
//! through the runtime neither build nor break a real streak.

use two_bot_core::automod::{
    match_automod, AutomodConfig, AutomodFilter, AutomodMessage, AutomodPolicy, RepeatTracker,
};
use two_bot_core::automod_preview::{preview_automod, PreviewResult};
use two_bot_core::automod_runtime::{
    AutomodRuntime, AutomodScope, Inspection, MessageDelivery, MessageDeliveryKind,
    STAGING_GUILD_ID,
};

fn scoped(config: AutomodConfig) -> AutomodRuntime {
    AutomodRuntime::new(
        config,
        AutomodScope {
            guild_id: STAGING_GUILD_ID.to_owned(),
            live_approved: false,
        },
    )
}

fn live_message(id: &str, content: &str, at_ms: u64) -> AutomodMessage {
    AutomodMessage {
        guild_id: STAGING_GUILD_ID.to_owned(),
        channel_id: "channel-1".to_owned(),
        message_id: id.to_owned(),
        author_id: "author-a".to_owned(),
        author_is_bot: false,
        role_ids: vec![],
        content: content.to_owned(),
        mentioned_user_ids: vec![],
        attachment_names: vec![],
        observed_timestamp_ms: at_ms,
    }
}

fn deliver(message: AutomodMessage, at_ms: u64) -> MessageDelivery {
    MessageDelivery {
        kind: MessageDeliveryKind::Create,
        guild_id: Some(STAGING_GUILD_ID.to_owned()),
        channel_id: message.channel_id.clone(),
        message_id: message.message_id.clone(),
        snapshot: Some(message),
        create_pending_roles: None,
        edited_timestamp_ms: None,
        observed_timestamp_ms: at_ms,
    }
}

fn inspect_create(runtime: &mut AutomodRuntime, id: &str, content: &str, at_ms: u64) -> Inspection {
    runtime.inspect(&deliver(live_message(id, content, at_ms), at_ms))
}

/// T15 core half: the clone tracks the live refresh, and the runtime preview
/// flips with it — no `from_env` re-parse, no parallel config path.
#[test]
fn effective_config_clone_flips_preview_with_live_refresh() {
    let mut runtime = scoped(AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy {
            bad_words: vec!["alpha".to_owned()],
            ..AutomodPolicy::default()
        },
    });
    assert_eq!(
        runtime.effective_config().policy.bad_words,
        vec!["alpha".to_owned()]
    );
    // The clone is a value, not a view: mutating it cannot move the runtime.
    let mut detached = runtime.effective_config();
    detached.policy.bad_words.clear();
    assert_eq!(
        runtime.effective_config().policy.bad_words,
        vec!["alpha".to_owned()],
        "effective_config must clone, not lend"
    );

    fn probe(
        runtime: &AutomodRuntime,
        text: &str,
    ) -> two_bot_core::automod_preview::PreviewVerdict {
        runtime.preview(
            &AutomodMessage {
                message_id: "preview:refresh".to_owned(),
                ..live_message("ignored", text, 1_000_000)
            },
            0,
        )
    }
    assert_eq!(
        probe(&runtime, "has alpha here").filter,
        Some(AutomodFilter::BadWords)
    );

    assert!(runtime.apply_live_config(&AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy {
            bad_words: vec!["beta".to_owned()],
            ..AutomodPolicy::default()
        },
    }));
    assert_eq!(
        runtime.effective_config().policy.bad_words,
        vec!["beta".to_owned()]
    );
    assert_eq!(
        probe(&runtime, "has alpha here").result,
        PreviewResult::NoMatch
    );
    assert_eq!(
        probe(&runtime, "has beta here").filter,
        Some(AutomodFilter::BadWords)
    );
}

/// The runtime's wired preview equals the pure-core entry point on identical
/// history: two seeded CREATEs plus the candidate complete the streak of 3,
/// so the probe reads `Matched(repeated_message)` on both paths — and the
/// third real CREATE matches on both as well, since the probe recorded
/// nothing.
#[test]
fn runtime_preview_matches_pure_core_on_seeded_history() {
    let config = AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy::default(),
    };
    let mut runtime = scoped(config.clone());
    let mut standalone = RepeatTracker::default();
    for (i, at_ms) in [1_000_000_u64, 1_001_000].into_iter().enumerate() {
        let seeded = live_message(&format!("seed-{i}"), "storm text", at_ms);
        assert_eq!(
            match_automod(&seeded, &config.policy, &mut standalone),
            None
        );
        assert!(matches!(
            inspect_create(&mut runtime, &format!("seed-{i}"), "storm text", at_ms),
            Inspection::Accepted(_)
        ));
    }
    let preview = AutomodMessage {
        message_id: "preview:seeded".to_owned(),
        ..live_message("ignored", "storm text", 1_002_000)
    };
    let wired = runtime.preview(&preview, 0);
    let pure = preview_automod(&preview, 0, &runtime.effective_config(), &standalone);
    assert_eq!(wired, pure);
    assert_eq!(wired.result, PreviewResult::Matched);
    assert_eq!(wired.filter, Some(AutomodFilter::RepeatedMessage));

    assert!(matches!(
        inspect_create(&mut runtime, "real-2", "storm text", 1_002_001),
        Inspection::Matched(matched) if matched.filter == AutomodFilter::RepeatedMessage
    ));
    let third = live_message("real-2", "storm text", 1_002_001);
    assert_eq!(
        match_automod(&third, &config.policy, &mut standalone),
        Some(AutomodFilter::RepeatedMessage)
    );
}

/// T3 through the runtime: N previews of identical text, then real CREATEs —
/// the streak counts only the real posts.
#[test]
fn preview_storm_through_runtime_builds_no_streak() {
    let mut runtime = scoped(AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy::default(),
    });
    for i in 0..5 {
        let preview = AutomodMessage {
            message_id: format!("preview:storm-{i}"),
            ..live_message("ignored", "storm text", 2_000_000)
        };
        let verdict = runtime.preview(&preview, 0);
        assert_eq!(verdict.result, PreviewResult::NoMatch, "storm preview {i}");
        // Deterministic under a frozen tracker: re-reads agree exactly.
        assert_eq!(runtime.preview(&preview, 0), verdict);
    }
    for i in 0..2 {
        assert!(
            matches!(
                inspect_create(
                    &mut runtime,
                    &format!("real-{i}"),
                    "storm text",
                    2_000_001 + i
                ),
                Inspection::Accepted(_)
            ),
            "only {i} real posts so far"
        );
    }
    assert!(
        matches!(
            inspect_create(&mut runtime, "real-2", "storm text", 2_000_003),
            Inspection::Matched(matched) if matched.filter == AutomodFilter::RepeatedMessage
        ),
        "third real post completes the streak of 3"
    );
}
