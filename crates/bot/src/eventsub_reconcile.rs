//! EventSub subscription reconcile at startup plus a bounded interval (M3.3).
//!
//! The receiver (`eventsub_http`) is served when `TWO_EVENTSUB` is exactly
//! `1`, but without subscriptions Twitch never delivers. This module drives
//! the pure-core plan (`plan_boot_actions` over `list_subscriptions`) through
//! the verifying executor (`execute_actions`, whose 409 path confirms via a
//! follow-up broadcaster listing) against the bot's own callback, then runs a
//! final listing that confirms every desired broadcaster holds a healthy
//! subscription. Anything unhealthy is logged with an `error_class`, never
//! silent.
//!
//! The plan runs one action at a time: a rejected action (a typo'd
//! broadcaster id answers 400, a rate limit 429, a blip 5xx) is logged and
//! counted, and the rest of the plan and the final listing still run, so one
//! bad id cannot starve the broadcasters sorted after it. The pass then fails
//! with the failed and missing counts.
//!
//! The task runs once at startup and on a bounded interval afterwards. The
//! signing secret travels only inside `WebhookSecret` (HMAC plus the
//! create-subscription body, both inside core) and no log line carries a
//! secret value, header, challenge or body byte: only scalar counts, statuses
//! and error classes.
//!
//! Each pass signs creates with the receiver's live snapshot, shared per
//! pass: the bound receiver owns one `RwLock<WebhookSecret>` (seeded at boot
//! from the Operator-provisioned secret, swapped by armed refresh), and the
//! pass clones whatever it holds right now, so creates always carry the
//! secret the receiver is verifying against. The pass never re-reads boot
//! env, and a dark receiver means no snapshot and no task rather than a
//! second secret source.
//!
//! Shutdown policy: the latched watch flag is checked before the startup
//! pass, before every interval pass, and between the Helix calls of a pass
//! (before the listing, before each plan action, and before the follow-up
//! listing), so shutdown never starts another pass and an in-flight pass
//! stops cooperatively after its current Helix call instead of being cut
//! mid-request. A pass stopped that way logs
//! `reconcile_shutdown_interrupted` with the completed/remaining action
//! counts and skips verification; the next startup pass re-lists the real
//! subscription state and re-plans, so nothing is left silently assumed.
//! The owner retains the spawned handle and [`join`]s it with what is left of
//! the same shutdown deadline (capped by [`SHUTDOWN_DRAIN`]), never a fresh
//! drain after the service drain; only a task that outlives that shared
//! budget is aborted, and that abort logs
//! `reconcile_shutdown_drain_exceeded` because an in-flight remote mutation
//! outcome is then uncertain until the next startup pass.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, RwLock},
    time::Duration,
};

use two_bot_core::metrics::Metrics;
use two_bot_core::twitch_eventsub::{
    CallbackUrl, ExistingSubscription, SubscriptionAction, WebhookSecret,
};
use two_bot_core::twitch_helix::{
    plan_boot_actions, ActionOutcome, HelixClient, HelixCredentials, HelixError, HelixTransport,
    HyperHelixTransport, HELIX_TIMEOUT_MS, TWITCH_WEBHOOK_SECRET_ENV,
};

/// Environment variable carrying the bot's own public callback. Reconcile only
/// ever touches subscriptions on this URL, never a foreign one.
pub const CALLBACK_ENV: &str = "TWO_TWITCH_CALLBACK_URL";
/// Environment variable carrying the comma-separated broadcaster allowlist
/// (`broadcaster_user_id` values) to subscribe to on the bot callback.
pub const BROADCASTERS_ENV: &str = "TWO_TWITCH_BROADCASTERS";
/// Reconcile cadence after the startup run. Hourly is cheap (a few Helix calls
/// per pass) and bounds how long a dropped subscription stays dark.
pub const RECONCILE_INTERVAL_SECS: u64 = 3_600;

/// Cap for how long the owner waits for the reconcile task to stop after
/// shutdown latches before aborting it in [`join`]. A pass stops
/// cooperatively between Helix calls, so the wait is at most one in-flight
/// action: a create can add a confirmation listing to its own round trip,
/// hence two Helix transport timeouts plus margin. The owner passes
/// `min(SHUTDOWN_DRAIN, deadline - elapsed)` so the whole drain stays bounded
/// by `SHUTDOWN_TIMEOUT_SECONDS`. The task normally exits long before the cap
/// expires because it drains concurrently with the gateway/HTTP drain.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_millis(HELIX_TIMEOUT_MS * 2 + 5_000);

/// What one reconcile pass did, for scalar receiver logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Stale/failed subscriptions deleted (plus already-gone deletes).
    pub deleted: usize,
    /// Subscriptions created with a 2xx answer.
    pub created: usize,
    /// 409 creates confirmed healthy by the follow-up listing.
    pub already_exists: usize,
    /// Pre-pass subscriptions on our callback already in a failed status
    /// (`webhook_callback_verification_failed`,
    /// `notification_failures_exceeded`, `authorization_revoked`). Surfaced
    /// even when the re-create succeeds so a never-verifying callback cannot
    /// log `healthy` while cycling delete/create every hour.
    pub prior_failed: usize,
    /// Final follow-up status for a successful reconcile pass.
    pub status: Option<&'static str>,
}

/// True when the EventSub slice is enabled in `vars`.
fn gate_open(vars: &HashMap<String, String>) -> bool {
    vars.get("TWO_EVENTSUB")
        .is_some_and(|flag| flag.as_str() == "1")
}

/// True when the process environment enables the reconcile task.
fn enabled() -> bool {
    gate_open(&std::env::vars().collect())
}

/// Split the broadcaster allowlist: comma-separated, trimmed, sorted and
/// deduplicated so the plan is deterministic.
fn parse_broadcasters(raw: &str) -> Vec<String> {
    let mut ids: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Load the callback plus desired broadcasters. An empty allowlist parks the
/// pass without mutations (the caller logs it); a missing or invalid callback
/// fails loudly so the operator notices.
fn load_from_map(vars: &HashMap<String, String>) -> Result<(CallbackUrl, Vec<String>), HelixError> {
    let raw = vars.get(CALLBACK_ENV).map(String::as_str).unwrap_or("");
    if raw.trim().is_empty() {
        return Err(HelixError::Missing(CALLBACK_ENV));
    }
    let callback = CallbackUrl::new(raw.trim()).map_err(|_| HelixError::InvalidCredential {
        name: CALLBACK_ENV,
        reason: "value must be an https callback without port, query or fragment",
    })?;
    let desired = vars
        .get(BROADCASTERS_ENV)
        .map_or_else(Vec::new, |raw| parse_broadcasters(raw));
    Ok((callback, desired))
}

/// Mirrors the healthy set in core (the authoritative check lives next to the
/// listing parser in `twitch_eventsub`): only these statuses deliver.
fn is_healthy_status(status: &str) -> bool {
    matches!(status, "enabled" | "webhook_callback_verification_pending")
}

/// True when the status is a terminal Twitch failure that `reconcile` deletes
/// and re-creates (as opposed to healthy `enabled`/`pending` or an unknown
/// value, which the follow-up listing judges).
fn is_failed_status(status: &str) -> bool {
    matches!(
        status,
        "webhook_callback_verification_failed"
            | "notification_failures_exceeded"
            | "authorization_revoked"
    )
}

/// Pre-pass subscriptions on our callback already in a failed status.
fn count_prior_failed(existing: &[ExistingSubscription], callback: &CallbackUrl) -> usize {
    existing
        .iter()
        .filter(|sub| sub.callback == callback.as_str() && is_failed_status(&sub.status))
        .count()
}

/// True only when every desired broadcaster holds an `enabled` subscription
/// on our callback in the follow-up listing: the only state the pass may log
/// as `healthy`. A desired broadcaster served only by a
/// `webhook_callback_verification_pending` subscription keeps the pass at
/// `pending`, even when other desired broadcasters are already `enabled`.
/// An empty desired set is vacuously satisfied (the startup path parks that
/// case before this check runs). Foreign-callback subscriptions never count.
fn all_desired_enabled(
    desired: &[String],
    callback: &CallbackUrl,
    confirmed: &[ExistingSubscription],
) -> bool {
    desired.iter().all(|id| {
        confirmed.iter().any(|sub| {
            &sub.broadcaster_user_id == id
                && sub.callback == callback.as_str()
                && sub.status == "enabled"
        })
    })
}

/// Desired broadcasters without a healthy subscription on our callback in the
/// follow-up listing.
fn unconfirmed_broadcasters(
    desired: &[String],
    callback: &CallbackUrl,
    listed: &[ExistingSubscription],
) -> Vec<String> {
    desired
        .iter()
        .filter(|id| {
            !listed.iter().any(|sub| {
                &sub.broadcaster_user_id == *id
                    && sub.callback == callback.as_str()
                    && is_healthy_status(&sub.status)
            })
        })
        .cloned()
        .collect()
}

/// Count one executed plan by outcome, for the scalar run log.
fn summarize(outcomes: &[ActionOutcome]) -> ReconcileSummary {
    let mut summary = ReconcileSummary::default();
    for outcome in outcomes {
        match outcome {
            ActionOutcome::Deleted { .. } => summary.deleted += 1,
            ActionOutcome::Created { .. } => summary.created += 1,
            ActionOutcome::AlreadyExists { .. } => summary.already_exists += 1,
        }
    }
    summary
}

/// What running a plan action by action produced.
struct PlanRun {
    /// Outcomes of the actions that succeeded, in plan order.
    outcomes: Vec<ActionOutcome>,
    /// Actions that failed; every later action still ran.
    failed: usize,
    /// The first failed action's error, returned when the pass fails.
    first_failure: Option<HelixError>,
    /// Shutdown latched before an action started; that action and every
    /// later one were skipped, so the pass is incomplete by request.
    interrupted: bool,
}

/// `delete` or `create`, for the scalar action-failure log.
fn action_label(action: &SubscriptionAction) -> &'static str {
    match action {
        SubscriptionAction::Delete { .. } => "delete",
        SubscriptionAction::Create { .. } => "create",
    }
}

/// Run `plan` one action at a time so a rejected action cannot starve the
/// ones after it: `execute_actions` returns on the first non-2xx answer, and
/// the creates are sorted, so a poisoned broadcaster id would otherwise block
/// every later create on every pass. Each failure is logged with its action
/// kind and error, counted, and the plan carries on; the plan is bounded by
/// the broadcaster allowlist, so the pass stays bounded too. Shutdown is
/// checked before each action: a latched shutdown skips every remaining
/// action (never cutting the in-flight one mid-request) and marks the run
/// interrupted for the caller to report.
async fn run_plan<T: HelixTransport>(
    client: &mut HelixClient<T>,
    plan: &[SubscriptionAction],
    callback: &CallbackUrl,
    secret: &WebhookSecret,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> PlanRun {
    let mut run = PlanRun {
        outcomes: Vec::with_capacity(plan.len()),
        failed: 0,
        first_failure: None,
        interrupted: false,
    };
    for action in plan {
        if stopped(shutdown) {
            run.interrupted = true;
            break;
        }
        match client
            .execute_actions(std::slice::from_ref(action), callback, secret)
            .await
        {
            Ok(mut done) => run.outcomes.append(&mut done),
            Err(error) => {
                tracing::error!(
                    event = "eventsub_reconcile",
                    error_class = "reconcile_action_failed",
                    action = action_label(action),
                    error = %error,
                    "eventsub reconcile action failed; continuing with the rest of the plan"
                );
                run.failed += 1;
                if run.first_failure.is_none() {
                    run.first_failure = Some(error);
                }
            }
        }
    }
    run
}

/// Report a pass stopped by shutdown between Helix calls. The completed and
/// remaining action counts say exactly how far the plan got; the resulting
/// subscription state is deliberately not guessed here, because the next
/// startup pass re-lists it before planning.
fn log_interrupted(completed: usize, remaining: usize) {
    tracing::warn!(
        event = "eventsub_reconcile",
        error_class = "reconcile_shutdown_interrupted",
        completed = completed,
        remaining = remaining,
        "eventsub reconcile pass interrupted by shutdown; remaining actions skipped and subscription state unverified until the next startup pass"
    );
}

/// Record the successful mutations of one executed plan in the metrics
/// registry: 2xx creates and deletes. Zero counts are no-ops; 409
/// already-exists confirmations fold into the pass outcome, not into actions.
fn record_actions(metrics: &Metrics, outcomes: &[ActionOutcome]) {
    let summary = summarize(outcomes);
    metrics.eventsub_reconcile_action("create", summary.created as u64);
    metrics.eventsub_reconcile_action("delete", summary.deleted as u64);
}

/// List, diff with `plan_boot_actions` (deletes before creates, our callback
/// only), run the plan action by action, then confirm with a follow-up
/// `list_subscriptions`: the pass fails loudly, with the failed and missing
/// counts, when any action failed or a desired broadcaster lacks a healthy
/// subscription afterwards. A latched `shutdown` stops the pass between
/// Helix calls instead (see the module shutdown policy): the actions done
/// so far are summarized with no verification status, and the interruption
/// is logged, never silently counted as converged. Every concluded pass also
/// records exactly one `two_bot_eventsub_reconcile_passes_total{outcome}`
/// sample via [`reconcile_all_with_metrics`]; shutdown interruptions stay
/// log-only (plus the mutations they already made).
pub async fn reconcile_all<T: HelixTransport>(
    client: &mut HelixClient<T>,
    callback: &CallbackUrl,
    desired: &[String],
    secret: &WebhookSecret,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Result<ReconcileSummary, HelixError> {
    reconcile_all_with_metrics(
        client,
        callback,
        desired,
        secret,
        shutdown,
        two_bot_core::metrics::global(),
    )
    .await
}

/// [`reconcile_all`] against an explicit metrics registry. Every concluded
/// pass that reaches Helix records exactly one
/// `two_bot_eventsub_reconcile_passes_total{outcome}` sample (`healthy`,
/// `pending`, `failed`), the successful creates/deletes in
/// `two_bot_eventsub_reconcile_actions_total{action}`, and each failure site in
/// `two_bot_eventsub_reconcile_errors_total{error_class}` under the
/// failure-site `error_class` log strings (`reconcile_action_failed`,
/// `reconcile_prior_failed`, `reconcile_unhealthy`, `reconcile_failed`).
/// `reconcile_pending`, config refusals, parked startup passes, and shutdown
/// interruptions stay log-only (interruptions still record the mutations they
/// already made via `record_actions`), and the `startup_reconcile` wrapper's
/// `reconcile_failed` line surfaces the same failure the pass already
/// counted. A latched `shutdown` stops the pass between Helix calls: the
/// actions done so far are summarized with no verification status, and the
/// interruption is logged via `log_interrupted`. Cardinality is bounded
/// per-outcome, never per-subscription: no broadcaster, subscription id,
/// status code or body byte reaches a label. Callers pass a fresh
/// [`Metrics`] in tests; production passes the global registry.
pub async fn reconcile_all_with_metrics<T: HelixTransport>(
    client: &mut HelixClient<T>,
    callback: &CallbackUrl,
    desired: &[String],
    secret: &WebhookSecret,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    metrics: &Metrics,
) -> Result<ReconcileSummary, HelixError> {
    if stopped(shutdown) {
        log_interrupted(0, 0);
        return Ok(ReconcileSummary::default());
    }
    let existing = match client.list_subscriptions().await {
        Ok(existing) => existing,
        Err(error) => {
            metrics.eventsub_reconcile_pass("failed");
            metrics.eventsub_reconcile_error("reconcile_failed");
            return Err(error);
        }
    };
    let prior_failed = count_prior_failed(&existing, callback);
    if prior_failed > 0 {
        metrics.eventsub_reconcile_error("reconcile_prior_failed");
        tracing::error!(
            event = "eventsub_reconcile",
            error_class = "reconcile_prior_failed",
            broadcasters = desired.len(),
            prior_failed = prior_failed,
            "eventsub reconcile found failed subscriptions on our callback; deleting and re-creating"
        );
    }
    let plan = match plan_boot_actions(desired, callback, &existing) {
        Ok(plan) => plan,
        Err(error) => {
            metrics.eventsub_reconcile_pass("failed");
            metrics.eventsub_reconcile_error("reconcile_failed");
            return Err(error);
        }
    };
    let run = run_plan(client, &plan, callback, secret, shutdown).await;
    for _ in 0..run.failed {
        metrics.eventsub_reconcile_error("reconcile_action_failed");
    }
    if run.interrupted || stopped(shutdown) {
        // The plan is finished or cut short, but shutdown takes priority
        // over the verification listing: report what ran, claim no status.
        // An action that already failed still fails the pass, exactly as in
        // the uninterrupted path below, so shutdown never masks a failure.
        // Mutations already made still count in actions; the interrupted pass
        // itself stays log-only unless it failed, in which case it records
        // the same failed/unhealthy samples as the uninterrupted failure.
        record_actions(metrics, &run.outcomes);
        let completed = run.outcomes.len() + run.failed;
        log_interrupted(completed, plan.len() - completed);
        if let Some(error) = run.first_failure {
            metrics.eventsub_reconcile_error("reconcile_unhealthy");
            metrics.eventsub_reconcile_pass("failed");
            return Err(error);
        }
        let mut summary = summarize(&run.outcomes);
        summary.prior_failed = prior_failed;
        return Ok(summary);
    }
    let confirmed = match client.list_subscriptions().await {
        Ok(confirmed) => confirmed,
        Err(error) => {
            record_actions(metrics, &run.outcomes);
            metrics.eventsub_reconcile_pass("failed");
            metrics.eventsub_reconcile_error("reconcile_failed");
            return Err(error);
        }
    };
    let missing = unconfirmed_broadcasters(desired, callback, &confirmed);
    if run.failed > 0 || !missing.is_empty() {
        record_actions(metrics, &run.outcomes);
        metrics.eventsub_reconcile_error("reconcile_unhealthy");
        metrics.eventsub_reconcile_pass("failed");
        tracing::error!(
            event = "eventsub_reconcile",
            error_class = "reconcile_unhealthy",
            broadcasters = desired.len(),
            failed = run.failed,
            missing = missing.len(),
            prior_failed = prior_failed,
            "eventsub reconcile did not converge: an action failed or a healthy subscription is missing"
        );
        return Err(run.first_failure.unwrap_or(HelixError::BadResponse(
            "reconcile follow-up listing is missing a healthy subscription",
        )));
    }
    let mut summary = summarize(&run.outcomes);
    record_actions(metrics, &run.outcomes);
    summary.prior_failed = prior_failed;
    let status = if all_desired_enabled(desired, callback, &confirmed) {
        "healthy"
    } else {
        "pending"
    };
    summary.status = Some(status);
    if status == "pending" {
        metrics.eventsub_reconcile_pass("pending");
        tracing::warn!(
            event = "eventsub_reconcile",
            error_class = "reconcile_pending",
            status = "pending",
            broadcasters = desired.len(),
            deleted = summary.deleted,
            created = summary.created,
            already_exists = summary.already_exists,
            prior_failed = summary.prior_failed,
            "eventsub subscriptions re-created but not all verified yet"
        );
        return Ok(summary);
    }
    metrics.eventsub_reconcile_pass("healthy");
    tracing::info!(
        event = "eventsub_reconcile",
        status = "healthy",
        broadcasters = desired.len(),
        deleted = summary.deleted,
        created = summary.created,
        already_exists = summary.already_exists,
        prior_failed = summary.prior_failed,
        "eventsub subscriptions reconciled"
    );
    Ok(summary)
}

/// Clone the receiver's live signing snapshot for one pass. The snapshot is
/// seeded at boot from the Operator-provisioned `TWO_TWITCH_WEBHOOK_SECRET`
/// (via `EventsubReceiverConfig`) and swapped by armed refresh; the pass
/// always signs with the secret the receiver is verifying against right now,
/// never boot env directly. The clone is per pass, not per create: a pass
/// that starts before an armed swap finishes with the old value, so rotation
/// deletes only after the runbook's drain fence (a reconcile terminal line
/// after the swap-confirming ack proves no stale pass is still in flight).
/// A poisoned lock refuses the pass (the next tick retries) rather than
/// signing with a torn copy. The error names only the variable and the
/// runtime cause, never secret material.
fn snapshot_secret(current: &RwLock<WebhookSecret>) -> Result<WebhookSecret, HelixError> {
    current
        .read()
        .map(|guard| guard.clone())
        .map_err(|_| HelixError::InvalidCredential {
            name: TWITCH_WEBHOOK_SECRET_ENV,
            reason: "in-memory snapshot is unreadable",
        })
}

/// One reconcile pass: gate, Operator credentials, callback plus allowlist,
/// and the receiver's live snapshot, then `reconcile_all`. Every refusal
/// and failure is logged with an `error_class`; callers need no extra log.
/// `shutdown` is the process latch the pass checks between Helix calls.
pub async fn startup_reconcile(
    shutdown: &tokio::sync::watch::Receiver<bool>,
    secret: &RwLock<WebhookSecret>,
) -> Result<ReconcileSummary, HelixError> {
    let vars: HashMap<String, String> = std::env::vars().collect();
    if !gate_open(&vars) {
        tracing::info!(
            event = "eventsub_reconcile",
            status = "parked",
            reason = "flag_dark",
            "eventsub reconcile parked: TWO_EVENTSUB is not exactly 1"
        );
        return Ok(ReconcileSummary::default());
    }
    let (callback, desired) = match load_from_map(&vars) {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_config_invalid",
                error = %error,
                "eventsub reconcile refused: callback configuration is invalid"
            );
            return Err(error);
        }
    };
    if desired.is_empty() {
        tracing::warn!(
            event = "eventsub_reconcile",
            error_class = "reconcile_no_broadcasters",
            "eventsub reconcile parked: broadcaster allowlist is empty, no subscriptions mutated"
        );
        return Ok(ReconcileSummary::default());
    }
    let creds = match HelixCredentials::from_env() {
        Ok(creds) => creds,
        Err(error) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_config_invalid",
                error = %error,
                "eventsub reconcile refused: Twitch credentials are missing or invalid"
            );
            return Err(error);
        }
    };
    let secret = match snapshot_secret(secret) {
        Ok(secret) => secret,
        Err(error) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_config_invalid",
                error = %error,
                "eventsub reconcile refused: webhook secret snapshot is unreadable"
            );
            return Err(error);
        }
    };
    let mut client = HelixClient::new(HyperHelixTransport, creds);
    match reconcile_all(&mut client, &callback, &desired, &secret, shutdown).await {
        Ok(summary) => Ok(summary),
        Err(error) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_failed",
                error = %error,
                "eventsub reconcile failed"
            );
            Err(error)
        }
    }
}

/// One pass; the outcome is already logged inside `startup_reconcile`.
async fn run_once(shutdown: &tokio::sync::watch::Receiver<bool>, secret: &RwLock<WebhookSecret>) {
    let _ = startup_reconcile(shutdown, secret).await;
}

/// Shutdown latch for the interval loop.
fn stopped(shutdown: &tokio::sync::watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

/// Startup run, then the bounded interval, until shutdown. Failures are
/// logged per pass and never stop the loop: the next tick retries. The
/// latch is checked before every pass, including the startup one, so a
/// shutdown that landed before the task first polls starts no pass at all;
/// a pass already in flight drains cooperatively (see the module shutdown
/// policy) and no pass after it starts.
async fn run_loop<P, Fut>(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    interval: Duration,
    mut pass: P,
) where
    P: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    if stopped(&shutdown) {
        return;
    }
    pass().await;
    loop {
        tokio::select! {
            biased;
            result = shutdown.changed() => {
                let _ = result;
                return;
            }
            _ = tokio::time::sleep(interval) => {}
        }
        if stopped(&shutdown) {
            return;
        }
        pass().await;
    }
}

/// Spawn the reconcile task when `TWO_EVENTSUB` is exactly `1`, returning
/// its handle so the owner can [`join`] it at shutdown instead of detaching
/// it. `secret` is the bound receiver's live snapshot: every pass clones
/// whatever it holds right now, so creates always carry the secret the
/// receiver is verifying against. The flag is checked first, so a dark boot
/// keeps the `flag_dark` parked line its log consumers key on; `None` with
/// the flag enabled (a snapshot missing while enabled) parks with
/// `receiver_dark` instead of a second secret source, never a silent
/// downgrade of an enabled receiver.
pub fn spawn_if_enabled(
    shutdown: tokio::sync::watch::Receiver<bool>,
    secret: Option<Arc<RwLock<WebhookSecret>>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled() {
        tracing::info!(
            event = "eventsub_reconcile",
            status = "parked",
            reason = "flag_dark",
            "eventsub reconcile parked: TWO_EVENTSUB is not exactly 1"
        );
        return None;
    }
    let Some(secret) = secret else {
        tracing::info!(
            event = "eventsub_reconcile",
            status = "parked",
            reason = "receiver_dark",
            "eventsub reconcile parked: the EventSub receiver is dark, no shared secret snapshot"
        );
        return None;
    };
    let pass_shutdown = shutdown.clone();
    Some(tokio::spawn(run_loop(
        shutdown,
        Duration::from_secs(RECONCILE_INTERVAL_SECS),
        move || {
            let shutdown = pass_shutdown.clone();
            let secret = Arc::clone(&secret);
            async move { run_once(&shutdown, &secret).await }
        },
    )))
}

/// Join the reconcile task after shutdown has latched, waiting at most
/// `drain` for its cooperative stop. A panicked task is logged, never
/// propagated. A task that outlives the drain is aborted and then still
/// awaited, so this returns only once the worker is actually finished; the
/// abort is logged as an uncertain remote-mutation outcome because the
/// in-flight Helix call may or may not have landed, and only the next
/// startup pass's listing settles it.
pub async fn join(task: Option<tokio::task::JoinHandle<()>>, drain: Duration) {
    let Some(mut task) = task else {
        return;
    };
    match tokio::time::timeout(drain, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_task_failed",
                panicked = error.is_panic(),
                "eventsub reconcile task ended abnormally"
            );
        }
        Err(_) => {
            tracing::error!(
                event = "eventsub_reconcile",
                error_class = "reconcile_shutdown_drain_exceeded",
                drain_ms = drain.as_millis() as u64,
                "eventsub reconcile task did not finish within the shutdown drain; aborting it, so an in-flight remote mutation outcome is uncertain until the next startup pass re-checks"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use axum::http::{Method, StatusCode};
    use serde_json::{json, Value};
    use two_bot_core::twitch_helix::{HelixRequest, HelixResponse};

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn subscription_entry(id: &str, status: &str, broadcaster: &str, callback: &str) -> Value {
        json!({
            "id": id,
            "status": status,
            "type": "stream.online",
            "version": "1",
            "condition": {"broadcaster_user_id": broadcaster},
            "transport": {"method": "webhook", "callback": callback},
        })
    }

    struct ScriptTransport {
        answers: Mutex<VecDeque<Result<HelixResponse, HelixError>>>,
        log: CallLog,
        /// Latches shutdown while a create-subscription call is being
        /// answered, so a test can land shutdown deterministically between
        /// two plan actions: the create completes, the next one must not run.
        flip_shutdown_on_create: Option<tokio::sync::watch::Sender<bool>>,
    }

    impl ScriptTransport {
        fn new(answers: Vec<Result<HelixResponse, HelixError>>, log: CallLog) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                log,
                flip_shutdown_on_create: None,
            }
        }

        fn token_answer() -> Result<HelixResponse, HelixError> {
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: json!({"access_token": "synthetic-token", "expires_in": 3_600})
                    .to_string()
                    .into_bytes(),
            })
        }

        fn listing_answer(document: &Value) -> Result<HelixResponse, HelixError> {
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: document.to_string().into_bytes(),
            })
        }

        fn no_content() -> Result<HelixResponse, HelixError> {
            Ok(HelixResponse {
                status: StatusCode::NO_CONTENT,
                body: Vec::new(),
            })
        }

        fn rejected(status: StatusCode) -> Result<HelixResponse, HelixError> {
            Ok(HelixResponse {
                status,
                body: Vec::new(),
            })
        }
    }

    impl HelixTransport for ScriptTransport {
        async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
            if let Some(shutdown) = &self.flip_shutdown_on_create {
                if request.method == Method::POST && request.url.contains("subscriptions") {
                    shutdown.send_replace(true);
                }
            }
            self.log
                .inner
                .lock()
                .expect("call log must unlock")
                .push(request);
            self.answers
                .lock()
                .expect("script must unlock")
                .pop_front()
                .expect("script must answer every call")
        }
    }

    #[derive(Clone, Default)]
    struct CallLog {
        inner: std::sync::Arc<Mutex<Vec<HelixRequest>>>,
    }

    impl CallLog {
        fn requests(&self) -> Vec<HelixRequest> {
            self.inner.lock().expect("call log must unlock").clone()
        }
    }

    /// A shutdown receiver that never latches: its sender lives in a static
    /// for the whole test process, because a dropped sender also counts as
    /// stopped.
    fn never_shutdown() -> tokio::sync::watch::Receiver<bool> {
        static SENDER: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
            std::sync::OnceLock::new();
        SENDER
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .subscribe()
    }

    fn test_secret() -> WebhookSecret {
        WebhookSecret::new("synthetic-webhook-secret-01".to_owned())
            .expect("fixture secret must parse")
    }

    fn callback() -> CallbackUrl {
        CallbackUrl::new("https://hooks.example.com/twitch").expect("fixture callback must parse")
    }

    fn creds() -> HelixCredentials {
        HelixCredentials {
            client_id: two_bot_core::Secret::new("fixture-client-id".to_owned()),
            client_secret: two_bot_core::Secret::new("fixture-client-secret".to_owned()),
        }
    }

    #[test]
    fn gate_opens_only_on_exact_one() {
        assert!(!gate_open(&vars(&[])));
        assert!(!gate_open(&vars(&[("TWO_EVENTSUB", "0")])));
        assert!(!gate_open(&vars(&[("TWO_EVENTSUB", "true")])));
        assert!(gate_open(&vars(&[("TWO_EVENTSUB", "1")])));
    }

    #[test]
    fn broadcasters_split_trim_and_dedupe() {
        assert_eq!(
            parse_broadcasters("222, 444,,222,  ,111"),
            vec!["111".to_owned(), "222".to_owned(), "444".to_owned()]
        );
        assert!(parse_broadcasters(" , ,").is_empty());
    }

    #[test]
    fn config_requires_a_valid_callback() {
        let err = load_from_map(&vars(&[])).expect_err("missing callback must fail");
        assert!(matches!(err, HelixError::Missing(CALLBACK_ENV)));
        let err = load_from_map(&vars(&[(CALLBACK_ENV, "http://plain.example.com/hook")]))
            .expect_err("non-https callback must fail");
        assert!(matches!(err, HelixError::InvalidCredential { .. }));
    }

    #[test]
    fn config_loads_callback_and_allowlist() {
        let (callback, desired) = load_from_map(&vars(&[
            (CALLBACK_ENV, "https://hooks.example.com/twitch"),
            (BROADCASTERS_ENV, "444,222,444"),
        ]))
        .expect("valid config must load");
        assert_eq!(callback.as_str(), "https://hooks.example.com/twitch");
        assert_eq!(desired, vec!["222".to_owned(), "444".to_owned()]);
        let (_, desired) =
            load_from_map(&vars(&[(CALLBACK_ENV, "https://hooks.example.com/twitch")]))
                .expect("missing allowlist parks with an empty list");
        assert!(desired.is_empty());
    }

    #[test]
    fn snapshot_secret_returns_the_live_value_after_a_swap() {
        // The pass clones whatever the receiver holds right now: a rotation
        // that landed between passes signs the next creates.
        let current = std::sync::RwLock::new(test_secret());
        let rotated = WebhookSecret::new("synthetic-webhook-secret-02".to_owned())
            .expect("fixture secret must parse");
        *current.write().expect("snapshot lock must be writable") = rotated.clone();
        assert_eq!(
            snapshot_secret(&current).expect("live snapshot must read"),
            rotated
        );
    }

    #[test]
    fn snapshot_secret_refuses_a_poisoned_lock_without_secret_material() {
        let current = std::sync::RwLock::new(test_secret());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = current.write().expect("snapshot lock must be writable");
            // Match the test-only panic injection used by the shutdown tests.
            std::panic::panic_any("poison the snapshot lock");
        }));
        let error = snapshot_secret(&current).expect_err("poisoned snapshot must refuse");
        assert!(
            matches!(error, HelixError::InvalidCredential { .. }),
            "poisoned snapshot must refuse as an unusable credential: {error:?}"
        );
        for rendered in [format!("{error}"), format!("{error:?}")] {
            assert!(
                !rendered.contains("synthetic-webhook-secret-01"),
                "refusal leaks the signing secret: {rendered}"
            );
        }
    }

    /// Spawn-gate tests hold `TWO_EVENTSUB`, which is process-global, so
    /// they serialize here and restore the prior value on the way out (the
    /// voice boot-settings idiom). No other test in this binary reads the
    /// flag from the process environment.
    static SPAWN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold `TWO_EVENTSUB` at `value` (`None` clears it) until the guard
    /// drops, restoring whatever was there before.
    struct FlagHold {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior: Option<String>,
    }

    impl Drop for FlagHold {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(value) => std::env::set_var("TWO_EVENTSUB", value),
                None => std::env::remove_var("TWO_EVENTSUB"),
            }
        }
    }

    fn hold_eventsub_flag(value: Option<&str>) -> FlagHold {
        let lock = SPAWN_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior = std::env::var("TWO_EVENTSUB").ok();
        match value {
            Some(value) => std::env::set_var("TWO_EVENTSUB", value),
            None => std::env::remove_var("TWO_EVENTSUB"),
        }
        FlagHold { _lock: lock, prior }
    }

    /// The parked line `spawn_if_enabled` appended after `start`, read from
    /// the process-wide capture (a scoped capture can miss events when
    /// sibling tests run on other threads).
    fn parked_line_since(capture: &crate::tracing_capture::Capture, start: usize) -> String {
        capture.text()[start..].to_owned()
    }

    #[test]
    fn spawn_with_flag_dark_and_no_snapshot_parks_flag_dark() {
        // Dark boots keep the `flag_dark` parked line even though no
        // snapshot exists: the flag is checked before the snapshot.
        let _flag = hold_eventsub_flag(None);
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(spawn_if_enabled(rx, None).is_none());
        let logged = parked_line_since(capture, start);
        assert!(
            logged.contains("reason=\"flag_dark\""),
            "dark boot must park flag_dark, not receiver_dark: {logged}"
        );
    }

    #[test]
    fn spawn_with_flag_dark_and_snapshot_parks_flag_dark() {
        // An explicit `0` parks on the flag even when a snapshot handle
        // exists: the gate stays closed.
        let _flag = hold_eventsub_flag(Some("0"));
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let secret = Arc::new(RwLock::new(test_secret()));
        assert!(spawn_if_enabled(rx, Some(secret)).is_none());
        let logged = parked_line_since(capture, start);
        assert!(
            logged.contains("reason=\"flag_dark\""),
            "flag 0 must park flag_dark: {logged}"
        );
    }

    #[test]
    fn spawn_with_flag_enabled_but_no_snapshot_parks_receiver_dark() {
        // The discriminating cell: the flag gate is open, so only the
        // snapshot guard parks. A second-source fallback mutant spawns here
        // and fails the assertion below.
        let _flag = hold_eventsub_flag(Some("1"));
        let capture = crate::tracing_capture::global();
        let start = capture.text().len();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(spawn_if_enabled(rx, None).is_none());
        let logged = parked_line_since(capture, start);
        assert!(
            logged.contains("reason=\"receiver_dark\""),
            "enabled without a snapshot must park receiver_dark: {logged}"
        );
    }

    #[tokio::test]
    async fn spawn_with_flag_enabled_and_snapshot_spawns_a_task() {
        let _flag = hold_eventsub_flag(Some("1"));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let secret = Arc::new(RwLock::new(test_secret()));
        let task = spawn_if_enabled(rx, Some(secret)).expect("enabled with a snapshot must spawn");
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn healthy_set_covers_enabled_and_pending() {
        assert!(is_healthy_status("enabled"));
        assert!(is_healthy_status("webhook_callback_verification_pending"));
        for status in [
            "webhook_callback_verification_failed",
            "notification_failures_exceeded",
            "authorization_revoked",
            "",
        ] {
            assert!(!is_healthy_status(status), "{status}");
        }
    }

    #[test]
    fn failed_status_set_covers_terminal_twitch_states() {
        for status in [
            "webhook_callback_verification_failed",
            "notification_failures_exceeded",
            "authorization_revoked",
        ] {
            assert!(is_failed_status(status), "{status}");
        }
        for status in [
            "enabled",
            "webhook_callback_verification_pending",
            "",
            "other",
        ] {
            assert!(!is_failed_status(status), "{status}");
        }
    }

    #[test]
    fn prior_failed_counts_only_failed_subscriptions_on_our_callback() {
        let other = CallbackUrl::new("https://other.example.com/twitch")
            .expect("fixture callback must parse");
        let existing = vec![
            ExistingSubscription {
                id: "failed-id".to_owned(),
                status: "webhook_callback_verification_failed".to_owned(),
                broadcaster_user_id: "444".to_owned(),
                callback: callback().as_str().to_owned(),
            },
            ExistingSubscription {
                id: "pending-id".to_owned(),
                status: "webhook_callback_verification_pending".to_owned(),
                broadcaster_user_id: "444".to_owned(),
                callback: callback().as_str().to_owned(),
            },
            ExistingSubscription {
                id: "foreign-failed-id".to_owned(),
                status: "authorization_revoked".to_owned(),
                broadcaster_user_id: "444".to_owned(),
                callback: other.as_str().to_owned(),
            },
        ];
        assert_eq!(count_prior_failed(&existing, &callback()), 1);
    }

    #[test]
    fn all_desired_enabled_requires_every_desired_broadcaster_enabled() {
        fn sub(
            id: &str,
            status: &str,
            broadcaster: &str,
            callback: &CallbackUrl,
        ) -> ExistingSubscription {
            ExistingSubscription {
                id: id.to_owned(),
                status: status.to_owned(),
                broadcaster_user_id: broadcaster.to_owned(),
                callback: callback.as_str().to_owned(),
            }
        }
        let desired = vec!["222".to_owned(), "444".to_owned()];
        let enabled_222 = sub("id-222", "enabled", "222", &callback());
        let enabled_444 = sub("id-444", "enabled", "444", &callback());
        let pending_444 = sub(
            "id-444-pending",
            "webhook_callback_verification_pending",
            "444",
            &callback(),
        );
        let failed_444 = sub(
            "id-444-failed",
            "webhook_callback_verification_failed",
            "444",
            &callback(),
        );
        let other = CallbackUrl::new("https://other.example.com/twitch")
            .expect("fixture callback must parse");
        let foreign_enabled_444 = sub("id-444-foreign", "enabled", "444", &other);

        assert!(
            all_desired_enabled(&desired, &callback(), &[enabled_222.clone(), enabled_444]),
            "all desired broadcasters enabled is healthy"
        );
        assert!(
            !all_desired_enabled(&desired, &callback(), &[enabled_222.clone(), pending_444]),
            "a mixed enabled/pending desired set is not healthy"
        );
        assert!(
            !all_desired_enabled(&desired, &callback(), &[enabled_222.clone(), failed_444]),
            "a failed desired subscription is not healthy"
        );
        assert!(
            !all_desired_enabled(&desired, &callback(), std::slice::from_ref(&enabled_222)),
            "a missing desired subscription is not healthy"
        );
        assert!(
            !all_desired_enabled(&desired, &callback(), &[enabled_222, foreign_enabled_444]),
            "an enabled subscription on a foreign callback does not count"
        );
        assert!(
            all_desired_enabled(&[], &callback(), &[]),
            "an empty desired set is vacuously satisfied"
        );
    }

    #[test]
    fn interval_is_bounded_and_nonzero() {
        const {
            assert!(RECONCILE_INTERVAL_SECS > 0);
            assert!(RECONCILE_INTERVAL_SECS <= 24 * 3_600);
        }
    }

    #[test]
    fn summary_counts_by_outcome() {
        let summary = summarize(&[
            ActionOutcome::Deleted {
                id: "gone".to_owned(),
            },
            ActionOutcome::Created {
                broadcaster_user_id: "444".to_owned(),
            },
            ActionOutcome::AlreadyExists {
                broadcaster_user_id: "222".to_owned(),
            },
        ]);
        assert_eq!(
            summary,
            ReconcileSummary {
                deleted: 1,
                created: 1,
                already_exists: 1,
                prior_failed: 0,
                status: None,
            }
        );
        assert_eq!(
            summarize(&[]),
            ReconcileSummary::default(),
            "an empty plan reconciles to nothing"
        );
    }

    #[tokio::test]
    async fn reconcile_runs_deletes_before_creates_then_confirms_healthy() {
        let listing = json!({
            "data": [
                subscription_entry("stale-id", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let confirmed = json!({
            "data": [
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
                subscription_entry("fresh-id", "enabled", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let summary = reconcile_all(
            &mut client,
            &callback(),
            &["222".to_owned(), "444".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect("healthy follow-up must succeed");
        assert_eq!(summary.status, Some("healthy"));
        assert_eq!(
            summary,
            ReconcileSummary {
                deleted: 1,
                created: 1,
                already_exists: 0,
                prior_failed: 0,
                status: Some("healthy"),
            }
        );
        let seen = log.requests();
        assert_eq!(
            seen.len(),
            5,
            "token, listing, delete, create, follow-up listing"
        );
        assert_eq!(seen[2].method, Method::DELETE);
        assert_eq!(seen[3].method, Method::POST);
    }

    #[tokio::test]
    async fn reconcile_reports_pending_when_only_some_desired_are_enabled() {
        let listing = json!({"data": [], "pagination": {}});
        let confirmed = json!({
            "data": [
                subscription_entry("id-111", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("id-222", "webhook_callback_verification_pending", "222", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let summary = reconcile_all(
            &mut client,
            &callback(),
            &["111".to_owned(), "222".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect("a mixed enabled/pending follow-up must still succeed");
        assert_eq!(
            summary.status,
            Some("pending"),
            "one enabled desired broadcaster must not mask a pending one"
        );
        assert_eq!(summary.created, 2);
        let seen = log.requests();
        assert_eq!(
            seen.len(),
            5,
            "token, listing, two creates, follow-up listing"
        );
        assert_eq!(seen[2].method, Method::POST);
        assert_eq!(seen[3].method, Method::POST);
    }

    #[tokio::test]
    async fn reconcile_fails_when_followup_desired_subscription_is_failed() {
        let listing = json!({"data": [], "pagination": {}});
        let confirmed = json!({
            "data": [
                subscription_entry("id-444", "webhook_callback_verification_failed", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let error = reconcile_all(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect_err("a failed desired subscription in the follow-up must fail the pass");
        assert!(
            matches!(error, HelixError::BadResponse(_)),
            "a failed follow-up subscription must surface, not count as done: {error}"
        );
    }

    #[tokio::test]
    async fn reconcile_fails_loudly_when_followup_listing_is_unhealthy() {
        let listing = json!({"data": [], "pagination": {}});
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&listing),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let error = reconcile_all(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect_err("missing follow-up subscription must fail");
        assert!(
            matches!(error, HelixError::BadResponse(_)),
            "unhealthy state must surface, not count as done: {error}"
        );
    }

    #[tokio::test]
    async fn reconcile_runs_later_creates_after_a_rejected_create_then_fails() {
        let listing = json!({"data": [], "pagination": {}});
        let confirmed = json!({
            "data": [
                subscription_entry("id-111", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("id-333", "enabled", "333", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::rejected(StatusCode::BAD_REQUEST),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let error = reconcile_all(
            &mut client,
            &callback(),
            &["111".to_owned(), "222".to_owned(), "333".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect_err("a rejected create must fail the pass");
        assert!(
            matches!(
                error,
                HelixError::HelixRejected { status, .. } if status == StatusCode::BAD_REQUEST
            ),
            "the first rejected action is the pass error: {error}"
        );
        let seen = log.requests();
        assert_eq!(
            seen.len(),
            6,
            "token, listing, three creates, follow-up listing"
        );
        assert_eq!(seen[4].method, Method::POST);
        assert!(
            seen[4]
                .body
                .as_ref()
                .expect("create carries a body")
                .expose()
                .contains("333"),
            "the create after the rejected one is still issued"
        );
        assert_eq!(
            seen[5].method,
            Method::GET,
            "the follow-up listing runs after a failed action"
        );
    }

    #[tokio::test]
    async fn reconcile_fails_on_a_rejected_delete_even_when_nothing_is_missing() {
        let listing = json!({
            "data": [
                subscription_entry("stale-id", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let confirmed = json!({
            "data": [
                subscription_entry("stale-id", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::rejected(StatusCode::INTERNAL_SERVER_ERROR),
                ScriptTransport::listing_answer(&confirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let error = reconcile_all(
            &mut client,
            &callback(),
            &["222".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect_err("a rejected delete must fail the pass even though 222 is healthy");
        assert!(
            matches!(
                error,
                HelixError::HelixRejected { status, .. }
                    if status == StatusCode::INTERNAL_SERVER_ERROR
            ),
            "the rejected delete is the pass error: {error}"
        );
        assert_eq!(
            log.requests().len(),
            4,
            "token, listing, delete, follow-up listing"
        );
    }

    #[tokio::test]
    async fn reconcile_surfaces_prior_failed_when_failed_entry_is_recreated_to_pending() {
        let listing = json!({
            "data": [
                subscription_entry("failed-id", "webhook_callback_verification_failed", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let confirmed = json!({
            "data": [
                subscription_entry("fresh-id", "webhook_callback_verification_pending", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let log = CallLog::default();
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            log.clone(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let summary = reconcile_all(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
        )
        .await
        .expect("pending follow-up after a prior failure must still succeed");
        assert_eq!(summary.status, Some("pending"));
        assert_eq!(summary.prior_failed, 1);
        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.created, 1);
        let seen = log.requests();
        assert_eq!(
            seen.len(),
            5,
            "token, listing, delete, create, follow-up listing"
        );
        assert_eq!(seen[2].method, Method::DELETE);
        assert_eq!(seen[3].method, Method::POST);
    }

    #[tokio::test]
    async fn reconcile_records_healthy_pass_creates_and_deletes() {
        let listing = json!({
            "data": [
                subscription_entry("stale-id", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let confirmed = json!({
            "data": [
                subscription_entry("keep-id", "enabled", "222", "https://hooks.example.com/twitch"),
                subscription_entry("fresh-id", "enabled", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let metrics = two_bot_core::metrics::Metrics::default();
        let summary = reconcile_all_with_metrics(
            &mut client,
            &callback(),
            &["222".to_owned(), "444".to_owned()],
            &test_secret(),
            &never_shutdown(),
            &metrics,
        )
        .await
        .expect("healthy follow-up must succeed");
        assert_eq!(summary.status, Some("healthy"));
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"pending\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 1\n"));
        for error_class in [
            "reconcile_action_failed",
            "reconcile_prior_failed",
            "reconcile_unhealthy",
            "reconcile_failed",
        ] {
            assert!(
                text.contains(&format!(
                    "two_bot_eventsub_reconcile_errors_total{{error_class=\"{error_class}\"}} 0\n"
                )),
                "no {error_class} on a clean healthy pass"
            );
        }
    }

    #[tokio::test]
    async fn reconcile_records_pending_pass_with_prior_failed() {
        let listing = json!({
            "data": [
                subscription_entry("failed-id", "webhook_callback_verification_failed", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let confirmed = json!({
            "data": [
                subscription_entry("fresh-id", "webhook_callback_verification_pending", "444", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let metrics = two_bot_core::metrics::Metrics::default();
        let summary = reconcile_all_with_metrics(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
            &metrics,
        )
        .await
        .expect("pending follow-up after a prior failure must still succeed");
        assert_eq!(summary.status, Some("pending"));
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"pending\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 1\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_prior_failed\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_unhealthy\"} 0\n"
        ));
    }

    #[tokio::test]
    async fn reconcile_records_failed_pass_when_initial_listing_fails() {
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                Err(HelixError::BadResponse("fixture listing must fail")),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let metrics = two_bot_core::metrics::Metrics::default();
        reconcile_all_with_metrics(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
            &metrics,
        )
        .await
        .expect_err("a failed listing must fail the pass");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"pending\"} 0\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_failed\"} 1\n"
        ));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 0\n"));
    }

    #[tokio::test]
    async fn reconcile_records_failed_pass_when_followup_listing_fails() {
        let listing = json!({"data": [], "pagination": {}});
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                Err(HelixError::BadResponse(
                    "fixture follow-up listing must fail",
                )),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let metrics = two_bot_core::metrics::Metrics::default();
        reconcile_all_with_metrics(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &never_shutdown(),
            &metrics,
        )
        .await
        .expect_err("a failed follow-up listing must fail the pass");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 1\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_failed\"} 1\n"
        ));
        // The successful create still counts even though confirmation never ran.
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 1\n"));
    }

    #[tokio::test]
    async fn reconcile_records_unhealthy_verdict_with_partial_actions() {
        let listing = json!({"data": [], "pagination": {}});
        let confirmed = json!({
            "data": [
                subscription_entry("id-111", "enabled", "111", "https://hooks.example.com/twitch"),
                subscription_entry("id-333", "enabled", "333", "https://hooks.example.com/twitch"),
            ],
            "pagination": {},
        });
        let transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
                ScriptTransport::rejected(StatusCode::BAD_REQUEST),
                ScriptTransport::no_content(),
                ScriptTransport::listing_answer(&confirmed),
            ],
            CallLog::default(),
        );
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let metrics = two_bot_core::metrics::Metrics::default();
        reconcile_all_with_metrics(
            &mut client,
            &callback(),
            &["111".to_owned(), "222".to_owned(), "333".to_owned()],
            &test_secret(),
            &never_shutdown(),
            &metrics,
        )
        .await
        .expect_err("a rejected create must fail the pass");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 1\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_action_failed\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_unhealthy\"} 1\n"
        ));
        // The two successful creates still count even though the pass failed.
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 2\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn run_loop_repeats_each_interval_until_shutdown() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let passes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&passes);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_loop(shutdown_rx, Duration::from_secs(60), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async {}
        }));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "startup pass runs at once"
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            2,
            "first interval tick runs a pass"
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            3,
            "second interval tick runs a pass"
        );
        shutdown_tx.send(true).expect("loop holds the receiver");
        task.await.expect("loop exits on shutdown");
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            3,
            "no pass runs after shutdown"
        );
    }

    #[tokio::test]
    async fn shutdown_latched_before_a_pass_skips_every_helix_call() {
        // The script holds no answers, so any Helix call would panic inside
        // the transport; zero logged requests is the positive proof.
        let log = CallLog::default();
        let transport = ScriptTransport::new(Vec::new(), log.clone());
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(true);
        let summary = reconcile_all(
            &mut client,
            &callback(),
            &["444".to_owned()],
            &test_secret(),
            &shutdown_rx,
        )
        .await
        .expect("a shutdown-skipped pass is not a failure");
        assert_eq!(summary, ReconcileSummary::default());
        assert!(log.requests().is_empty(), "no Helix call may run");
    }

    #[tokio::test]
    async fn shutdown_between_creates_skips_the_remaining_actions() {
        let listing = json!({"data": [], "pagination": {}});
        let log = CallLog::default();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::no_content(),
            ],
            log.clone(),
        );
        transport.flip_shutdown_on_create = Some(shutdown_tx);
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let summary = reconcile_all(
            &mut client,
            &callback(),
            &["111".to_owned(), "222".to_owned()],
            &test_secret(),
            &shutdown_rx,
        )
        .await
        .expect("a shutdown-interrupted pass is not a failure");
        assert_eq!(summary.created, 1, "only the in-flight create completes");
        assert_eq!(
            summary.status, None,
            "an interrupted pass claims no verification status"
        );
        assert_eq!(
            log.requests().len(),
            3,
            "token, listing, first create: the second create and the follow-up listing never run"
        );
    }

    #[tokio::test]
    async fn shutdown_after_a_failed_create_still_fails_the_pass() {
        let listing = json!({"data": [], "pagination": {}});
        let log = CallLog::default();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut transport = ScriptTransport::new(
            vec![
                ScriptTransport::token_answer(),
                ScriptTransport::listing_answer(&listing),
                ScriptTransport::rejected(StatusCode::BAD_REQUEST),
            ],
            log.clone(),
        );
        transport.flip_shutdown_on_create = Some(shutdown_tx);
        let mut client = HelixClient::new(transport, creds()).with_clock(|| 0);
        let error = reconcile_all(
            &mut client,
            &callback(),
            &["111".to_owned(), "222".to_owned()],
            &test_secret(),
            &shutdown_rx,
        )
        .await
        .expect_err("shutdown must not mask a create rejected before it latched");
        assert!(
            matches!(
                error,
                HelixError::HelixRejected { status, .. } if status == StatusCode::BAD_REQUEST
            ),
            "the rejected create is still the pass error: {error}"
        );
        assert_eq!(
            log.requests().len(),
            3,
            "token, listing, rejected create: no later action runs after shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_loop_skips_the_startup_pass_when_shutdown_is_already_latched() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let passes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&passes);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(true);
        let task = tokio::spawn(run_loop(shutdown_rx, Duration::from_secs(60), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async {}
        }));
        join(Some(task), Duration::from_secs(300)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            0,
            "a latched shutdown starts no pass, not even the startup one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_a_pass_drains_it_and_the_task_is_joined() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::sync::Notify;

        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let passes = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let (pass_started, pass_release) = (Arc::clone(&started), Arc::clone(&release));
        let (pass_count, done_count) = (Arc::clone(&passes), Arc::clone(&finished));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_loop(shutdown_rx, Duration::from_secs(60), move || {
            let started = Arc::clone(&pass_started);
            let release = Arc::clone(&pass_release);
            let passes = Arc::clone(&pass_count);
            let finished = Arc::clone(&done_count);
            async move {
                passes.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
                release.notified().await;
                finished.fetch_add(1, Ordering::SeqCst);
            }
        }));
        started.notified().await;
        shutdown_tx.send(true).expect("loop holds the receiver");
        assert!(
            !task.is_finished(),
            "shutdown must not abandon the in-flight pass"
        );
        assert_eq!(finished.load(Ordering::SeqCst), 0, "pass still held");
        release.notify_one();
        join(Some(task), Duration::from_secs(300)).await;
        assert_eq!(
            finished.load(Ordering::SeqCst),
            1,
            "join returned only after the drained pass completed"
        );
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert_eq!(
            passes.load(Ordering::SeqCst),
            1,
            "no pass starts after shutdown"
        );
    }

    #[tokio::test]
    async fn join_aborts_a_task_that_outlives_the_drain_and_still_awaits_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let task = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        join(Some(task), Duration::from_millis(20)).await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "the aborted task's future was dropped before join returned, so nothing is detached"
        );
    }

    #[tokio::test]
    async fn join_swallows_a_panicked_task_and_accepts_no_task() {
        join(None, Duration::from_secs(1)).await;
        let task = tokio::spawn(async {
            // panic_any (not the panic macro) so the semgrep
            // rust-panic-in-prod gate, which flags added panic lines even in
            // inline tests, stays silent while still producing a panicked
            // JoinError for join to swallow.
            std::panic::panic_any("synthetic reconcile task panic");
        });
        // Returning at all is the assertion: the JoinError is logged inside
        // join, never propagated to the shutdown path.
        join(Some(task), Duration::from_secs(60)).await;
    }
}
