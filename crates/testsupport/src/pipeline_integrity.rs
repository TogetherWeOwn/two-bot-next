//! Independent correctness oracles for the synthetic pipeline, never runtime code.
//!
//! Expected event keys must be captured before the candidate persistence seam.
//! Server counters must come from a separate observer database: querying the
//! measured database would add observer transactions to the quantity under test.

use anyhow::{ensure, Context, Result};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};

/// Create the observer fixture, awaiting measured-fixture cleanup on errors.
/// Drop-only cleanup cannot be relied on while a CLI runtime is shutting down.
pub async fn add_observer(
    raw: &str,
    db: crate::TestDatabase,
) -> Result<(crate::TestDatabase, crate::TestDatabase)> {
    match crate::TestDatabase::create(raw, &sqlx::migrate::Migrator::DEFAULT).await {
        Ok(observer) => Ok((db, observer)),
        Err(error) => {
            db.close()
                .await
                .context("verify measured fixture cleanup after observer setup failure")?;
            Err(error).context("create disposable observer database")
        }
    }
}

/// Check the complete expected set, not just a plausible surviving row count.
/// Query only after the dispatch writer has drained (or the killed backend has
/// exited). The database is a fresh, uniquely owned TestDatabase fixture.
pub async fn verify_durable_events(pool: &PgPool, expected: &BTreeSet<String>) -> Result<()> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT e.idempotency_key, count(j.id),
                count(j.id) FILTER (WHERE j.op = 'insert' AND j.pre_image IS NULL)
         FROM events e LEFT JOIN rollback_journal j
           ON j.table_name = 'events' AND j.row_identity = e.id::text
         GROUP BY e.id, e.idempotency_key",
    )
    .fetch_all(pool)
    .await
    .context("read durable pipeline event coverage")?;
    verify_event_rows(expected, &rows)?;
    let orphaned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rollback_journal j
         LEFT JOIN events e ON j.row_identity = e.id::text
         WHERE j.table_name = 'events' AND e.id IS NULL",
    )
    .fetch_one(pool)
    .await
    .context("read orphaned pipeline journal coverage")?;
    ensure!(orphaned == 0, "orphaned pipeline event journal entries");
    Ok(())
}

fn verify_event_rows(expected: &BTreeSet<String>, rows: &[(String, i64, i64)]) -> Result<()> {
    let actual: BTreeSet<_> = rows.iter().map(|(key, _, _)| key.clone()).collect();
    ensure!(
        actual == *expected && rows.len() == expected.len(),
        "every expected event must be durable after drain; unexpected events are forbidden"
    );
    ensure!(
        rows.iter()
            .all(|(_, total, inserts)| *total == 1 && *inserts == 1),
        "exactly one insert journal entry required per durable event"
    );
    Ok(())
}

/// Read completed Execute/simple-query records from the disposable server's
/// JSON log. PostgreSQL statistics do not reliably count repeated prepared
/// utility statements, so they are a separate mechanism profile, not the exact
/// exchange counter. Boundaries are two fixed observer statements, outside the
/// measured client window. Their two transactions are explicitly accounted for.
pub async fn completed_statement_profile(
    observer: &PgPool,
    measured_database: &str,
) -> Result<BTreeMap<i64, i64>> {
    let path: Option<String> = sqlx::query_scalar("SELECT pg_current_logfile('jsonlog')")
        .fetch_one(observer)
        .await
        .context("disposable JSON logging required")?;
    let path = path.context("disposable JSON logging required")?;
    let mut window = CompletionWindow::default();
    let mut pending = Vec::new();
    let mut offset = 0_i64;
    // Backend exit does not flush the separate logging collector. Follow
    // appended bytes until the mandatory stop marker, under a fixed deadline.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let size: i64 = sqlx::query_scalar("SELECT size FROM pg_stat_file($1)")
                .bind(&path)
                .fetch_one(observer)
                .await?;
            ensure!(
                size >= offset && size <= 64 * 1024 * 1024,
                "server log truncated or exceeds 64 MiB budget"
            );
            while offset < size {
                let length = (size - offset).min(128 * 1024);
                let bytes: Vec<u8> = sqlx::query_scalar("SELECT pg_read_binary_file($1, $2, $3)")
                    .bind(&path)
                    .bind(offset)
                    .bind(length)
                    .fetch_one(observer)
                    .await?;
                ensure!(
                    bytes.len() as i64 == length,
                    "server log truncated during observation"
                );
                offset += length;
                pending.extend_from_slice(&bytes);
                if let Some(last) = pending.iter().rposition(|byte| *byte == b'\n') {
                    for line in pending[..last].split(|byte| *byte == b'\n') {
                        ensure!(
                            line.len() <= 64 * 1024,
                            "server log line exceeds 64 KiB budget"
                        );
                        if !line.is_empty() {
                            window.record(line, measured_database)?;
                        }
                    }
                    pending.drain(..=last);
                }
                ensure!(
                    pending.len() <= 64 * 1024,
                    "server log line exceeds 64 KiB budget"
                );
            }
            let current: Option<String> =
                sqlx::query_scalar("SELECT pg_current_logfile('jsonlog')")
                    .fetch_one(observer)
                    .await?;
            ensure!(
                current.as_deref() == Some(path.as_str()),
                "server log rotated during observation"
            );
            if window.stopped {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("server completion log did not drain within 10 seconds")??;
    // A trailing partial observer record is harmless; the complete stop marker
    // is mandatory. The measured writer backends have already exited.
    let statements = window.finish()?;
    let current: Option<String> = sqlx::query_scalar("SELECT pg_current_logfile('jsonlog')")
        .fetch_one(observer)
        .await?;
    ensure!(
        current.as_deref() == Some(path.as_str()),
        "server log rotated during observation"
    );
    let mut profile = BTreeMap::new();
    for (statement, count) in statements {
        // Hash on the server for a stable PG-version-pinned mechanism identity.
        // Only synthetic statement text, never log detail/bind values, is used.
        let fingerprint: i64 = sqlx::query_scalar("SELECT hashtextextended($1, 0)")
            .bind(statement)
            .fetch_one(observer)
            .await?;
        ensure!(
            profile.insert(fingerprint, count).is_none(),
            "server statement fingerprint collision"
        );
    }
    Ok(profile)
}

#[derive(Default)]
struct CompletionWindow {
    started: bool,
    stopped: bool,
    statements: BTreeMap<String, i64>,
}

impl CompletionWindow {
    fn record(&mut self, line: &[u8], database: &str) -> Result<()> {
        let record: serde_json::Value = serde_json::from_slice(line)
            .map_err(|_| anyhow::anyhow!("malformed server JSON log record"))?;
        if record["dbname"].as_str() != Some(database) {
            return Ok(());
        }
        if self.started && !self.stopped {
            ensure!(
                !matches!(
                    record["error_severity"].as_str(),
                    Some("ERROR" | "FATAL" | "PANIC")
                ),
                "server error inside measured pipeline window"
            );
        }
        let message = record["message"]
            .as_str()
            .context("server log message missing")?;
        let Some(duration) = message.strip_prefix("duration: ") else {
            return Ok(());
        };
        let Some((millis, command)) = duration.split_once(" ms  ") else {
            return Ok(());
        };
        ensure!(
            millis.parse::<f64>().is_ok(),
            "malformed server duration record"
        );
        let statement = if let Some(statement) = command.strip_prefix("statement: ") {
            statement
        } else if let Some(execute) = command.strip_prefix("execute ") {
            execute
                .split_once(": ")
                .context("malformed server Execute record")?
                .1
        } else {
            return Ok(()); // Parse and Bind are not completed SQL exchanges.
        };
        if statement == "SELECT 'pipeline-accounting-start'" {
            ensure!(
                !self.started && !self.stopped,
                "duplicate server start marker"
            );
            self.started = true;
        } else if statement == "SELECT 'pipeline-accounting-stop'" {
            ensure!(
                self.started && !self.stopped,
                "missing start or duplicate server stop marker"
            );
            self.stopped = true;
        } else if self.started && !self.stopped {
            ensure!(
                self.statements.len() < 1024 || self.statements.contains_key(statement),
                "server statement profile exceeds 1024 identities"
            );
            *self.statements.entry(statement.to_owned()).or_default() += 1;
        }
        Ok(())
    }

    fn finish(self) -> Result<BTreeMap<String, i64>> {
        ensure!(
            self.started && self.stopped && !self.statements.is_empty(),
            "completed server log window unavailable"
        );
        Ok(self.statements)
    }
}

/// Absolute server observations. Statement identities are PostgreSQL query IDs,
/// not SQLx summaries (which truncate statements and can collide).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerSnapshot {
    pub commits: i64,
    pub rollbacks: i64,
    pub stats_reset: Option<String>,
    pub statement_reset: String,
    pub deallocations: i64,
    pub statements: BTreeMap<i64, i64>,
}

impl ServerSnapshot {
    /// The extension must already be loaded by the disposable server and its
    /// view installed in the observer fixture. Never silently substitute client
    /// counts when server instrumentation is unavailable.
    pub async fn read(observer: &PgPool, measured_database: &str) -> Result<Self> {
        // pg_stat_database includes maintenance transactions, not just client
        // work. The newly initialized disposable server must disable autovacuum;
        // never infer/subtract its contributions from a mismatched result.
        let autovacuum: bool = sqlx::query_scalar("SELECT current_setting('autovacuum')::boolean")
            .fetch_one(observer)
            .await
            .context("read disposable server maintenance configuration")?;
        ensure!(
            !autovacuum,
            "independent transaction accounting requires autovacuum=off on the disposable server"
        );
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(observer)
            .await
            .context("clear observer statistics snapshot")?;
        let (commits, rollbacks, stats_reset): (i64, i64, Option<String>) = sqlx::query_as(
            "SELECT xact_commit, xact_rollback, stats_reset::text
             FROM pg_stat_database WHERE datname = $1",
        )
        .bind(measured_database)
        .fetch_one(observer)
        .await
        .context("read independent server transaction counters")?;
        let (statement_reset, deallocations): (String, i64) =
            sqlx::query_as("SELECT stats_reset::text, dealloc FROM pg_stat_statements_info")
                .fetch_one(observer)
                .await
                .context("independent pg_stat_statements instrumentation required")?;
        // This is a separate PostgreSQL mechanism profile, not an exact RTT
        // counter: repeated prepared utility statements may be under-reported.
        // Neither SQL nor bind values are retained in the report.
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT queryid, sum(calls)::bigint FROM pg_stat_statements
             WHERE dbid = (SELECT oid FROM pg_database WHERE datname = $1)
               AND toplevel
             GROUP BY queryid",
        )
        .bind(measured_database)
        .fetch_all(observer)
        .await
        .context("read independent server statement profile")?;
        Ok(Self {
            commits,
            rollbacks,
            stats_reset,
            statement_reset,
            deallocations,
            statements: rows.into_iter().collect(),
        })
    }

    pub fn delta(&self, before: &Self) -> Result<ServerDelta> {
        ensure!(
            self.stats_reset == before.stats_reset
                && self.statement_reset == before.statement_reset
                && self.deallocations == before.deallocations,
            "server statistics reset or eviction invalidates measurement"
        );
        let commits = self.commits - before.commits;
        let rollbacks = self.rollbacks - before.rollbacks;
        ensure!(commits >= 0 && rollbacks >= 0, "server counters decreased");
        let mut statements = BTreeMap::new();
        for query in before.statements.keys().chain(self.statements.keys()) {
            let count = self.statements.get(query).copied().unwrap_or(0)
                - before.statements.get(query).copied().unwrap_or(0);
            ensure!(
                count >= 0,
                "server statement counter disappeared or decreased"
            );
            if count > 0 {
                statements.insert(*query, count);
            }
        }
        Ok(ServerDelta {
            commits,
            rollbacks,
            statements,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerDelta {
    pub commits: i64,
    pub rollbacks: i64,
    pub statements: BTreeMap<i64, i64>,
}

impl ServerDelta {
    /// Transactions are not statements: a dispatch has one commit, but many
    /// statements. SQLx 0.9 omits BEGIN diagnostics, so add only those known
    /// successful dispatch BEGIN exchanges to the client statement count.
    pub fn verify(&self, dispatches: u64, sqlx_statements: u64, sqlx_commits: u64) -> Result<()> {
        let dispatches = i64::try_from(dispatches)?;
        let client = i64::try_from(sqlx_statements)?
            .checked_add(dispatches)
            .context("client statement count overflow")?;
        let server = self.statements.values().try_fold(0_i64, |sum, count| {
            ensure!(*count > 0, "server statement deltas must be positive");
            sum.checked_add(*count)
                .context("server statement count overflow")
        })?;
        ensure!(dispatches > 0, "empty pipeline measurement");
        ensure!(
            self.commits == dispatches && sqlx_commits == dispatches as u64 && self.rollbacks == 0,
            "independent server transactions disagree with successful dispatches: server={}, client={sqlx_commits}, dispatches={dispatches}, rollbacks={}",
            self.commits, self.rollbacks
        );
        ensure!(
            server == client,
            "independent server statements disagree with SQLx plus BEGIN exchanges: server={server}, client={client}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestDatabase;
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires disposable test database with JSON completion logging"]
    async fn completion_reader_accepts_a_stop_marker_appended_after_initial_eof() {
        let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
        let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
            .await
            .unwrap();
        let observer = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
            .await
            .unwrap();
        // Broad ignored/coverage sweeps log only this synthetic connection,
        // not every statement in their unrelated database suites.
        let mut writer = db.pool().acquire().await.unwrap();
        sqlx::raw_sql("SET log_min_duration_statement = 0")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::raw_sql("SELECT 'pipeline-accounting-start'")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::raw_sql("SELECT 42")
            .execute(&mut *writer)
            .await
            .unwrap();
        // Model the collector delivering the final record after the reader's
        // first EOF, without stopping a server process.
        let stop = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sqlx::raw_sql("SELECT 'pipeline-accounting-stop'")
                .execute(&mut *writer)
                .await
        });
        let profile = completed_statement_profile(observer.pool(), db.name()).await;
        let stopped = stop.await.unwrap();
        db.close().await.unwrap();
        observer.close().await.unwrap();
        stopped.unwrap();
        assert_eq!(profile.unwrap().values().sum::<i64>(), 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable test database with pg_stat_statements"]
    async fn server_snapshots_require_disabled_autovacuum() {
        let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
        let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
            .await
            .unwrap();
        let (db, observer) = add_observer(&raw, db).await.unwrap();
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .execute(observer.pool())
            .await
            .unwrap();
        let autovacuum: bool = sqlx::query_scalar("SELECT current_setting('autovacuum')::boolean")
            .fetch_one(observer.pool())
            .await
            .unwrap();
        let snapshot = ServerSnapshot::read(observer.pool(), db.name()).await;
        db.close().await.unwrap();
        observer.close().await.unwrap();
        if autovacuum {
            assert!(
                snapshot.unwrap_err().to_string().contains("autovacuum"),
                "background maintenance transactions must invalidate server accounting"
            );
        } else {
            snapshot.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable test database for verified fixture teardown"]
    async fn observer_setup_failure_awaits_first_fixture_cleanup() {
        let raw = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
        let db = TestDatabase::create(&raw, &sqlx::migrate::Migrator::DEFAULT)
            .await
            .unwrap();
        let held = db.pool().acquire().await.unwrap();
        // A checked-out connection blocks verified close. A Drop-only cleanup
        // returns early here and can be lost when the main runtime shuts down.
        let mut cleanup = tokio::spawn(add_observer("invalid fixture URL", db));
        let premature = tokio::time::timeout(Duration::from_millis(20), &mut cleanup).await;
        drop(held);
        let (returned_early, outcome) = match premature {
            Ok(result) => (true, result.unwrap()),
            Err(_) => (false, cleanup.await.unwrap()),
        };
        assert!(
            !returned_early,
            "observer error returned before verified first-fixture cleanup"
        );
        assert!(outcome.is_err(), "invalid observer URL must still fail");
    }

    #[test]
    fn durable_oracle_rejects_loss_duplicates_and_corrupt_journal() {
        let expected = BTreeSet::from(["first".into(), "second".into()]);
        let valid = vec![("first".into(), 1, 1), ("second".into(), 1, 1)];
        verify_event_rows(&expected, &valid).unwrap();
        for invalid in [
            vec![("first".into(), 1, 1)], // lost work can still have perfect journaling
            vec![("first".into(), 1, 1), ("substitute".into(), 1, 1)],
            vec![("first".into(), 1, 1), ("first".into(), 1, 1)],
            vec![("first".into(), 0, 0), ("second".into(), 1, 1)],
            vec![("first".into(), 2, 2), ("second".into(), 1, 1)],
            vec![("first".into(), 1, 0), ("second".into(), 1, 1)],
        ] {
            assert!(verify_event_rows(&expected, &invalid).is_err());
        }
    }

    fn log_record(database: &str, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "dbname":database,"message":message,"detail":"parameters deliberately ignored"
        }))
        .unwrap()
    }

    #[test]
    fn completed_log_counts_execute_and_simple_queries_but_not_parse_bind_or_other_databases() {
        let mut window = CompletionWindow::default();
        for (database, message) in [
            ("measured", "duration: 0.1 ms  statement: seed"),
            (
                "measured",
                "duration: 0.1 ms  statement: SELECT 'pipeline-accounting-start'",
            ),
            ("observer", "duration: 0.1 ms  statement: observer noise"),
            ("measured", "duration: 0.1 ms  parse sqlx_s_1: SELECT $1"),
            ("measured", "duration: 0.1 ms  bind sqlx_s_1: SELECT $1"),
            ("measured", "duration: 0.1 ms  execute sqlx_s_1: SELECT $1"),
            (
                "measured",
                "duration: 0.1 ms  execute sqlx_s_2: SAVEPOINT gateway_journal",
            ),
            (
                "measured",
                "duration: 0.1 ms  execute sqlx_s_2: SAVEPOINT gateway_journal",
            ),
            ("measured", "duration: 0.1 ms  statement: COMMIT"),
            (
                "measured",
                "duration: 0.1 ms  statement: SELECT 'pipeline-accounting-stop'",
            ),
            ("measured", "duration: 0.1 ms  statement: verification"),
        ] {
            window
                .record(&log_record(database, message), "measured")
                .unwrap();
        }
        assert_eq!(
            window.finish().unwrap(),
            BTreeMap::from([
                ("SELECT $1".into(), 1),
                ("SAVEPOINT gateway_journal".into(), 2),
                ("COMMIT".into(), 1),
            ])
        );
    }

    #[test]
    fn incomplete_ambiguous_or_erroring_server_log_windows_are_invalid() {
        let start = "duration: 0.1 ms  statement: SELECT 'pipeline-accounting-start'";
        let stop = "duration: 0.1 ms  statement: SELECT 'pipeline-accounting-stop'";
        assert!(CompletionWindow::default().finish().is_err());
        let mut window = CompletionWindow::default();
        assert!(window.record(b"not JSON", "measured").is_err());
        assert!(window
            .record(&log_record("measured", stop), "measured")
            .is_err());
        window
            .record(&log_record("measured", start), "measured")
            .unwrap();
        assert!(window
            .record(&log_record("measured", start), "measured")
            .is_err());
        let error = serde_json::to_vec(&serde_json::json!({
            "dbname":"measured","error_severity":"ERROR","message":"synthetic failed statement"
        }))
        .unwrap();
        assert!(window.record(&error, "measured").is_err());
        assert!(window.finish().is_err());
    }

    fn snapshot() -> ServerSnapshot {
        ServerSnapshot {
            commits: 20,
            rollbacks: 0,
            stats_reset: Some("reset-a".into()),
            statement_reset: "reset-b".into(),
            deallocations: 0,
            statements: BTreeMap::from([(1, 40)]),
        }
    }

    #[test]
    fn accounting_rejects_undercount_extra_work_and_transaction_statement_confusion() {
        let valid = ServerDelta {
            commits: 2,
            rollbacks: 0,
            statements: BTreeMap::from([(1, 10), (2, 2)]),
        };
        valid.verify(2, 10, 2).unwrap();
        assert!(valid.verify(2, 8, 2).is_err()); // suppressed SQLx diagnostics
        assert!(valid.verify(2, 12, 2).is_err()); // BEGIN counted twice
        assert!(valid.verify(2, 10, 1).is_err());
        assert!(valid.verify(12, 0, 12).is_err()); // transactions != statements
        for invalid in [
            ServerDelta {
                commits: 3,
                ..valid.clone()
            },
            ServerDelta {
                rollbacks: 1,
                ..valid.clone()
            },
            ServerDelta {
                statements: BTreeMap::from([(1, 13)]),
                ..valid.clone()
            },
            ServerDelta {
                statements: BTreeMap::new(),
                ..valid.clone()
            },
            ServerDelta {
                statements: BTreeMap::from([(1, -1)]),
                ..valid.clone()
            },
        ] {
            assert!(invalid.verify(2, 10, 2).is_err());
        }
        assert!(valid.verify(0, 0, 0).is_err());
        assert!(valid.verify(2, u64::MAX, 2).is_err());
    }

    #[test]
    fn server_profile_deltas_refuse_resets_evictions_and_disappearing_queries() {
        let before = snapshot();
        let after = ServerSnapshot {
            commits: 22,
            statements: BTreeMap::from([(1, 50), (2, 2)]),
            ..before.clone()
        };
        let delta = after.delta(&before).unwrap();
        assert_eq!(delta.statements, BTreeMap::from([(1, 10), (2, 2)]));
        delta.verify(2, 10, 2).unwrap();
        for invalid in [
            ServerSnapshot {
                commits: 19,
                ..after.clone()
            },
            ServerSnapshot {
                stats_reset: None,
                ..after.clone()
            },
            ServerSnapshot {
                statement_reset: "changed".into(),
                ..after.clone()
            },
            ServerSnapshot {
                deallocations: 1,
                ..after.clone()
            },
            ServerSnapshot {
                statements: BTreeMap::new(),
                ..after.clone()
            },
            ServerSnapshot {
                statements: BTreeMap::from([(1, 39)]),
                ..after.clone()
            },
        ] {
            assert!(invalid.delta(&before).is_err());
        }
    }
}
