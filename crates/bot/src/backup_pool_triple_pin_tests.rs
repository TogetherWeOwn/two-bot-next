#![cfg(test)]
//! Exact-value pins for the backup/restore and guild-config admission pool
//! triples (`backup_cli.rs` review follow-up).
//!
//! The frozen acceptance tests pin each leg's existence by source substring
//! and the acquire bound behaviorally (hung-database refusal), but value
//! mutants (max 5→500, statement 15 s→3600 s) kept that suite green. These
//! pins assert the built values exactly:
//!
//! - backup pool options: max 5 + 10 s acquire via the sqlx getters.
//! - backup statement setting: the exact `60000ms` server setting the pool
//!   applies, plus a live `SHOW statement_timeout` on a connected pool.
//! - guild-config admission pool options: max 2 + 10 s acquire via getters,
//!   and the exact inherited `15000ms` admission setting.
//!
//! The live check needs `TWO_TEST_DATABASE_URL` (a disposable test binding,
//! never staging or production): it connects to the bootstrap database,
//! runs read-only `SHOW`, and closes. Without the env var it skips.

use std::time::Duration;

use super::{
    backup_pool_options, backup_statement_timeout_option, guild_config_admission_pool_options,
    guild_config_statement_timeout_option, open_pool_with_tls,
};
use two_bot_core::database_tls::TlsPolicy;

#[test]
fn backup_pool_options_pins_max_and_acquire() {
    let options = backup_pool_options();
    assert_eq!(options.get_max_connections(), 5);
    assert_eq!(
        options.get_acquire_timeout(),
        Duration::from_secs(10),
        "backup pool must refuse a hung database within the documented 10 s acquire bound"
    );
}

#[test]
fn backup_statement_timeout_option_pins_bulk_bound() {
    assert_eq!(
        backup_statement_timeout_option(),
        ("statement_timeout".to_owned(), "60000ms".to_owned()),
        "backup pool must apply the 60 s bulk statement bound, not the 15 s application value"
    );
}

#[test]
fn guild_config_admission_pool_options_pins_max_and_acquire() {
    let options = guild_config_admission_pool_options();
    assert_eq!(options.get_max_connections(), 2);
    assert_eq!(
        options.get_acquire_timeout(),
        Duration::from_secs(10),
        "guild-config admission pool must refuse a hung database within 10 s"
    );
}

#[test]
fn guild_config_statement_timeout_option_pins_admission_bound() {
    assert_eq!(
        guild_config_statement_timeout_option(),
        ("statement_timeout".to_owned(), "15000ms".to_owned()),
        "guild-config admission pool must inherit the 15 s admission statement bound"
    );
}

#[tokio::test]
async fn backup_pool_applies_statement_timeout_on_connected_pool() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("SKIP backup statement-timeout SHOW pin: TWO_TEST_DATABASE_URL not set");
            return;
        }
    };
    let pool = open_pool_with_tls(&url, TlsPolicy::LocalOnly)
        .await
        .expect("backup pool opens on the disposable test binding");
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&pool)
        .await
        .expect("SHOW statement_timeout");
    pool.close().await;
    assert_eq!(
        timeout, "1min",
        "connected backup pool must report the 60 s bulk statement bound"
    );
}
