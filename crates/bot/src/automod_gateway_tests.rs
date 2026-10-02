use std::collections::HashMap;
use std::future::{pending, Future};

use super::*;
use serde_json::json;
use two_bot_core::automod_runtime::{
    AutomodMatch, DeliveryKey, LedgerClaim, MessageDeliveryKind, MessageSubject, StoredOutcome,
    TargetFacts, ViolationRecord, STAGING_GUILD_ID,
};
use two_bot_core::AutomodFilter;
use two_bot_discord::automod_activation::FetchedMessage;

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
        FunnelDisposition::CaptureOnly
    );
    assert_eq!(
        process(
            &activation,
            delivery(MessageDeliveryKind::Update),
            "2026-10-02T00:00:00.000Z"
        )
        .await,
        FunnelDisposition::None
    );
}
