//! Observe SQLx's executed-query receipts, not source text or caller output.
//! Each capture owns a dispatcher; parallel fixtures cannot mix observations.

use std::future::Future;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Metadata, Subscriber};

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub sql: String,
    pub rows: u64,
}

impl Query {
    pub fn normalized(&self) -> String {
        self.sql
            .replace("\\n", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    }
}

#[derive(Clone, Default)]
pub struct Queries(Arc<Mutex<Vec<Query>>>);

#[derive(Default)]
struct Fields {
    statement: String,
    summary: String,
    rows: Option<u64>,
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows_returned" {
            self.rows = Some(value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "db.statement" => self.statement = value.to_owned(),
            "summary" => self.summary = value.to_owned(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

impl Subscriber for Queries {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "sqlx::query"
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(rows) = fields.rows {
            self.0.lock().unwrap().push(Query {
                sql: if fields.statement.is_empty() {
                    fields.summary
                } else {
                    fields.statement
                },
                rows,
            });
        }
    }
}

impl Queries {
    fn dispatch(&self) -> Dispatch {
        // tracing-core's single-dispatcher fast path registers a new callsite
        // against the registering thread's default, which may be no subscriber.
        // Retain an inert second dispatcher so registration consults all live
        // subscribers instead. Do not install a global default: bot tests already
        // own one. Only each capture's scoped dispatcher holds its receipt buffer.
        static ANCHOR: std::sync::OnceLock<Dispatch> = std::sync::OnceLock::new();
        ANCHOR.get_or_init(|| Dispatch::new(tracing::subscriber::NoSubscriber::default()));
        Dispatch::new(self.clone())
    }

    pub async fn capture<F: Future>(future: F) -> (F::Output, Vec<Query>) {
        let queries = Self::default();
        let dispatch = queries.dispatch();
        let result = future.with_subscriber(dispatch).await;
        let captured = queries.0.lock().unwrap().clone();
        (result, captured)
    }

    pub fn capture_sync<T>(call: impl FnOnce() -> T) -> (T, Vec<Query>) {
        let queries = Self::default();
        let dispatch = queries.dispatch();
        let result = tracing::dispatcher::with_default(&dispatch, call);
        let captured = queries.0.lock().unwrap().clone();
        (result, captured)
    }
}

/// Every actual fetch must respect the bound, including the lookahead row.
/// A missing receipt is a broken observer, never evidence of a bounded read.
pub fn assert_fetch_bound(queries: &[Query], selects: impl Fn(&str) -> bool, cap: u64) {
    let reads: Vec<_> = queries
        .iter()
        .filter(|q| selects(&q.normalized()))
        .collect();
    assert!(
        !reads.is_empty(),
        "no executed-query receipt for the target read"
    );
    assert!(
        reads.iter().any(|q| q.rows > 0),
        "fixture must exercise a nonempty read"
    );
    for read in reads {
        assert!(
            read.rows <= cap,
            "actual fetch returned {} rows, cap {cap}: {}",
            read.rows,
            read.sql
        );
    }
}

#[test]
fn observer_keeps_callsite_interest_from_an_unobserved_thread() {
    const CHILD: &str = "TWO_QUERY_OBSERVER_CALLSITE_CHILD";
    let test_name = format!(
        "{}::observer_keeps_callsite_interest_from_an_unobserved_thread",
        module_path!().split_once("::").unwrap().1
    );
    if std::env::var(CHILD).ok().as_deref() != Some(test_name.as_str()) {
        // The defect requires exactly one live dispatcher. Isolate this probe
        // from sibling tests that might otherwise accidentally repair the cache.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD, &test_name)
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
            "isolated callsite probe failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let register_elsewhere = || {
        std::thread::spawn(|| {
            // Register metadata only: never manufacture an executed receipt.
            let site = tracing::callsite! {
                name: "query observer foreign registration",
                kind: tracing::metadata::Kind::EVENT,
                target: "sqlx::query",
                level: tracing::Level::DEBUG,
                fields: "db.statement", "rows_returned"
            };
            assert!(
                !site.interest().is_never(),
                "foreign registration disabled a live query capture"
            );
            tracing::callsite::rebuild_interest_cache();
            assert!(
                !site.interest().is_never(),
                "foreign rebuild disabled a live query capture"
            );
        })
        .join()
        .unwrap();
    };
    let (_, receipts) = Queries::capture_sync(register_elsewhere);
    assert!(receipts.is_empty(), "metadata is not an executed query");
    for _ in 0..3 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_, receipts) = runtime.block_on(Queries::capture(async {
            tokio::task::yield_now().await;
            register_elsewhere();
        }));
        assert!(receipts.is_empty(), "metadata is not an executed query");
    }
}

#[test]
fn observer_isolates_sync_async_and_unobserved_queries_across_runtimes() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let workers: Vec<_> = (0..3)
        .map(|worker| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let url = std::env::var("TWO_TEST_DATABASE_URL")
                    .expect("disposable DB bootstrap required");
                let db = runtime
                    .block_on(two_bot_testsupport::TestDatabase::create(
                        &url,
                        &sqlx::migrate!("../cutover/migrations"),
                    ))
                    .unwrap();
                let (sql, rows) = match worker {
                    0 => (
                        "SELECT n FROM generate_series(1, 26) n /* observer_sync */",
                        26,
                    ),
                    1 => (
                        "SELECT n FROM generate_series(1, 60) n /* observer_async */",
                        60,
                    ),
                    _ => (
                        "SELECT n FROM generate_series(1, 17) n /* observer_noise */",
                        17,
                    ),
                };
                for _ in 0..3 {
                    let read = async {
                        tokio::time::timeout(std::time::Duration::from_secs(30), barrier.wait())
                            .await
                            .expect("parallel captures did not rendezvous");
                        tokio::task::yield_now().await;
                        let result = sqlx::query(sql).fetch_all(db.pool()).await;
                        tokio::time::timeout(std::time::Duration::from_secs(30), barrier.wait())
                            .await
                            .expect("parallel queries did not finish");
                        result
                    };
                    let (result, receipts) = match worker {
                        0 => Queries::capture_sync(|| runtime.block_on(read)),
                        1 => runtime.block_on(Queries::capture(read)),
                        _ => (runtime.block_on(read), Vec::new()),
                    };
                    assert_eq!(result.unwrap().len() as u64, rows);
                    if worker < 2 {
                        let reads: Vec<_> = receipts
                            .iter()
                            .filter(|query| query.normalized().contains("generate_series"))
                            .collect();
                        assert_eq!(reads.len(), 1, "capture lost or mixed executed queries");
                        assert_eq!(reads[0].normalized(), sql.to_ascii_lowercase());
                        assert_eq!(reads[0].rows, rows);
                    }
                }
                runtime.block_on(db.close()).unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn observer_sees_actual_rows_and_rejects_unbounded_control() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable DB bootstrap required");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
    let (bounded, receipts) = Queries::capture_sync(|| {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                sqlx::query("SELECT n FROM generate_series(1, 60) n LIMIT 26").fetch_all(db.pool()),
            )
        })
    });
    assert_eq!(bounded.unwrap().len(), 26);
    assert_fetch_bound(&receipts, |q| q.contains("generate_series"), 26);
    let (unbounded, receipts) = Queries::capture(
        sqlx::query("SELECT n FROM generate_series(1, 60) n").fetch_all(db.pool()),
    )
    .await;
    assert_eq!(unbounded.unwrap().len(), 60);
    assert_eq!(
        receipts
            .iter()
            .find(|q| q.normalized().contains("generate_series"))
            .unwrap()
            .rows,
        60
    );
    db.close().await.unwrap();
    assert!(
        std::panic::catch_unwind(|| assert_fetch_bound(
            &receipts,
            |q| q.contains("generate_series"),
            26
        ))
        .is_err(),
        "unbounded control must be rejected"
    );
}
