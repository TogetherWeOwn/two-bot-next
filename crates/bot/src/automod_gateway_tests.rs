#![cfg(test)]

use std::collections::HashMap;
use std::future::{pending, Future};

use super::*;
use serde_json::json;
use two_bot_core::automod_runtime::{
    AutomodMatch, DeliveryKey, LedgerClaim, MessageDeliveryKind, MessageSubject, StoredOutcome,
    TargetFacts, ViolationRecord, STAGING_GUILD_ID,
};
use two_bot_core::AutomodFilter;
use two_bot_discord::automod_activation::{FetchedMessage, RetainReason};

const OWEN: &str = "123456789012345678";
const ROLE: &str = "234567890123456789";
const GUILD: u64 = 1_545_644_954_272_137_297;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn disabled_automod_resolves_to_nothing() {
    assert!(resolve(&vars(&[]), GUILD).unwrap().is_none());
    assert!(resolve(&vars(&[("TWO_AUTOMOD", "0")]), GUILD)
        .unwrap()
        .is_none());
}

#[test]
fn enabled_automod_needs_owen_and_valid_roles() {
    assert!(resolve(&vars(&[("TWO_AUTOMOD", "1")]), GUILD).is_err());
    assert!(resolve(
        &vars(&[("TWO_AUTOMOD", "1"), ("TWO_OWEN_USER_ID", "owen")]),
        GUILD
    )
    .is_err());
    assert!(resolve(
        &vars(&[
            ("TWO_AUTOMOD", "1"),
            ("TWO_OWEN_USER_ID", OWEN),
            ("TWO_MODERATION_PROTECTED_ROLE_IDS", "not-a-role"),
        ]),
        GUILD
    )
    .is_err());
}

#[test]
fn owen_and_protection_never_depend_on_the_enforce_flag() {
    let base = [
        ("TWO_AUTOMOD", "1"),
        ("TWO_OWEN_USER_ID", OWEN),
        ("TWO_MODERATION_PROTECTED_ROLE_IDS", ROLE),
    ];
    let mut enforcing = base.to_vec();
    enforcing.push(("TWO_AUTOMOD_ENFORCE", "1"));
    let preview = resolve(&vars(&base), GUILD).unwrap().unwrap();
    let enforce = resolve(&vars(&enforcing), GUILD).unwrap().unwrap();
    assert!(preview.config.dry_run && !enforce.config.dry_run);
    assert_eq!(preview.owen_user_id, OWEN);
    assert_eq!(enforce.owen_user_id, OWEN);
    assert_eq!(preview.protected_role_ids, enforce.protected_role_ids);
    assert!(preview.protected_role_ids.contains(ROLE));
    // Live approval is never inferred from any gate here.
    assert!(!preview.scope.live_approved && !enforce.scope.live_approved);
    assert!(preview.scope.permits(STAGING_GUILD_ID));
    // Any other configured guild falls outside the staging fence.
    let other = resolve(&vars(&base), 42).unwrap().unwrap();
    assert!(!other.scope.permits(STAGING_GUILD_ID));
}

#[test]
fn partial_message_update_decodes_before_twilight_parse() {
    let minimal = json!({
        "op": 0, "s": 7, "t": "MESSAGE_UPDATE",
        "d": {"id": "9", "channel_id": "8", "guild_id": "7",
              "edited_timestamp": "2026-10-02T00:00:01.000000+00:00",
              "content": "edited"}
    })
    .to_string();
    let delivery = partial_edit(&minimal, 1_790_726_400_000).expect("partial edit");
    assert_eq!(delivery.kind, MessageDeliveryKind::Update);
    assert_eq!(delivery.message_id, "9");
    assert_eq!(delivery.channel_id, "8");
    assert_eq!(delivery.guild_id.as_deref(), Some("7"));
    assert!(delivery.snapshot.is_none() && delivery.create_pending_roles.is_none());
    assert!(delivery.edited_timestamp_ms.is_some());
    assert_eq!(delivery.observed_timestamp_ms, 1_790_726_400_000);
}

#[test]
fn only_message_update_dispatches_are_partial_edits() {
    let create =
        json!({"op": 0, "s": 1, "t": "MESSAGE_CREATE", "d": {"id": "9", "channel_id": "8"}})
            .to_string();
    assert!(partial_edit(&create, 0).is_none());
    let junk = r#"{"t":"MESSAGE_UPDATE","d":{"content":"no ids"}}"#;
    assert!(partial_edit(junk, 0).is_none());
    assert!(partial_edit("not json MESSAGE_UPDATE", 0).is_none());
}

#[test]
fn text_automations_run_only_for_accepted_or_unscreened_messages() {
    assert!(runs_text_automations(None));
    assert!(runs_text_automations(Some(FunnelDisposition::Accept)));
    assert!(!runs_text_automations(Some(FunnelDisposition::CaptureOnly)));
    assert!(!runs_text_automations(Some(FunnelDisposition::None)));
}

#[test]
fn receipt_clock_parses_or_falls_back_to_zero() {
    assert_eq!(receipt_ms("2026-10-02T00:00:00.000Z"), 1_790_899_200_000);
    assert_eq!(receipt_ms("garbage"), 0);
}

struct HangingLedger;

impl AutomodClaimLedger for HangingLedger {
    type Claim = ();
    type Error = String;

    fn ledger_claim(
        &self,
        _: &DeliveryKey,
    ) -> impl Future<Output = Result<LedgerClaim<()>, String>> + Send {
        pending()
    }
    fn ledger_preserve(
        &self,
        _: &(),
        _: &AutomodMatch,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        pending()
    }
    fn ledger_mark_started(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
        pending()
    }
    fn ledger_complete(
        &self,
        _: &(),
        _: &StoredOutcome,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        pending()
    }
    fn ledger_release(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
        pending()
    }
    fn ledger_record(
        &self,
        _: &(),
        _: &MessageSubject,
        _: AutomodFilter,
        _: &str,
    ) -> impl Future<Output = Result<ViolationRecord, String>> + Send {
        pending()
    }
}

struct HangingFacts;

impl AutomodFacts for HangingFacts {
    fn fetch_message(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> impl Future<Output = Option<FetchedMessage>> + Send {
        pending()
    }
    fn target_facts(&self, _: &MessageSubject) -> impl Future<Output = Option<TargetFacts>> + Send {
        pending()
    }
}

fn delivery(kind: MessageDeliveryKind) -> MessageDelivery {
    MessageDelivery {
        kind,
        guild_id: Some(STAGING_GUILD_ID.to_owned()),
        channel_id: "8".to_owned(),
        message_id: "9".to_owned(),
        snapshot: None,
        create_pending_roles: None,
        edited_timestamp_ms: None,
        observed_timestamp_ms: 1,
    }
}

/// TOG-19027: stored automod lists/thresholds reach the consumer through
/// the live snapshot, without a restart. The poller publishes; the same
/// process re-resolves and observes the stored values.
#[test]
fn live_snapshot_moves_automod_lists_and_thresholds_without_restart() {
    let deployment = vars(&[
        ("TWO_AUTOMOD", "1"),
        ("TWO_OWEN_USER_ID", OWEN),
        ("TWO_MODERATION_PROTECTED_ROLE_IDS", ROLE),
    ]);
    let boot = resolve_with_live(&deployment, GUILD, None)
        .expect("boot resolves")
        .expect("automod on");
    assert!(boot.config.policy.bad_words.is_empty());
    assert_eq!(boot.config.policy.repeated_message_count, 3);
    assert_eq!(boot.config.policy.mention_limit, 5);
    assert!(boot.config.dry_run, "absent ENFORCE stays dry-run");

    let guild = GUILD.to_string();
    let (mut writer, live) = two_bot_core::settings::live_channel();
    writer.publish(&two_bot_core::settings::SettingsSnapshot {
        revision: 1,
        rows: vec![
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_BAD_WORDS".to_owned(),
                value: json!(["spamword"]),
                version: 1,
            },
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_REPEAT_COUNT".to_owned(),
                value: json!(7),
                version: 1,
            },
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_MENTION_LIMIT".to_owned(),
                value: json!(2),
                version: 1,
            },
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_ENFORCE".to_owned(),
                value: json!(true),
                version: 1,
            },
        ],
    });

    let reloaded = resolve_with_live(&deployment, GUILD, Some(&live))
        .expect("live resolves")
        .expect("automod on");
    assert_eq!(reloaded.config.policy.bad_words, vec!["spamword"]);
    assert_eq!(reloaded.config.policy.repeated_message_count, 7);
    assert_eq!(reloaded.config.policy.mention_limit, 2);
    assert!(
        !reloaded.config.dry_run,
        "stored ENFORCE=1 arms enforcement"
    );

    // A deleted row hands the key back to the deployment environment.
    writer.publish(&two_bot_core::settings::SettingsSnapshot {
        revision: 2,
        rows: vec![],
    });
    let reverted = resolve_with_live(&deployment, GUILD, Some(&live))
        .expect("revert resolves")
        .expect("automod on");
    assert!(reverted.config.policy.bad_words.is_empty());
    assert_eq!(reverted.config.policy.repeated_message_count, 3);
}

/// The running activation applies the live policy in place: the first
/// refresh after a stored write reports a change, a repeat reports none, and
/// repeat history is never rebuilt (no restart, no new activation).
#[tokio::test]
async fn running_activation_applies_live_policy_without_restart() {
    crate::gateway::ensure_crypto_provider();
    let deployment = vars(&[
        ("TWO_AUTOMOD", "1"),
        ("TWO_OWEN_USER_ID", OWEN),
        ("TWO_MODERATION_PROTECTED_ROLE_IDS", ROLE),
    ]);
    let resolved = resolve(&deployment, GUILD)
        .expect("boot resolves")
        .expect("automod on");
    let executor = ActionExecutor::with_proxy(
        "test-token".to_owned(),
        Some("http://127.0.0.1:9".to_owned()),
    )
    .expect("executor builds");
    let activation = AutomodActivation::new(
        AutomodRuntime::new(resolved.config, resolved.scope),
        HangingLedger,
        HangingFacts,
        executor,
    );

    let guild = GUILD.to_string();
    let (mut writer, live) = two_bot_core::settings::live_channel();
    assert!(
        !refresh_live(&activation, &deployment, &guild, &live),
        "empty snapshot changes nothing"
    );
    writer.publish(&two_bot_core::settings::SettingsSnapshot {
        revision: 1,
        rows: vec![two_bot_core::settings::SettingRow {
            guild_id: guild.clone(),
            key: "TWO_AUTOMOD_BAD_WORDS".to_owned(),
            value: json!(["spamword"]),
            version: 1,
        }],
    });
    assert!(
        refresh_live(&activation, &deployment, &guild, &live),
        "stored bad words move the running policy"
    );
    assert!(
        !refresh_live(&activation, &deployment, &guild, &live),
        "unchanged snapshot is a no-op"
    );
}

/// One bad stored value does not take the whole policy down: the offending
/// key falls back to its boot value while the other stored values still
/// apply, so the refresh still reports a change.
#[tokio::test]
async fn refresh_falls_back_per_key_on_a_bad_stored_value() {
    crate::gateway::ensure_crypto_provider();
    let deployment = vars(&[
        ("TWO_AUTOMOD", "1"),
        ("TWO_OWEN_USER_ID", OWEN),
        ("TWO_MODERATION_PROTECTED_ROLE_IDS", ROLE),
    ]);
    let resolved = resolve(&deployment, GUILD)
        .expect("boot resolves")
        .expect("automod on");
    let executor = ActionExecutor::with_proxy(
        "test-token".to_owned(),
        Some("http://127.0.0.1:9".to_owned()),
    )
    .expect("executor builds");
    let activation = AutomodActivation::new(
        AutomodRuntime::new(resolved.config, resolved.scope),
        HangingLedger,
        HangingFacts,
        executor,
    );

    let guild = GUILD.to_string();
    let (mut writer, live) = two_bot_core::settings::live_channel();
    writer.publish(&two_bot_core::settings::SettingsSnapshot {
        revision: 1,
        rows: vec![
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_BAD_WORDS".to_owned(),
                value: json!(["spamword"]),
                version: 1,
            },
            two_bot_core::settings::SettingRow {
                guild_id: guild.clone(),
                key: "TWO_AUTOMOD_REPEAT_COUNT".to_owned(),
                value: json!(999),
                version: 1,
            },
        ],
    });
    assert!(
        refresh_live(&activation, &deployment, &guild, &live),
        "the good stored value applies while the bad one falls back"
    );
    assert!(
        !refresh_live(&activation, &deployment, &guild, &live),
        "settled fallback refresh is a no-op"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stalled_delivery_times_out_without_becoming_acceptance() {
    crate::gateway::ensure_crypto_provider();
    let mut env = HashMap::new();
    env.insert("TWO_AUTOMOD".to_owned(), "1".to_owned());
    let runtime = AutomodRuntime::new(
        AutomodConfig::from_map(&env).unwrap(),
        AutomodScope {
            guild_id: STAGING_GUILD_ID.to_owned(),
            live_approved: false,
        },
    );
    let executor = ActionExecutor::with_proxy(
        "test-token".to_owned(),
        Some("http://127.0.0.1:9".to_owned()),
    )
    .expect("executor builds");
    let activation = AutomodActivation::new(runtime, HangingLedger, HangingFacts, executor);
    assert_eq!(
        process(
            &activation,
            delivery(MessageDeliveryKind::Create),
            "2026-10-02T00:00:00.000Z"
        )
        .await,
        WorkerVerdict {
            funnel: FunnelDisposition::CaptureOnly,
            trigger: FunnelDisposition::CaptureOnly,
        }
    );
    assert_eq!(
        process(
            &activation,
            delivery(MessageDeliveryKind::Update),
            "2026-10-02T00:00:00.000Z"
        )
        .await
        .funnel,
        FunnelDisposition::None
    );
}

#[test]
fn uninspected_create_keeps_funnel_accept_with_capture_only_trigger() {
    let bypassed = Activation {
        disposition: FunnelDisposition::Accept,
        outcome: ActivationOutcome::Bypassed,
    };
    assert_eq!(
        verdict_of(&bypassed, MessageDeliveryKind::Create),
        WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::CaptureOnly,
        }
    );
}

#[test]
fn settled_clean_create_hands_its_accept_to_triggers() {
    let clean = Activation {
        disposition: FunnelDisposition::CaptureOnly,
        outcome: ActivationOutcome::Duplicate(Some(StoredOutcome {
            matched: false,
            deleted: false,
            outcome: two_bot_core::automod_runtime::CompletionKind::Accepted,
        })),
    };
    assert_eq!(
        verdict_of(&clean, MessageDeliveryKind::Create),
        WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::Accept,
        }
    );
}

#[test]
fn retained_completion_hands_triggers_a_capture_only_verdict() {
    let retained = Activation {
        disposition: FunnelDisposition::Accept,
        outcome: ActivationOutcome::Retained(RetainReason::CompletionRefused),
    };
    assert_eq!(
        verdict_of(&retained, MessageDeliveryKind::Create),
        WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::CaptureOnly,
        }
    );
}
