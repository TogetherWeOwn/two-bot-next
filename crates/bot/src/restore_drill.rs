//! Retained scratch-only restore rehearsals. Each invocation creates a new
//! database, applies the shipped migrations, and uses the normal safety guard.
//! This is not a general-purpose database provisioner or production recovery.

// Operator commands intentionally emit human-readable output to stdout.
#![allow(clippy::print_stdout)]

use std::env;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use two_bot_core::backup::{dump, dump_file, s3};

const BOOTSTRAP: &str = "postgres://agent_test:@agent-testdb:5432/postgres";
const PG_ENV: &[&str] = &[
    "PGHOST",
    "PGHOSTADDR",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
    "PGOPTIONS",
    "PGSERVICE",
    "PGSERVICEFILE",
    "PGPASSFILE",
    "PGSSLMODE",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGAPPNAME",
];

fn authorize_bootstrap(url: &str, is_set: impl Fn(&str) -> bool) -> Result<(), &'static str> {
    // A prefix alone does not authorize a database. The only currently
    // authorized allocator is the explicitly bound disposable test service.
    if !matches!(
        url,
        BOOTSTRAP | "postgresql://agent_test:@agent-testdb:5432/postgres"
    ) {
        return Err(
            "drill bootstrap must explicitly bind the approved agent-testdb test principal",
        );
    }
    if PG_ENV.iter().any(|name| is_set(name)) {
        return Err("unset inherited libpq connection/credential variables for scratch drills");
    }
    Ok(())
}

fn options(database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database(database)
        .ssl_mode(PgSslMode::Disable)
        .options([("statement_timeout", "15000ms")])
}

fn receipt(dir: &Path, stage: &str, value: &Value) -> Result<(), &'static str> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(format!("{stage}.json")))
        .map_err(|_| "cannot exclusively create drill receipt")?;
    serde_json::to_writer_pretty(&mut file, value).map_err(|_| "cannot encode drill receipt")?;
    file.write_all(b"\n")
        .map_err(|_| "cannot write drill receipt")?;
    file.sync_all().map_err(|_| "cannot sync drill receipt")?;
    std::fs::File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "cannot sync drill evidence directory")
}

async fn restore_fresh(
    name: &str,
    archive: &Path,
    dir: &Path,
    identity: &Value,
) -> Result<Value, &'static str> {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options("postgres"))
        .await
        .map_err(|_| "cannot connect to approved scratch allocator")?;
    // Only an internally generated CSPRNG hexadecimal identifier reaches SQL.
    // CREATE has no IF NOT EXISTS: a collision refuses, never reuses a target.
    let created = sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await;
    admin.close().await;
    created.map_err(|_| "scratch allocation failed; no existing target was reused")?;
    receipt(dir, "allocated", identity)?;

    let target = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options(name))
        .await
        .map_err(|_| "cannot connect to new scratch target; target retained")?;
    let result = async {
        two_bot_cutover::db::migrate_pool(&target)
            .await
            .map_err(|_| "scratch migration failed; target retained")?;
        // S6 has not yet ported seven legacy archive tables. Rehearsal uses
        // their pinned full legacy DDL only in this new disposable target,
        // never as a foreign-feature migration or runtime initializer.
        sqlx::raw_sql(include_str!("restore_drill_schema.sql"))
            .execute(&target)
            .await
            .map_err(|_| "scratch archive compatibility failed; target retained")?;
        receipt(dir, "migrated", identity)?;
        let report = dump::restore(&target, archive)
            .await
            .map_err(|_| "scratch restore failed; target and evidence retained")?;
        if !report.ok {
            return Err("scratch restore counts differ; target and evidence retained");
        }
        Ok(json!({
            "status": "verified",
            "target_database": name,
            "archive": identity["archive"],
            "restored": report.restored,
            "dropped_columns": report.dropped_columns,
            "quarantined_unbans": report.quarantined_unbans,
            "missing_member_ban_ownership": report.missing_member_ban_ownership,
        }))
    }
    .await;
    target.close().await;
    result
}

async fn run(file: &Path, evidence_root: &Path) -> Result<PathBuf, &'static str> {
    if !evidence_root.is_absolute() {
        return Err("drill evidence directory must be an explicitly configured absolute path");
    }
    let source = std::fs::File::open(file).map_err(|_| "cannot open drill archive")?;
    if source
        .metadata()
        .map_err(|_| "cannot inspect drill archive")?
        .len()
        > dump_file::MAX_DUMP_BYTES
    {
        return Err("drill archive exceeds compressed size budget");
    }
    let name = format!("two_next_restore_drill_{:032x}", rand::random::<u128>());
    if !evidence_root.is_dir() {
        return Err(
            "drill evidence root must already exist; provision a protected retained directory",
        );
    }
    let dir = evidence_root.join(&name);
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|_| "cannot exclusively create drill directory")?;
    let archive = dir.join("archive.ndjson.gz");
    let mut retained = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&archive)
        .map_err(|_| "cannot exclusively retain drill archive")?;
    let bytes = std::io::copy(
        &mut source.take(dump_file::MAX_DUMP_BYTES + 1),
        &mut retained,
    )
    .map_err(|_| "cannot retain drill archive")?;
    retained
        .sync_all()
        .map_err(|_| "cannot sync drill archive")?;
    if bytes > dump_file::MAX_DUMP_BYTES {
        return Err("drill archive grew beyond compressed size budget");
    }
    // Inspect the retained private copy, not a path the nightly writer can
    // replace between validation, migration and restore. No DB exists yet.
    let manifest = dump_file::inspect(&archive)
        .map_err(|_| "invalid drill archive; no database provisioned")?
        .manifest;
    let archive_hash = s3::sha256_reader_hex(
        std::fs::File::open(&archive).map_err(|_| "cannot open retained archive for hashing")?,
    )
    .map_err(|_| "cannot hash retained archive")?;
    let identity = json!({
        "target_database": name,
        "scratch_service": "agent-testdb:5432",
        "archive": {"sha256": archive_hash, "bytes": bytes, "created_at": manifest.created_at},
    });
    receipt(&dir, "planned", &identity)?;
    // Make the child directory's entry durable before CREATE DATABASE can
    // commit. The protected evidence root is pre-provisioned, never invented.
    std::fs::File::open(evidence_root)
        .and_then(|root| root.sync_all())
        .map_err(|_| "cannot sync retained drill directory entry; no database provisioned")?;
    match restore_fresh(&name, &archive, &dir, &identity).await {
        Ok(verified) => {
            if verified["dropped_columns"]
                .as_object()
                .is_some_and(|columns| !columns.is_empty())
            {
                eprintln!("restore-drill: archive columns were dropped; inspect retained receipt");
            }
            receipt(&dir, "verified", &verified)?;
        }
        Err(classification) => {
            receipt(
                &dir,
                "failed",
                &json!({"identity": identity, "classification": classification}),
            )?;
            return Err(classification);
        }
    }
    Ok(dir)
}

pub async fn dispatch(args: &[String]) -> i32 {
    if args.len() != 2 || args[1] != "--confirm-scratch" || args[0].starts_with('-') {
        eprintln!("restore-drill: usage: two-bot restore-drill FILE --confirm-scratch");
        return 2;
    }
    let bootstrap = env::var("TWO_RESTORE_DRILL_BOOTSTRAP_URL").unwrap_or_default();
    if let Err(classification) = authorize_bootstrap(&bootstrap, |name| env::var_os(name).is_some())
    {
        eprintln!("restore-drill: {classification}");
        return 2;
    }
    let Some(evidence_root) = env::var_os("TWO_RESTORE_DRILL_EVIDENCE_DIR") else {
        eprintln!("restore-drill: missing TWO_RESTORE_DRILL_EVIDENCE_DIR");
        return 2;
    };
    match run(Path::new(&args[0]), Path::new(&evidence_root)).await {
        Ok(dir) => {
            println!("restore-drill: retained evidence {}", dir.display());
            println!("RESTORE VERIFIED");
            0
        }
        Err(classification) => {
            eprintln!("restore-drill: {classification}");
            eprintln!("RESTORE FAILED");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_requires_exact_test_binding_without_inherited_credentials() {
        assert!(authorize_bootstrap(BOOTSTRAP, |_| false).is_ok());
        for url in [
            "",
            "postgres://agent_test:@production:5432/postgres",
            "postgres://agent_test:@staging:5432/postgres",
            "postgres://agent_test:secret@agent-testdb:5432/postgres",
            "postgres://agent_test:@agent-testdb:5432/postgres?host=production",
            "postgres://agent_test:@agent-testdb:5432/two_next_restore_drill_previous",
        ] {
            assert!(authorize_bootstrap(url, |_| false).is_err());
        }
        for inherited in PG_ENV {
            assert!(authorize_bootstrap(BOOTSTRAP, |name| name == *inherited).is_err());
        }
    }

    #[test]
    fn receipts_never_overwrite_prior_evidence() {
        let root = PathBuf::from(
            env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
                .expect("receipt tests require run-owned scratch"),
        );
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join(format!("two-drill-receipt-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&dir).unwrap();
        receipt(&dir, "verified", &json!({"status": "first"})).unwrap();
        assert!(receipt(&dir, "verified", &json!({"status": "replacement"})).is_err());
        let value: Value =
            serde_json::from_slice(&std::fs::read(dir.join("verified.json")).unwrap()).unwrap();
        assert_eq!(value["status"], "first");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
