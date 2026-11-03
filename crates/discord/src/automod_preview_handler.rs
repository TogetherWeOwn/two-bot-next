//! Thin Discord wiring for `/automod-preview`
//! (docs/automod-preview-spec.md §§3.1, 5, 6; slice 3h).
//!
//! This module owns three things and nothing else: the twilight mapping from
//! an application-command interaction to a [`PreviewRequest`], the
//! lock-scoped runtime snapshot plus synthetic-message build, and the
//! transaction that carries the claim. Every protocol decision — strict
//! decode first, scope gate before any budget or database touch, locked
//! claim-and-read, limiter charge, single evaluation, content-free commit
//! before any verdict reply — lives in
//! [`two_bot_core::automod_preview_attempt::attempt_preview`], with the
//! ordering plan in [`two_bot_core::automod_preview_flow`]. The wiring
//! contributes no oracle bit of its own: unknown shapes answer the generic
//! unavailable line, never a guessed verdict.
//!
//! Caller contract (mirrors the gateway refresh precedent): the caller
//! refreshes the activation from the live snapshot before calling, so the
//! lock-scoped [`AutomodActivation::preview`] read sees the current effective
//! config. The runtime mutex is released before any audit I/O await; it is
//! never held across the transaction's awaits.

#[cfg(feature = "db")]
use twilight_model::application::interaction::InteractionData;
use twilight_model::application::interaction::{
    application_command::{CommandData, CommandOptionValue},
    Interaction,
};
use two_bot_core::automod_preview_attempt::{
    attempt_preview, AttemptInput, AttemptResult, PreviewAttemptStore,
};
use two_bot_core::automod_preview_flow::{plan_pre_claim, PreClaimPlan};
use two_bot_core::automod_preview_input::{
    build_preview_message, decode_preview_input, DecodedPreviewInput, PreviewInputRefusal,
    PreviewOptionValue,
};
use two_bot_core::automod_preview_limiter::PreviewRateLimiter;
use two_bot_core::automod_preview_report::{
    metric_not_evaluated, metric_unavailable, PREVIEW_NOT_EVALUATED_REPLY,
    PREVIEW_UNAVAILABLE_REPLY,
};
use two_bot_core::funnel::format_iso_millis;

use crate::automod_activation::{AutomodActivation, AutomodFacts};
#[cfg(feature = "db")]
use crate::channel_moderation::invoking_channel_id;
use two_bot_core::automod_runtime::AutomodClaimLedger;

/// Everything the wire takes from one `/automod-preview` invocation: the
/// caller-supplied clocks excepted, every field comes from the interaction
/// itself (§3.1). Author, channel, and guild are the invoker's own; mentions
/// count the synthetic cohort and attachments carry bare names only — the
/// mapping keeps no content beyond what strict decode already accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    /// `None` for a DM invocation, which the gate answers `not_evaluated`.
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub actor_id: String,
    pub role_ids: Vec<String>,
    /// Raw interaction id: the limiter's idempotency key and the audit claim.
    pub interaction_id: u64,
}

/// Modern `channel` only, so this module compiles on every feature
/// combination (the shared helper lives in the `db`-gated moderation
/// module). `/automod-preview` is a new command, and Discord always sends
/// `channel` for it — the legacy `channel_id` field is deprecated in the
/// Discord API, so there is no fallback to it here. A missing `channel`
/// yields `None`, and the caller answers unavailable without touching
/// decode, the limiter, or the database.
#[cfg(not(feature = "db"))]
fn invoking_channel_id(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.to_string())
}

impl PreviewRequest {
    /// Extract the invocation fields. `None` means the interaction cannot
    /// form a request at all (no invokable channel, no member, or no member
    /// user): the caller answers unavailable without touching decode, the
    /// limiter, or the database. A missing guild is *not* `None` — DMs reach
    /// the scope gate and answer `not_evaluated` per spec §5.
    #[must_use]
    pub fn from_interaction(interaction: &Interaction) -> Option<Self> {
        let member = interaction.member.as_ref()?;
        let user = member.user.as_ref()?;
        Some(Self {
            guild_id: interaction.guild_id.map(|id| id.to_string()),
            channel_id: invoking_channel_id(interaction)?,
            actor_id: user.id.to_string(),
            role_ids: member.roles.iter().map(ToString::to_string).collect(),
            interaction_id: interaction.id.get(),
        })
    }
}

/// Map one slash command's options to the strict decoder's borrowed shape.
/// Unknown option *values* map to `Other` and refuse downstream; this mapping
/// itself never refuses — strictness lives in
/// [`decode_preview_input`].
#[must_use]
pub fn map_preview_options(data: &CommandData) -> Vec<(String, PreviewOptionValue<'_>)> {
    data.options
        .iter()
        .map(|option| {
            let value = match &option.value {
                CommandOptionValue::String(text) => PreviewOptionValue::Text(text),
                CommandOptionValue::Integer(count) => PreviewOptionValue::Integer(*count),
                _ => PreviewOptionValue::Other,
            };
            (option.name.clone(), value)
        })
        .collect()
}

/// Strict-decode one slash command's options (§3.1, option B).
pub fn decode_interaction_options(
    data: &CommandData,
) -> Result<DecodedPreviewInput, PreviewInputRefusal> {
    let mapped = map_preview_options(data);
    let borrowed: Vec<(&str, PreviewOptionValue<'_>)> = mapped
        .iter()
        .map(|(name, value)| (name.as_str(), *value))
        .collect();
    decode_preview_input(&borrowed)
}

/// Resolve the activation against the strict decode: decode refusals win over
/// the gate, a missing runtime answers `not_evaluated` without building one
/// ad hoc (§5.0), and only a live activation with accepted input proceeds.
fn resolve_activation<'a, L, F>(
    activation: Option<&'a AutomodActivation<L, F>>,
    decoded: &Result<DecodedPreviewInput, PreviewInputRefusal>,
) -> Result<&'a AutomodActivation<L, F>, AttemptResult>
where
    L: AutomodClaimLedger,
    F: AutomodFacts,
{
    match (activation, decoded) {
        (_, Err(refusal)) => Err(AttemptResult {
            reply: refusal.message().to_owned(),
            metric: None,
            evaluated: false,
        }),
        (Some(active), Ok(_)) => Ok(active),
        (None, Ok(_)) => Err(AttemptResult {
            reply: PREVIEW_NOT_EVALUATED_REPLY.to_owned(),
            metric: Some(metric_not_evaluated()),
            evaluated: false,
        }),
    }
}

fn unavailable(dry_run: Option<bool>) -> AttemptResult {
    AttemptResult {
        reply: PREVIEW_UNAVAILABLE_REPLY.to_owned(),
        metric: Some(metric_unavailable(dry_run)),
        evaluated: false,
    }
}

/// Store-generic attempt core: resolve, lock-scoped snapshot, synthetic
/// message, and the driver call. The caller owns the transaction (and the
/// refresh that precedes it); this function never begins, commits, or rolls
/// back. `now_ms` is the caller-supplied clock — the receipt instant and the
/// limiter share the one timestamp. Answers before any store touch on
/// refusal, gate, or missing runtime.
pub async fn preview_attempt_via<L, F, S>(
    activation: Option<&AutomodActivation<L, F>>,
    limiter: &mut PreviewRateLimiter,
    request: &PreviewRequest,
    decoded: &Result<DecodedPreviewInput, PreviewInputRefusal>,
    now_ms: u64,
    store: S,
) -> AttemptResult
where
    L: AutomodClaimLedger,
    F: AutomodFacts,
    S: PreviewAttemptStore,
{
    let activation = match resolve_activation(activation, decoded) {
        Ok(active) => active,
        Err(answer) => return answer,
    };
    let as_ref: Result<&DecodedPreviewInput, PreviewInputRefusal> = match decoded {
        Ok(input) => Ok(input),
        Err(refusal) => Err(*refusal),
    };
    let interaction_id = request.interaction_id.to_string();
    let (admitted, dry_run) = activation.preview(|runtime| {
        (
            runtime.admits(request.guild_id.as_deref()),
            runtime.dry_run(),
        )
    });
    if let PreClaimPlan::AnswerNow { reply, metric } =
        plan_pre_claim(as_ref, admitted, admitted, &interaction_id)
    {
        return AttemptResult {
            reply,
            metric,
            evaluated: false,
        };
    }
    let Ok(preview_input) = as_ref else {
        // Unreachable: `Claim` came from an `Ok` decode. Fail closed without
        // touching the store.
        return unavailable(Some(dry_run));
    };
    let guild_id = match request.guild_id.as_deref() {
        Some(guild_id) => guild_id,
        // Unreachable: a claim requires `admits`, which requires a guild.
        // Fail closed rather than evaluating under an empty scope.
        None => return unavailable(Some(dry_run)),
    };
    let message = match build_preview_message(
        preview_input,
        guild_id,
        &request.channel_id,
        &request.actor_id,
        &request.role_ids,
        request.interaction_id,
        now_ms,
    ) {
        Ok(message) => message,
        // Unreachable post-decode: the builder re-checks the invariants
        // strict decode already enforced. Fail closed, no store touch.
        Err(_) => return unavailable(Some(dry_run)),
    };
    let receipt_iso = format_iso_millis(i64::try_from(now_ms).unwrap_or(i64::MAX));
    let input = AttemptInput {
        decoded: as_ref,
        automod_enabled: admitted,
        in_scope_guild: admitted,
        guild_id,
        actor_id: &request.actor_id,
        channel_id: &request.channel_id,
        interaction_id: &interaction_id,
        interaction_numeric: request.interaction_id,
        receipt_iso: &receipt_iso,
        now_ms,
        dry_run,
    };
    let mention_count = preview_input.mention_count;
    attempt_preview(input, limiter, store, || {
        activation.preview(|runtime| runtime.preview(&message, mention_count))
    })
    .await
}

/// Transaction store for the §6 claim: the advisory lock and the audit row
/// share the one transaction connection, and the row text the winner commits
/// is the timestamp every reply renders.
#[cfg(feature = "db")]
pub struct SqlPreviewStore<'t> {
    tx: sqlx::Transaction<'t, sqlx::Postgres>,
    interaction_id: String,
}

#[cfg(feature = "db")]
impl<'t> SqlPreviewStore<'t> {
    /// Wrap an already-begun transaction. `interaction_id` must be the same
    /// interaction the audit id derives from, so the lock key and the row id
    /// stay the one `automod-preview:<id>` string.
    #[must_use]
    pub fn new(tx: sqlx::Transaction<'t, sqlx::Postgres>, interaction_id: String) -> Self {
        Self { tx, interaction_id }
    }
}

#[cfg(feature = "db")]
impl PreviewAttemptStore for SqlPreviewStore<'_> {
    type Error = sqlx::Error;

    async fn claim_and_read(
        &mut self,
        audit_id: &str,
    ) -> Result<Option<two_bot_core::automod_preview_audit::PreviewAuditRow>, Self::Error> {
        two_bot_core::automod_preview_audit::lock_preview_attempt(
            &mut self.tx,
            &self.interaction_id,
        )
        .await?;
        two_bot_core::automod_preview_audit::load_preview_audit(&mut *self.tx, audit_id).await
    }

    async fn insert_and_commit(
        mut self,
        record: two_bot_core::custom_commands::AuditRecord,
        audit_id: &str,
        receipt_iso: &str,
    ) -> Result<String, Self::Error> {
        let stored = two_bot_core::automod_preview_audit::insert_preview_audit(
            &mut *self.tx,
            &record,
            audit_id,
            receipt_iso,
        )
        .await?;
        self.tx.commit().await?;
        Ok(stored)
    }
}

/// Answer one `/automod-preview` slash interaction through the §6 driver:
/// extract the invocation, strict-decode the options, resolve the runtime,
/// then begin the claim transaction. Anything that cannot form a request —
/// or a database that will not begin — answers the generic unavailable line
/// with the effective mode when a snapshot exists, and commits nothing.
#[cfg(feature = "db")]
pub async fn handle_automod_preview<L, F>(
    activation: Option<&AutomodActivation<L, F>>,
    pool: &sqlx::PgPool,
    limiter: &mut PreviewRateLimiter,
    interaction: &Interaction,
    now_ms: u64,
) -> AttemptResult
where
    L: AutomodClaimLedger,
    F: AutomodFacts,
{
    let snapshot_dry_run = activation.map(|active| active.preview(|runtime| runtime.dry_run()));
    let request = match PreviewRequest::from_interaction(interaction) {
        Some(request) => request,
        None => return unavailable(snapshot_dry_run),
    };
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return unavailable(snapshot_dry_run);
    };
    let decoded = decode_interaction_options(data);
    let activation = match resolve_activation(activation, &decoded) {
        // Decode refusal or missing runtime: answer before any pool touch.
        Ok(active) => active,
        Err(answer) => return answer,
    };
    let dry_run = activation.preview(|runtime| runtime.dry_run());
    let tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(_) => return unavailable(Some(dry_run)),
    };
    let store = SqlPreviewStore::new(tx, request.interaction_id.to_string());
    preview_attempt_via(Some(activation), limiter, &request, &decoded, now_ms, store).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automod_activation::FetchedMessage;
    use serde_json::json;
    use std::fmt;
    use std::future::{ready, Future};
    use std::sync::{Arc, Mutex};
    use two_bot_core::automod::{AutomodConfig, AutomodPolicy};
    use two_bot_core::automod_preview_audit::PreviewAuditRow;
    use two_bot_core::automod_preview_report::{metric_unavailable, render_preview_reply};
    use two_bot_core::automod_runtime::{
        AutomodMatch, AutomodRuntime, AutomodScope, DeliveryKey, LedgerClaim, MessageSubject,
        StoredOutcome, TargetFacts, ViolationRecord, STAGING_GUILD_ID,
    };
    use two_bot_core::custom_commands::AuditRecord;

    struct StubLedger;

    impl AutomodClaimLedger for StubLedger {
        type Claim = ();
        type Error = String;

        fn ledger_claim(
            &self,
            _: &DeliveryKey,
        ) -> impl Future<Output = Result<LedgerClaim<()>, String>> + Send {
            ready(Err("preview path never claims".to_owned()))
        }

        fn ledger_preserve(
            &self,
            _: &(),
            _: &AutomodMatch,
        ) -> impl Future<Output = Result<bool, String>> + Send {
            ready(Ok(false))
        }

        fn ledger_mark_started(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
            ready(Ok(false))
        }

        fn ledger_complete(
            &self,
            _: &(),
            _: &StoredOutcome,
        ) -> impl Future<Output = Result<bool, String>> + Send {
            ready(Ok(false))
        }

        fn ledger_release(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
            ready(Ok(false))
        }

        fn ledger_record(
            &self,
            _: &(),
            _: &MessageSubject,
            _: two_bot_core::automod::AutomodFilter,
            _: &str,
        ) -> impl Future<Output = Result<ViolationRecord, String>> + Send {
            ready(Ok(ViolationRecord {
                count: 0,
                inserted: false,
            }))
        }
    }

    struct StubFacts;

    impl AutomodFacts for StubFacts {
        fn fetch_message(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> impl Future<Output = Option<FetchedMessage>> + Send {
            ready(None)
        }

        fn target_facts(
            &self,
            _: &MessageSubject,
        ) -> impl Future<Output = Option<TargetFacts>> + Send {
            ready(None)
        }
    }

    #[derive(Debug)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "fake store: {}", self.0)
        }
    }

    #[derive(Debug, Default)]
    struct Probe {
        lock_calls: usize,
        commits: Vec<(String, String)>,
    }

    /// Offline stand-in for the claim transaction: replays one canned row,
    /// records commits to shared probe state, and fails on demand — no
    /// database. `Send` so the attempt future keeps the pool entry's shape.
    struct FakeStore {
        row: Option<PreviewAuditRow>,
        probe: Arc<Mutex<Probe>>,
        fail_claim: bool,
        fail_insert: bool,
    }

    impl FakeStore {
        fn fresh() -> (Self, Arc<Mutex<Probe>>) {
            let probe = Arc::new(Mutex::new(Probe::default()));
            (
                Self {
                    row: None,
                    probe: Arc::clone(&probe),
                    fail_claim: false,
                    fail_insert: false,
                },
                probe,
            )
        }
    }

    impl PreviewAttemptStore for FakeStore {
        type Error = FakeError;

        async fn claim_and_read(
            &mut self,
            _audit_id: &str,
        ) -> Result<Option<PreviewAuditRow>, Self::Error> {
            self.probe.lock().expect("probe is not poisoned").lock_calls += 1;
            if self.fail_claim {
                return Err(FakeError("claim failed"));
            }
            Ok(self.row.clone())
        }

        async fn insert_and_commit(
            self,
            _record: AuditRecord,
            audit_id: &str,
            receipt_iso: &str,
        ) -> Result<String, Self::Error> {
            if self.fail_insert {
                return Err(FakeError("insert failed"));
            }
            self.probe
                .lock()
                .expect("probe is not poisoned")
                .commits
                .push((audit_id.to_owned(), receipt_iso.to_owned()));
            Ok(receipt_iso.to_owned())
        }
    }

    fn preview_interaction(options: serde_json::Value) -> Interaction {
        serde_json::from_value(json!({
            "id": "4242", "application_id": "1111", "type": 2,
            "guild_id": STAGING_GUILD_ID,
            "channel": {"id": "777", "type": 0},
            "data": {"id": "1", "type": 1, "name": "automod-preview",
                "options": options},
            "member": {"user": {"id": "4242", "username": "previewer", "discriminator": "0"},
                "roles": ["9"], "flags": 0, "deaf": false, "mute": false},
            "token": "synthetic-test-token", "version": 1,
            "authorizing_integration_owners": {"0": STAGING_GUILD_ID},
            "entitlements": []
        }))
        .expect("synthetic preview interaction")
    }

    fn text_options(text: &str) -> serde_json::Value {
        json!([{"name": "text", "type": 3, "value": text}])
    }

    fn command_data(options: serde_json::Value) -> CommandData {
        serde_json::from_value(
            json!({"id": "1", "type": 1, "name": "automod-preview", "options": options}),
        )
        .expect("synthetic command data")
    }

    fn staging_activation(
        enabled: bool,
        bad_words: &[&str],
    ) -> AutomodActivation<StubLedger, StubFacts> {
        let config = AutomodConfig {
            enabled,
            dry_run: true,
            policy: AutomodPolicy {
                bad_words: bad_words.iter().map(ToString::to_string).collect(),
                ..AutomodPolicy::default()
            },
        };
        let scope = AutomodScope {
            guild_id: STAGING_GUILD_ID.to_owned(),
            live_approved: false,
        };
        AutomodActivation::new(
            AutomodRuntime::new(config, scope),
            StubLedger,
            StubFacts,
            crate::ActionExecutor::with_proxy(
                "synthetic-test-token".to_owned(),
                Some("http://127.0.0.1:1".to_owned()),
            )
            .expect("synthetic executor"),
        )
    }

    fn staging_request() -> PreviewRequest {
        PreviewRequest {
            guild_id: Some(STAGING_GUILD_ID.to_owned()),
            channel_id: "777".to_owned(),
            actor_id: "4242".to_owned(),
            role_ids: vec!["9".to_owned()],
            interaction_id: 4242,
        }
    }

    #[test]
    fn request_takes_invocation_ids_and_member_roles() {
        let request = PreviewRequest::from_interaction(&preview_interaction(text_options("hi")))
            .expect("complete invocation forms a request");
        assert_eq!(request.guild_id.as_deref(), Some(STAGING_GUILD_ID));
        assert_eq!(request.channel_id, "777");
        assert_eq!(request.actor_id, "4242");
        assert_eq!(request.role_ids, vec!["9".to_owned()]);
        assert_eq!(request.interaction_id, 4242);
    }

    #[test]
    fn request_maps_dm_to_no_guild_and_refuses_contextless_shapes() {
        let mut dm = preview_interaction(text_options("hi"));
        dm.guild_id = None;
        let request = PreviewRequest::from_interaction(&dm).expect("DM still forms a request");
        assert_eq!(request.guild_id, None);

        for missing in ["member", "member_user", "channel"] {
            let mut interaction = preview_interaction(text_options("hi"));
            match missing {
                "member" => interaction.member = None,
                "member_user" => {
                    interaction.member.as_mut().expect("fixture member").user = None;
                }
                "channel" => interaction.channel = None,
                _ => unreachable!("one missing field at a time"),
            }
            assert_eq!(
                PreviewRequest::from_interaction(&interaction),
                None,
                "missing {missing} cannot form a request"
            );
        }
    }

    #[test]
    fn options_map_string_and_integer_and_refuse_the_rest_downstream() {
        // The mapping itself never refuses: a boolean attachment value maps
        // to `Other`, and strict decode refuses it downstream.
        let data = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "mentions", "type": 4, "value": 3},
            {"name": "attachments", "type": 5, "value": true}
        ]));
        let mapped = map_preview_options(&data);
        assert_eq!(mapped.len(), 3);
        assert!(matches!(mapped[0].1, PreviewOptionValue::Text("hello")));
        assert!(matches!(mapped[1].1, PreviewOptionValue::Integer(3)));
        assert!(matches!(mapped[2].1, PreviewOptionValue::Other));
        assert_eq!(
            decode_interaction_options(&data),
            Err(PreviewInputRefusal::WrongType)
        );

        let valid = command_data(json!([
            {"name": "text", "type": 3, "value": "hello"},
            {"name": "mentions", "type": 4, "value": 3}
        ]));
        let decoded = decode_interaction_options(&valid).expect("valid options decode");
        assert_eq!(decoded.text, "hello");
        assert_eq!(decoded.mention_count, 3);
    }

    #[test]
    fn decode_refusal_beats_a_missing_runtime() {
        let decoded = Err(PreviewInputRefusal::MissingText);
        let Err(answer) = resolve_activation::<StubLedger, StubFacts>(None, &decoded) else {
            panic!("refusal answers, nothing proceeds");
        };
        assert_eq!(answer.reply, PreviewInputRefusal::MissingText.message());
        assert_eq!(answer.metric, None);
        assert!(!answer.evaluated);
    }

    #[test]
    fn missing_runtime_answers_not_evaluated_for_accepted_input() {
        let decoded = decode_interaction_options(&command_data(text_options("hello")));
        assert!(decoded.is_ok());
        let Err(answer) = resolve_activation::<StubLedger, StubFacts>(None, &decoded) else {
            panic!("no runtime answers not_evaluated");
        };
        assert_eq!(answer.reply, PREVIEW_NOT_EVALUATED_REPLY);
        assert_eq!(answer.metric, Some(metric_not_evaluated()));
        assert!(!answer.evaluated);
    }

    #[tokio::test]
    async fn via_refuses_before_any_claim_or_charge() {
        let activation = staging_activation(false, &["bannedword"]);
        let request = staging_request();
        let decoded =
            decode_interaction_options(&command_data(text_options("has bannedword here")));
        assert!(decoded.is_ok());
        let mut limiter = PreviewRateLimiter::new();
        let (store, probe) = FakeStore::fresh();
        let answer = preview_attempt_via(
            Some(&activation),
            &mut limiter,
            &request,
            &decoded,
            1_700_000_000_000,
            store,
        )
        .await;
        assert_eq!(answer.reply, PREVIEW_NOT_EVALUATED_REPLY);
        assert_eq!(answer.metric, Some(metric_not_evaluated()));
        assert!(!answer.evaluated);
        let probe = probe.lock().expect("probe is not poisoned");
        assert_eq!(probe.lock_calls, 0, "gate refusal takes no lock");
        assert!(probe.commits.is_empty(), "gate refusal commits nothing");
    }

    #[tokio::test]
    async fn via_evaluates_once_and_commits_content_free() {
        let activation = staging_activation(true, &["bannedword"]);
        let request = staging_request();
        let decoded =
            decode_interaction_options(&command_data(text_options("has bannedword here")));
        assert!(decoded.is_ok());
        let mut limiter = PreviewRateLimiter::new();
        let (store, probe) = FakeStore::fresh();
        let now_ms = 1_700_000_000_000;
        let answer = preview_attempt_via(
            Some(&activation),
            &mut limiter,
            &request,
            &decoded,
            now_ms,
            store,
        )
        .await;
        assert!(answer.evaluated, "fresh attempt evaluates exactly once");
        assert!(
            answer.reply.contains("TWO_AUTOMOD_BAD_WORDS"),
            "reply names the setting, not the content"
        );
        assert!(
            !answer.reply.contains("bannedword"),
            "reply never echoes content"
        );
        let receipt = format_iso_millis(i64::try_from(now_ms).expect("test clock fits"));
        assert!(
            answer.reply.contains(&receipt),
            "first reply renders the winning receipt instant"
        );
        let metric = answer.metric.expect("evaluated attempt emits a metric");
        assert_eq!(metric.result, "matched");
        assert_eq!(metric.filter, "bad_words");
        let probe = probe.lock().expect("probe is not poisoned");
        assert_eq!(probe.lock_calls, 1, "one claim, one read");
        assert_eq!(
            probe.commits,
            vec![("automod-preview:4242".to_owned(), receipt)],
            "one content-free commit under the claim id"
        );
    }

    #[tokio::test]
    async fn via_replays_a_committed_row_without_evaluating() {
        let activation = staging_activation(true, &["changed"]);
        let request = staging_request();
        let decoded =
            decode_interaction_options(&command_data(text_options("has bannedword here")));
        assert!(decoded.is_ok());
        // The committed row was written for a bad_words match under the old
        // policy; replay renders it byte-identical without evaluating.
        let created_at = "2026-11-02 08:00:00+00";
        let verdict = two_bot_core::automod_preview::PreviewVerdict {
            result: two_bot_core::automod_preview::PreviewResult::Matched,
            filter: Some(two_bot_core::automod::AutomodFilter::BadWords),
            knobs: two_bot_core::automod_preview::deciding_knobs(
                two_bot_core::automod::AutomodFilter::BadWords,
            ),
            exemption: None,
            dry_run: true,
        };
        let first = render_preview_reply(&verdict, created_at);
        let (mut store, probe) = FakeStore::fresh();
        store.row = Some(PreviewAuditRow {
            outcome: "matched".to_owned(),
            reason: Some("filter=bad_words;exemption=none;mode=dry_run;len=0_50".to_owned()),
            created_at: created_at.to_owned(),
        });
        let mut limiter = PreviewRateLimiter::new();
        let answer = preview_attempt_via(
            Some(&activation),
            &mut limiter,
            &request,
            &decoded,
            1_700_000_000_000,
            store,
        )
        .await;
        assert_eq!(answer.reply, first);
        assert_eq!(answer.metric, None, "replay carries no metric");
        assert!(!answer.evaluated, "replay evaluates nothing");
        let probe = probe.lock().expect("probe is not poisoned");
        assert!(probe.commits.is_empty(), "replay commits nothing");
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn begin_failure_answers_unavailable_with_the_effective_mode() {
        // A closed lazy pool makes any begin fail, without a database.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy test pool");
        pool.close().await;
        let activation = staging_activation(true, &["bannedword"]);
        let mut limiter = PreviewRateLimiter::new();
        let answer = handle_automod_preview(
            Some(&activation),
            &pool,
            &mut limiter,
            &preview_interaction(text_options("has bannedword here")),
            1_700_000_000_000,
        )
        .await;
        assert_eq!(answer.reply, PREVIEW_UNAVAILABLE_REPLY);
        assert!(!answer.evaluated);
        assert_eq!(answer.metric, Some(metric_unavailable(Some(true))));
    }

    #[tokio::test]
    async fn via_answers_unavailable_when_the_claim_fails() {
        let activation = staging_activation(true, &["bannedword"]);
        let request = staging_request();
        let decoded =
            decode_interaction_options(&command_data(text_options("has bannedword here")));
        assert!(decoded.is_ok());
        let mut limiter = PreviewRateLimiter::new();
        let (mut store, probe) = FakeStore::fresh();
        store.fail_claim = true;
        let answer = preview_attempt_via(
            Some(&activation),
            &mut limiter,
            &request,
            &decoded,
            1_700_000_000_000,
            store,
        )
        .await;
        assert_eq!(answer.reply, PREVIEW_UNAVAILABLE_REPLY);
        assert!(!answer.evaluated);
        assert_eq!(
            answer
                .metric
                .expect("suppressed verdict still emits")
                .result,
            "unavailable"
        );
        let probe = probe.lock().expect("probe is not poisoned");
        assert_eq!(probe.lock_calls, 1, "the claim was attempted");
        assert!(probe.commits.is_empty(), "no claim, no commit");
    }
}
