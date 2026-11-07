//! Protected acceptance: run a registered feed job, not a constant gate helper.
//! The real poller sees a closed test pool: an admitted tick returns Database,
//! whereas a disabled tick returns Ok before ANY database or fetch work.
//!
//! Builder API: FeedPoller::registered_job(self: Arc<Self>, context: Arc<Context>,
//! gates: FeatureGates, activation: &BootActivation, live: Option<LiveSettings>,
//! fetch: Arc<dyn FeedFetch>) -> Option<Job>. Production register must delegate
//! to this SAME factory (None selects the production provider, Some is local),
//! and both paths must share the actual registration gate, never fork it.
//! The baseline fallback assembles the existing real poller action and calls
//! register_fenced; it intentionally cannot inject the missing settings reader.
//! Mutant: hardcode true at the production per-tick announcements reader.

#![cfg(test)]

use super::*;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::settings::LiveSettings;

trait BaselineFeedRegistration {
    fn registered_job(
        self: Arc<Self>,
        context: Arc<Context>,
        gates: FeatureGates,
        activation: &BootActivation,
        live: Option<LiveSettings>,
        fetch: Arc<dyn FeedFetch>,
    ) -> Option<Job>;
}

impl BaselineFeedRegistration for FeedPoller {
    fn registered_job(
        self: Arc<Self>,
        context: Arc<Context>,
        gates: FeatureGates,
        activation: &BootActivation,
        _live: Option<LiveSettings>,
        fetch: Arc<dyn FeedFetch>,
    ) -> Option<Job> {
        let guild = context.guild.clone();
        let action: JobAction = Arc::new(move || {
            let context = context.clone();
            let poller = self.clone();
            let fetch = fetch.clone();
            Box::pin(async move {
                poller
                    .run_once(
                        context.pool().await?,
                        &context.rest,
                        &context.guild,
                        fetch.as_ref(),
                    )
                    .await
            })
        });
        register_fenced(gates, activation, action, guild)
    }
}

struct ForbiddenFetch(Arc<AtomicUsize>);
impl FeedFetch for ForbiddenFetch {
    fn fetch(&self, _feed: FeedRelay) -> FetchFuture {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ErrorClass::Configuration) })
    }
}

async fn closed_context(mock: &MockRest) -> Arc<Context> {
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("two_bot_test_live_settings_acceptance");
    let pool = PgPoolOptions::new().connect_lazy_with(options);
    pool.close().await;
    Context::live_settings_fixture(pool, executor(mock), GUILD)
}

#[tokio::test(start_paused = true)]
async fn registered_job_observes_published_off_on_delete_and_foreign_rows() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let context = closed_context(&mock).await;
    let (mut writer, live) = live_channel();
    let fetches = Arc::new(AtomicUsize::new(0));
    let boot = announcements(Some("1"));
    let interval = Duration::from_secs(boot.feed_poll_seconds + 1);
    let job = Arc::new(FeedPoller::default())
        .registered_job(
            context,
            boot,
            &fixtures::staging(),
            Some(live),
            Arc::new(ForbiddenFetch(fetches.clone())),
        )
        .expect("boot-on staging job registers before live off is read");
    let mut skipped = Vec::new();
    for (revision, guild, value) in [
        (1, GUILD, Some(json!(0))),
        (2, GUILD, Some(json!(1))),
        (3, GUILD, None),
        (4, GUILD, Some(json!(0))),
        (5, "9999", Some(json!(0))),
    ] {
        writer.publish(&SettingsSnapshot {
            revision,
            rows: value
                .map(|value| SettingRow {
                    guild_id: guild.into(),
                    key: ANNOUNCEMENTS_KEY.into(),
                    value,
                    version: revision,
                })
                .into_iter()
                .collect(),
        });
        tokio::time::advance(interval).await;
        let result = (job.action)().await;
        assert!(
            result.is_ok() || matches!(result, Err(ErrorClass::Database)),
            "closed pool is the only admitted-tick failure, revision {revision}"
        );
        skipped.push(result.is_ok());
    }
    assert_eq!(skipped, vec![true, false, false, true, false],
        "registered poller must skip only own-guild off snapshots, not enabled/delete/foreign ticks");
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        0,
        "closed pool fences all feed fetches"
    );
    assert!(
        mock.requests().is_empty(),
        "no remote delivery at this boundary"
    );
}

#[tokio::test(start_paused = true)]
async fn baseline_registration_control_executes_the_real_poller() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(500)).await;
    let context = closed_context(&mock).await;
    let (_writer, live) = live_channel();
    let fetches = Arc::new(AtomicUsize::new(0));
    let job = <FeedPoller as BaselineFeedRegistration>::registered_job(
        Arc::new(FeedPoller::default()),
        context,
        announcements(Some("1")),
        &fixtures::staging(),
        Some(live),
        Arc::new(ForbiddenFetch(fetches.clone())),
    )
    .expect("baseline real registered job");
    assert!(
        matches!((job.action)().await, Err(ErrorClass::Database)),
        "real poller reaches its database query; this is not an action-count fake"
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    assert!(mock.requests().is_empty());
}
