//! Only agent-testdb or the ephemeral CI service, never an application URL.
//! Controller: python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test legacy_verify_db -- --ignored

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::{
    postgres::{PgConnectOptions, PgSslMode},
    Connection, PgConnection,
};
use two_bot_cutover::{
    legacy_mapping::MappingSpec,
    legacy_verify::{read_only_transaction, verify, VerificationReport},
};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn test_database_url_allowed(url: &str, github_actions: bool) -> bool {
    url == "postgres://agent_test@agent-testdb:5432/agent_test"
        || (github_actions && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
}

struct ScratchDatabases {
    admin: PgConnection,
    options: PgConnectOptions,
    source: String,
    target: String,
    host: String,
}

impl ScratchDatabases {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Same exact-URL guard as channel_moderation_store. The URL is used
        // only to select the fixed test host, NEVER parsed as connection options.
        let url = std::env::var("TWO_TEST_DATABASE_URL")?;
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        assert!(
            test_database_url_allowed(&url, ci),
            "non-test database refused"
        );
        for key in ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"] {
            assert!(
                std::env::var_os(key).is_none(),
                "inherited PG settings refused"
            );
        }
        let host = if url.contains("@127.0.0.1:") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test")
            .ssl_mode(PgSslMode::Disable);
        // No fallback on authentication/ownership failure.
        let mut admin = PgConnection::connect_with(&options).await?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let prefix = format!("verify_{}_{}", std::process::id(), nonce);
        let source = format!("{prefix}_source");
        let target = format!("{prefix}_target");
        for database in [&source, &target] {
            // Names contain only our constant prefix and decimal process/time IDs.
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
                .execute(&mut admin)
                .await?;
        }
        Ok(Self {
            admin,
            options,
            source,
            target,
            host: host.to_owned(),
        })
    }

    async fn finish(mut self) -> TestResult {
        // Exact two databases just created by this test, never shared app data.
        for database in [&self.source, &self.target] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {database} WITH (FORCE)"
            )))
            .execute(&mut self.admin)
            .await?;
        }
        self.admin.close().await?;
        Ok(())
    }
}

const SOURCE_FIXTURE: &str = r#"
CREATE TABLE legacy_members (
    id text PRIMARY KEY, enabled text NOT NULL, joined_at text NOT NULL,
    metadata text, display_name text
);
INSERT INTO legacy_members VALUES
    ('1','1','2026-09-30T03:00:00.123456+03:00','{"z":[true,{"b":2,"a":1}],"n":100.00,"large":184467440737095516160}',NULL),
    ('2','false','2026-09-30T00:00:01Z','null','null'),
    ('3','t','2026-09-30T00:00:02Z',NULL,'Zoë 🎮');
"#;
const TARGET_FIXTURE: &str = r#"
CREATE TABLE next_members (
    member_id bigint PRIMARY KEY, enabled boolean NOT NULL, joined_at timestamptz NOT NULL,
    metadata jsonb, display_name text
);
INSERT INTO next_members VALUES
    (1,true,'2026-09-30T00:00:00.123456Z','{"large":184467440737095516160,"n":1e2,"z":[true,{"a":1,"b":2}]}',NULL),
    (2,false,'2026-09-29T20:00:01-04:00','null','null'),
    (3,true,'2026-09-30T00:00:02Z',NULL,'Zoë 🎮');
"#;

async fn report(
    source: &mut PgConnection,
    target: &mut PgConnection,
) -> Result<VerificationReport, Box<dyn std::error::Error + Send + Sync>> {
    let spec = MappingSpec::parse(include_str!("../mappings/example.json"))?;
    Ok(verify(source, target, &spec, &[], 10).await?)
}

async fn cli(host: &str, source: &str, target: &str, expected: i32) -> TestResult {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_legacy_verify"))
        .args([
            "--source-url-env",
            "VERIFY_SOURCE_URL",
            "--target-url-env",
            "VERIFY_TARGET_URL",
            "--mapping",
            concat!(env!("CARGO_MANIFEST_DIR"), "/mappings/example.json"),
        ])
        .env(
            "VERIFY_SOURCE_URL",
            format!("postgres://agent_test@{host}:5432/{source}"),
        )
        .env(
            "VERIFY_TARGET_URL",
            format!("postgres://agent_test@{host}:5432/{target}"),
        )
        .output()
        .await?;
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if expected == 2 {
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate normalized key"));
        return Ok(());
    }
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["matches"], expected == 0);
    Ok(())
}

async fn scenarios(
    options: PgConnectOptions,
    host: String,
    source_name: String,
    target_name: String,
) -> TestResult {
    let mut source = PgConnection::connect_with(&options.clone().database(&source_name)).await?;
    let mut target = PgConnection::connect_with(&options.database(&target_name)).await?;
    sqlx::raw_sql(SOURCE_FIXTURE).execute(&mut source).await?;
    sqlx::raw_sql(TARGET_FIXTURE).execute(&mut target).await?;

    let equal = report(&mut source, &mut target).await?;
    assert!(equal.matches);
    assert_eq!(equal.tables[0].source_rows, 3);
    assert_eq!(equal.tables[0].target_rows, 3);
    assert!(equal.tables[0].columns.iter().all(|c| c.matches));
    cli(&host, &source_name, &target_name, 0).await?;

    sqlx::query("DELETE FROM next_members WHERE member_id = 2")
        .execute(&mut target)
        .await?;
    let missing = report(&mut source, &mut target).await?;
    assert!(!missing.matches);
    assert_eq!(missing.tables[0].target_rows, 2);
    assert_eq!(missing.tables[0].missing_in_target.count, 1);
    assert_eq!(
        missing.tables[0].missing_in_target.sample,
        [vec!["2".to_owned()]]
    );
    assert_eq!(missing.tables[0].extra_in_target.count, 0);
    cli(&host, &source_name, &target_name, 1).await?;
    sqlx::query("INSERT INTO next_members VALUES (2,false,'2026-09-30T00:00:01Z','null','null')")
        .execute(&mut target)
        .await?;

    sqlx::query("UPDATE next_members SET display_name = 'changed' WHERE member_id = 3")
        .execute(&mut target)
        .await?;
    let changed = report(&mut source, &mut target).await?;
    assert!(!changed.matches);
    assert_eq!(changed.tables[0].missing_in_target.count, 0);
    assert_eq!(changed.tables[0].extra_in_target.count, 0);
    assert_eq!(
        changed.tables[0]
            .columns
            .iter()
            .filter(|c| !c.matches)
            .map(|c| c.target.as_str())
            .collect::<Vec<_>>(),
        ["display_name"]
    );
    cli(&host, &source_name, &target_name, 1).await?;
    sqlx::query("UPDATE next_members SET display_name = 'Zoë 🎮' WHERE member_id = 3")
        .execute(&mut target)
        .await?;

    sqlx::query("INSERT INTO next_members SELECT 4, enabled, joined_at, metadata, display_name FROM next_members WHERE member_id = 3").execute(&mut target).await?;
    let extra = report(&mut source, &mut target).await?;
    assert!(!extra.matches);
    assert_eq!(extra.tables[0].target_rows, 4);
    assert_eq!(extra.tables[0].missing_in_target.count, 0);
    assert_eq!(extra.tables[0].extra_in_target.count, 1);
    assert_eq!(
        extra.tables[0].extra_in_target.sample,
        [vec!["4".to_owned()]]
    );
    cli(&host, &source_name, &target_name, 1).await?;

    sqlx::query("DELETE FROM next_members WHERE member_id = 2")
        .execute(&mut target)
        .await?;
    let replaced = report(&mut source, &mut target).await?;
    assert_eq!(
        replaced.tables[0].source_rows,
        replaced.tables[0].target_rows
    );
    assert!(!replaced.matches);
    assert_eq!(replaced.tables[0].missing_in_target.count, 1);
    assert_eq!(replaced.tables[0].extra_in_target.count, 1);

    // Enforce the exact verifier setup on BOTH endpoints, not just inspect SQL.
    for connection in [&mut source, &mut target] {
        let mut tx = read_only_transaction(connection).await?;
        let readonly: String = sqlx::query_scalar("SHOW transaction_read_only")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(readonly, "on");
        let error = sqlx::query("CREATE TABLE forbidden_write (id integer)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("25006")
        );
        tx.rollback().await?;
    }

    // Normalized duplicate keys must refuse, never accidentally balance counts.
    sqlx::query("INSERT INTO legacy_members SELECT '01',enabled,joined_at,metadata,display_name FROM legacy_members WHERE id = '1'").execute(&mut source).await?;
    assert!(report(&mut source, &mut target)
        .await
        .unwrap_err()
        .to_string()
        .contains("duplicate normalized key"));
    cli(&host, &source_name, &target_name, 2).await?;
    sqlx::query("DELETE FROM legacy_members WHERE id = '01'")
        .execute(&mut source)
        .await?;
    sqlx::query("DELETE FROM next_members WHERE member_id = 4")
        .execute(&mut target)
        .await?;
    sqlx::query("INSERT INTO next_members VALUES (2,false,'2026-09-30T00:00:01Z','null','null')")
        .execute(&mut target)
        .await?;

    // Cross the 1024-row cursor boundary with fixtures in different insert order.
    sqlx::query("INSERT INTO legacy_members SELECT n::text,'true','2026-09-30T00:00:00Z','{}',n::text FROM generate_series(10,1110) n ORDER BY n DESC").execute(&mut source).await?;
    sqlx::query("INSERT INTO next_members SELECT n,true,'2026-09-30T00:00:00Z','{}',n::text FROM generate_series(10,1110) n").execute(&mut target).await?;
    let paged = report(&mut source, &mut target).await?;
    assert!(paged.matches);
    assert_eq!(paged.tables[0].source_rows, 1104);

    sqlx::query("TRUNCATE legacy_members")
        .execute(&mut source)
        .await?;
    sqlx::query("TRUNCATE next_members")
        .execute(&mut target)
        .await?;
    let empty = report(&mut source, &mut target).await?;
    assert!(empty.matches);
    assert_eq!(empty.tables[0].source_rows, 0);
    source.close().await?;
    target.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI service behind TWO_TEST_DATABASE_URL guard"]
async fn parity_mismatch_cursor_and_read_only_acceptance() -> TestResult {
    let db = ScratchDatabases::new().await?;
    // A spawned task captures assertion panics, allowing cleanup before the
    // test propagates either a panic or a query failure.
    let result = tokio::spawn(scenarios(
        db.options.clone(),
        db.host.clone(),
        db.source.clone(),
        db.target.clone(),
    ))
    .await;
    db.finish().await?;
    result??;
    Ok(())
}

#[test]
fn guard_rejects_real_endpoints_and_url_overrides() {
    assert!(test_database_url_allowed(
        "postgres://agent_test@agent-testdb:5432/agent_test",
        false
    ));
    assert!(test_database_url_allowed(
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        true
    ));
    for url in [
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        "postgres://agent_test@staging:5432/agent_test",
        "postgres://agent_test@agent-testdb:5432/production",
        "postgres://admin@agent-testdb:5432/agent_test",
        "postgres://agent_test@agent-testdb:5432/agent_test?host=production",
    ] {
        assert!(!test_database_url_allowed(url, false));
    }
}
