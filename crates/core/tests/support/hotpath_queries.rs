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
    pub async fn capture<F: Future>(future: F) -> (F::Output, Vec<Query>) {
        let queries = Self::default();
        let dispatch = Dispatch::new(queries.clone());
        let result = future.with_subscriber(dispatch).await;
        let captured = queries.0.lock().unwrap().clone();
        (result, captured)
    }

    pub fn capture_sync<T>(call: impl FnOnce() -> T) -> (T, Vec<Query>) {
        let queries = Self::default();
        let dispatch = Dispatch::new(queries.clone());
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
