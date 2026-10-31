//! Protected acceptance: the standalone capture/backfill baseline writer
//! accepts Discord u64 counters and persists the bounded INTEGER domain.
//! No live Discord calls; only the guarded disposable test database is used.

#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

use sqlx::PgPool;
use two_bot_cutover::invite_store::{store_counters, CounterRow};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const FOREIGN_GUILD: &str = "100000000000000002";
const CAPTURED: &str = "2026-10-21T00:00:00Z";
const INVITER: &str = "fixture-private-inviter";
const CHANNEL: &str = "fixture-private-channel";
const MAX: u64 = i32::MAX as u64;

type StoredRow = (String, i32, Option<String>, Option<String>, String);

fn row(code: &str, uses: u64) -> CounterRow {
    CounterRow {
        code: format!("capture-saturation-{code}"),
        uses,
        inviter_id: Some(INVITER.to_owned()),
        channel_id: Some(CHANNEL.to_owned()),
    }
}

async fn stored(pool: &PgPool, guild: &str) -> Vec<StoredRow> {
    sqlx::query_as(
        "SELECT code, uses, inviter_id, channel_id, updated_at::text
         FROM invite_snapshots WHERE guild_id = $1 ORDER BY code",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .expect("read persisted invite baseline")
}

async fn exercise(pool: &PgPool) {
    // One overflow row is an update; the others are inserts. A stale target
    // row must disappear while the same code in another guild stays intact.
    sqlx::query(
        "INSERT INTO invite_snapshots
         (guild_id, code, uses, inviter_id, channel_id, updated_at) VALUES
         ($1, 'capture-saturation-over-i32', 4, 'old-inviter', 'old-channel', '2026-01-01T00:00:00Z'),
         ($1, 'stale', 9, NULL, NULL, '2026-01-01T00:00:00Z'),
         ($2, 'capture-saturation-over-i32', 17, 'foreign-inviter', 'foreign-channel', '2026-01-01T00:00:00Z')",
    )
    .bind(GUILD)
    .bind(FOREIGN_GUILD)
    .execute(pool)
    .await
    .expect("seed target and foreign baselines");
    let foreign_before = stored(pool, FOREIGN_GUILD).await;
    let rows = [
        row("over-i32", MAX + 1),
        row("over-i64", i64::MAX as u64 + 1),
        row("u64-max", u64::MAX),
        row("exact-max", MAX),
        row("zero", 0),
        row("normal", 7),
    ];
    let capture = tracing_capture::global();

    // Repeating the checkpoint exercises update binds for every boundary,
    // not just insertion; overflow must neither abort nor wrap to negative.
    for _ in 0..2 {
        let offset = capture.text().len();
        let result = store_counters(pool, GUILD, &rows, CAPTURED).await;
        assert!(
            result.is_ok(),
            "out-of-range u64 counters must not abort the checkpoint"
        );
        let actual = stored(pool, GUILD).await;
        let counts: Vec<_> = actual.iter().map(|row| (row.0.as_str(), row.1)).collect();
        assert_eq!(
            counts,
            [
                ("capture-saturation-exact-max", i32::MAX),
                ("capture-saturation-normal", 7),
                ("capture-saturation-over-i32", i32::MAX),
                ("capture-saturation-over-i64", i32::MAX),
                ("capture-saturation-u64-max", i32::MAX),
                ("capture-saturation-zero", 0),
            ],
            "saturate only overflow counters, retaining normal values and removing the stale code"
        );
        assert!(
            actual.iter().all(|row| row.2.as_deref() == Some(INVITER)
                && row.3.as_deref() == Some(CHANNEL)
                && row.4.starts_with("2026-10-21 00:00:00")),
            "counter saturation must preserve inviter, channel and capture timestamp"
        );
        assert_eq!(
            stored(pool, FOREIGN_GUILD).await,
            foreign_before,
            "a saturated checkpoint must not change any foreign guild column"
        );

        let diagnostics = capture.text()[offset..].to_owned();
        let warnings: Vec<_> = diagnostics
            .lines()
            .filter(|line| line.starts_with("WARN") && line.contains("capture-saturation-"))
            .collect();
        assert_eq!(
            warnings.len(),
            3,
            "warn once per clamped row, not in-range rows: {warnings:?}"
        );
        for code in ["over-i32", "over-i64", "u64-max"] {
            let code = format!("capture-saturation-{code}");
            let matching: Vec<_> = warnings
                .iter()
                .filter(|line| line.contains(&code))
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "missing or repeated saturation warning for {code}"
            );
            let warning = matching[0];
            assert!(
                warning.contains(GUILD),
                "saturation warning must identify its guild"
            );
            assert!(
                warning.len() <= 512,
                "saturation diagnostic must remain bounded"
            );
            for private in [
                INVITER,
                CHANNEL,
                CAPTURED,
                FOREIGN_GUILD,
                "uses=",
                "counter=",
                "inviter_id=",
                "channel_id=",
            ] {
                assert!(
                    !warning.contains(private),
                    "warning must contain only row identifiers, not {private}"
                );
            }
        }
        for raw in [
            (MAX + 1).to_string(),
            (i64::MAX as u64 + 1).to_string(),
            u64::MAX.to_string(),
        ] {
            assert!(
                !diagnostics.contains(&raw),
                "raw overflow counters must not be logged"
            );
        }
    }
}

#[tokio::test]
async fn standalone_counter_checkpoint_saturates_without_losing_rows_or_metadata() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert!(
                std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
                    && std::env::var("CI").as_deref() != Ok("true"),
                "CI must configure TWO_TEST_DATABASE_URL for invite counter saturation"
            );
            eprintln!("SKIP invite counter saturation: TWO_TEST_DATABASE_URL not set");
            return;
        }
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("create guarded disposable invite counter fixture");
    let pool = db.pool().clone();
    let outcome = tokio::spawn(async move { exercise(&pool).await }).await;
    // Preserve assertion failures, but explicitly complete owned DB teardown
    // before unwinding: Drop at runtime shutdown is only best-effort.
    db.close().await.expect("drop owned invite counter fixture");
    match outcome {
        Ok(()) => (),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => panic!("invite counter fixture task was cancelled"),
    }
}
