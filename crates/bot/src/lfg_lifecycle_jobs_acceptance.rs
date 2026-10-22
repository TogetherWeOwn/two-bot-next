//! Protected acceptance corpus: the builder must not edit this file.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the test card's branch,
//! keep every test here verbatim, and make the suite green by changing
//! production code only. This half of the LFG lifecycle corpus lives in the
//! bot crate (rather than beside the other cases in
//! `crates/discord/tests/lfg_lifecycle_acceptance.rs`) because the supervised
//! job seam under test is the binary's own `crate::jobs`: only a bot unit
//! test resolves its `crate::` graph. Real migrated disposable Postgres, the
//! real LFG service, the real HTTP executor and the real jobs supervisor;
//! only Discord is a loopback double. No implementation diff informed these
//! expectations. See tests/lfg-lifecycle.acceptance.json for the contract.

use crate::{
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
    jobs,
};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use two_bot_core::{
    lfg::{self, LfgPost, LfgStatus},
    lfg_store as store,
};
use two_bot_discord::{
    lfg_interactions::{LfgError, LfgInteractions},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: u64 = 1111;
const OWNER: &str = "3333";
const OLD: &str = "2000-01-01T00:00:00.000Z";

// Baseline compatibility adapter, NOT a sweep implementation: the baseline has
// no lifecycle entrypoint and therefore does no work. An inherent production
// method with this signature takes precedence at every unqualified call below.
// Returning a count without changing real rows / HTTP cannot satisfy the tests.
trait MissingLifecycleSweep {
    async fn sweep_lifecycle(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        bot_user_id: u64,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<usize, LfgError>;
}
impl MissingLifecycleSweep for LfgInteractions {
    async fn sweep_lifecycle(
        &self,
        _executor: &ActionExecutor,
        _guild_id: &str,
        _bot_user_id: u64,
        _now: OffsetDateTime,
        _limit: usize,
    ) -> Result<usize, LfgError> {
        Ok(0)
    }
}

fn clock() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(4_102_444_800).expect("2100-01-01 UTC")
}

fn post(id: &str, starts_at: &str, message: Option<&str>, created_at: &str) -> LfgPost {
    LfgPost {
        id: id.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: message.map(str::to_owned),
        title: "Offline raid".into(),
        starts_at: starts_at.into(),
        status: LfgStatus::Open,
        created_by: OWNER.into(),
        created_at: created_at.into(),
        closed_at: None,
    }
}

fn ok_rest(request: &RestRequest) -> ScriptedResponse {
    if request.method == "GET" {
        ScriptedResponse::json(200, serde_json::json!([]))
    } else if request.path.ends_with("/callback") {
        ScriptedResponse::status(204)
    } else {
        ScriptedResponse::json(200, serde_json::json!({"id": "9000"}))
    }
}

struct Fixture {
    db: TestDatabase,
    service: Arc<LfgInteractions>,
    executor: Arc<ActionExecutor>,
    rest: MockRest,
}
impl Fixture {
    async fn new(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        // The shared guard rejects production/staging, credentials and libpq
        // overrides. Absence/setup failure is NOT silently counted as a pass.
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let rest = MockRest::with_responder(responder).await;
        let executor = Arc::new(
            ActionExecutor::with_proxy("offline-lfg-test-token".into(), Some(rest.origin()))
                .expect("loopback executor"),
        );
        let service = Arc::new(LfgInteractions::new(db.pool().clone()));
        Self {
            db,
            service,
            executor,
            rest,
        }
    }

    async fn seed(&self, row: &LfgPost) {
        let roles = lfg::spec_roles(
            &row.id,
            &lfg::parse_role_spec("tank:Tank:1,dps:DPS:1").expect("roles"),
        );
        store::put_lfg(self.db.pool(), row, &roles, true)
            .await
            .expect("seed LFG");
    }

    async fn saved(&self, id: &str) -> LfgPost {
        store::get_lfg(self.db.pool(), GUILD, id)
            .await
            .expect("read LFG")
            .expect("retained row")
    }

    async fn finish(self) -> Vec<RestRequest> {
        let requests = self.rest.requests();
        self.rest.shutdown().await;
        drop(self.service);
        drop(self.executor);
        self.db.close().await.expect("fixture teardown");
        requests
    }
}

#[tokio::test]
async fn lifecycle_work_runs_under_the_existing_supervised_job_seam() {
    // Keep the compatibility trait live after an inherent method is added.
    let _baseline_entrypoint = <LfgInteractions as MissingLifecycleSweep>::sweep_lifecycle;
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-job", OLD, Some("5001"), OLD)).await;
    let action: jobs::JobAction = {
        let service = Arc::clone(&f.service);
        let executor = Arc::clone(&f.executor);
        Arc::new(move || {
            let service = Arc::clone(&service);
            let executor = Arc::clone(&executor);
            Box::pin(async move {
                service
                    .sweep_lifecycle(&executor, GUILD, BOT, clock(), 3)
                    .await
                    .map(|_| ())
                    .map_err(|_| jobs::ErrorClass::Database)
            })
        })
    };
    let status = jobs::statuses(&["lfg_sweep"], false);
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(
        vec![jobs::Job {
            name: "lfg_sweep",
            cadence: Duration::from_secs(3600),
            cadence_source: None,
            startup_jitter: Duration::ZERO,
            timeout: Duration::from_secs(10),
            action,
        }],
        Arc::clone(&status),
        receiver,
    ));
    // Synchronize on the real supervisor's completion, not a guessed sleep or
    // paused clock competing with PostgreSQL/loopback I/O.
    let completed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = status.read().await;
            if snapshot["lfg_sweep"].last_success.is_some()
                || snapshot["lfg_sweep"].last_error_class.is_some()
            {
                break snapshot["lfg_sweep"].clone();
            }
            drop(snapshot);
            tokio::task::yield_now().await;
        }
    })
    .await;
    shutdown.send(true).expect("stop supervisor");
    assert!(supervisor.await.is_ok(), "supervisor joined");
    let saved = f.saved("lfg-job").await;
    f.finish().await;
    let completed = completed.expect("bounded job completion");
    assert!(
        completed.last_success.is_some(),
        "sweep uses the existing job success/failure contract"
    );
    assert_eq!(
        saved.status,
        LfgStatus::Closed,
        "the scheduled action must change real persisted state"
    );
}
