//! Actual CLI publication regression for TOG-9970 findings 3 and 8.
//!
//! Build the unmodified bot binary, then set TWO_BOT_TEST_BACKUP_BIN to that
//! executable alongside the dedicated TWO_BOT_TEST_DATABASE_URL and run scratch.
//! Limits are set only in an isolated Python child immediately before exec;
//! the Rust test process and the invoking shell never change their limits.
#![cfg(all(feature = "db", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use two_bot_core::backup::{dump, dump_file};

fn test_url() -> Option<String> {
    let url = std::env::var("TWO_BOT_TEST_DATABASE_URL").ok()?;
    let parsed = url::Url::parse(&url).expect("test database URL");
    assert_eq!(parsed.host_str(), Some("agent-testdb"));
    assert_eq!(parsed.port().unwrap_or(5432), 5432);
    assert_eq!(parsed.username(), "agent_test");
    assert!(parsed.password().unwrap_or("").is_empty());
    assert!(parsed.query().is_none(), "no host or credential overrides");
    assert!(
        parsed.path().starts_with("/two_next_backup_publication_"),
        "use a newly created dedicated publication regression database"
    );
    Some(url)
}

fn cli(binary: &Path, url: &str, dir: &Path, marker: &Path, fault: &str) -> Output {
    // Resource/signal semantics are verified against the Python standard docs:
    // https://docs.python.org/3/library/resource.html#resource.RLIMIT_FSIZE
    // https://docs.python.org/3/library/signal.html#signal.SIG_IGN
    // https://man7.org/linux/man-pages/man2/getrlimit.2.html (SIGXFSZ/EFBIG;
    // inherited across exec; child-only RLIMIT_CORE=0 avoids core artifacts)
    // No pre_exec hook or limits in the multithreaded parent process.
    let script = r#"
import os, resource, signal, sys
fault = sys.argv[2]
if fault != 'none':
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    signal.signal(signal.SIGXFSZ, signal.SIG_IGN if fault == 'error' else signal.SIG_DFL)
    resource.setrlimit(resource.RLIMIT_FSIZE, (500, 500))
os.execv(sys.argv[1], [sys.argv[1], 'backup'])
"#;
    Command::new("/usr/bin/python3")
        .args(["-c", script])
        .arg(binary)
        .arg(fault)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TWO_DATABASE_URL", url)
        .env("TWO_BACKUP_DIR", dir)
        .env("TWO_BACKUP_KEEP", "2")
        // A local marker, not a bucket/network command. The CLI appends the
        // backup path; it must never invoke this after a failed dump.
        .env(
            "TWO_BACKUP_UPLOAD_CMD",
            format!("/usr/bin/touch {}", marker.display()),
        )
        .output()
        .expect("isolated CLI child; Linux regression requires Python resource")
}

fn candidates(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("two-funnel-") && name.ends_with(".ndjson.gz")
        })
        .collect();
    paths.sort();
    paths
}

fn drill_latest(dir: &Path) -> PathBuf {
    // Same selector as the shipped restore drill, replacing only its directory.
    let output = Command::new("/usr/bin/bash")
        .args([
            "-o",
            "pipefail",
            "-c",
            "ls -1t \"$TWO_BACKUP_DIR\"/two-funnel-*.ndjson.gz | head -1",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TWO_BACKUP_DIR", dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn save_output(dir: &Path, label: &str, output: &Output) {
    // Preserve fault evidence in run scratch even if a later assertion fails.
    std::fs::write(
        dir.join(format!("{label}.result.txt")),
        format!(
            "status={}\nstdout={}\nstderr={}\n",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump() {
    let Some(url) = test_url() else {
        eprintln!("SKIP backup_dump_publication: no dedicated test database URL");
        return;
    };
    let binary = PathBuf::from(
        std::env::var_os("TWO_BOT_TEST_BACKUP_BIN")
            .expect("set TWO_BOT_TEST_BACKUP_BIN to the newly built bot binary"),
    );
    assert!(binary.is_absolute() && binary.is_file());
    let scratch = PathBuf::from(
        std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR").expect("run-owned scratch required"),
    );
    let dir = scratch.join(format!(
        "actual-cli-dump-publication-{}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let backups = dir.join("backups");
    std::fs::create_dir(&backups).unwrap();
    let marker = dir.join("upload-invoked");
    // Only the supplied test principal; a connection error aborts without any
    // credential search or fallback. The schema lives in this new dedicated DB.
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("given test DB credential");
    sqlx::query("DROP TABLE IF EXISTS schema_migrations")
        .execute(&pool)
        .await
        .unwrap();
    for table in dump_file::DUMP_TABLES {
        // Audited identifiers come exclusively from the production allowlist.
        // A compact stand-in: what the dump needs of each table is a primary
        // key (its order) and at most one serial column (its allocator).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (id BIGSERIAL PRIMARY KEY, guild_id TEXT, member_id TEXT, \
             code TEXT, entry_id TEXT, created_at TIMESTAMPTZ, execute_at TIMESTAMPTZ, \
             request_id TEXT, channel_id TEXT, idempotency_key TEXT, occurred_at TIMESTAMPTZ, \
             audit_entry_id TEXT, started_at TIMESTAMPTZ, event_id TEXT, joined_at TIMESTAMPTZ, \
             name TEXT, ticket_id TEXT, user_id TEXT, message_id TEXT, panel_id TEXT, body TEXT)"
        )))
        .execute(&pool)
        .await
        .unwrap();
    }
    // Incompressible enough that both gzip writes and final output exceed 500.
    sqlx::query("INSERT INTO events (guild_id, body) SELECT 'g', string_agg(md5(i::text), '') FROM generate_series(1, 1000) AS i")
        .execute(&pool).await.unwrap();
    let previous = backups.join("two-funnel-previous.ndjson.gz");
    dump::dump(&pool, &previous).await.unwrap();
    let saved = std::fs::read(&previous).unwrap();
    assert!(saved.len() > 500);
    // An additional valid recovery point proves the failed CLI doesn't prune.
    let older = backups.join("two-funnel-older.ndjson.gz");
    dump::dump(&pool, &older).await.unwrap();
    let baseline = candidates(&backups);
    let baseline_latest = drill_latest(&backups);

    let failed = cli(&binary, &url, &backups, &marker, "error");
    save_output(&dir, "write-error", &failed);
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    assert!(failed.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("File too large"),
        "{failed:?}"
    );
    assert_eq!(candidates(&backups), baseline);
    assert_eq!(
        std::fs::read_dir(&backups).unwrap().count(),
        2,
        "failed temporary removed"
    );
    assert_eq!(std::fs::read(&previous).unwrap(), saved);
    assert_eq!(drill_latest(&backups), baseline_latest);
    assert!(!marker.exists(), "failed write cannot reach upload");

    let killed = cli(&binary, &url, &backups, &marker, "crash");
    save_output(&dir, "crash", &killed);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        killed.status.signal(),
        Some(25),
        "child must die from SIGXFSZ: {killed:?}"
    );
    assert_eq!(candidates(&backups), baseline);
    assert_eq!(drill_latest(&backups), baseline_latest);
    assert!(!marker.exists());
    let orphaned: Vec<_> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|path| !baseline.contains(path))
        .collect();
    assert!(
        !orphaned.is_empty(),
        "fault actually interrupted a temporary write"
    );
    for path in &orphaned {
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".dump-writing-") && name.ends_with(".tmp"));
        assert!(std::fs::metadata(path).unwrap().len() <= 500);
    }
    for path in candidates(&backups) {
        dump_file::inspect(&path).unwrap();
    }

    // Source-budget refusal must take the same no-prune/no-upload CLI path.
    sqlx::query("UPDATE events SET body = $1")
        .bind("x".repeat(dump_file::MAX_DUMP_LINE_BYTES as usize + 1))
        .execute(&pool)
        .await
        .unwrap();
    let oversized = cli(&binary, &url, &backups, &marker, "none");
    save_output(&dir, "source-budget", &oversized);
    assert_eq!(oversized.status.code(), Some(1), "{oversized:?}");
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("decoded line exceeds"));
    assert_eq!(candidates(&backups), baseline);
    assert_eq!(
        std::fs::read_dir(&backups).unwrap().count(),
        baseline.len() + orphaned.len()
    );
    assert_eq!(drill_latest(&backups), baseline_latest);
    assert!(!marker.exists());
    sqlx::query("UPDATE events SET body = 'small readable event'")
        .execute(&pool)
        .await
        .unwrap();
    let success = cli(&binary, &url, &backups, &marker, "none");
    save_output(&dir, "success", &success);
    assert!(success.status.success(), "{success:?}");
    assert!(
        marker.exists(),
        "successful fully validated archive reaches upload"
    );
    let retained = candidates(&backups);
    assert_eq!(
        retained.len(),
        2,
        "crash temporary is excluded from retention count"
    );
    for path in &retained {
        dump_file::inspect(path).unwrap();
    }
    let latest = drill_latest(&backups);
    assert!(
        !baseline.contains(&latest),
        "drill selects newly completed archive"
    );
    dump_file::inspect(&latest).unwrap();
    for path in orphaned {
        assert!(path.exists(), "retention must ignore crash temporaries");
    }
    pool.close().await;
    // Keep the bounded textual evidence above; remove only test backup files.
    std::fs::remove_dir_all(backups).unwrap();
    std::fs::remove_file(marker).unwrap();
}
