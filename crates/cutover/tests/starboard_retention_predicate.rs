//! Starboard erasure-predicate regression (Test Author-owned).
//!
//! `starboard_starrers` uses BIGINT snowflake columns while the erasure and
//! retention queues supply decimal TEXT binds. The predicates must cast the
//! binds (`$1::bigint`), never the indexed columns (`guild_id::text`), or
//! Postgres raises an operator mismatch (bigint = text) and a column cast
//! would defeat the index.
//!
//! This pins the public erasure plan (`member_erasure_plan.json` via
//! `two_bot_cutover::member_erasure::plan`). The private retention purge
//! (`guild_purge_predicate("starboard_starrers")` in `retention.rs`) must
//! match with `guild_id = $1::bigint`; it is builder-owned source, covered
//! indirectly because every DB-backed `purge_guild` run plans that DELETE
//! and would fail with the same operator mismatch if the cast were missing.
//! `starboard_config.updated_by` stays a retained-attribution allowlist and
//! `starboard_posts` carries no member column, so neither appears in tables.

use two_bot_cutover::member_erasure::plan;

#[test]
fn starboard_starrers_erasure_predicate_casts_text_binds_to_bigint() {
    let erasure = plan();
    let entry = erasure
        .tables
        .iter()
        .find(|entry| entry.table == "starboard_starrers")
        .expect("erasure plan must cover starboard_starrers");
    assert_eq!(
        entry.columns,
        vec!["user_id"],
        "starrers erasure scopes the member column only"
    );
    assert!(
        entry.predicate.contains("$1::bigint"),
        "guild bind must cast TEXT to BIGINT, got: {}",
        entry.predicate
    );
    assert!(
        entry.predicate.contains("$2::bigint"),
        "member bind must cast TEXT to BIGINT, got: {}",
        entry.predicate
    );
    assert!(
        !entry.predicate.contains("guild_id::text"),
        "must not cast the indexed guild column, got: {}",
        entry.predicate
    );
    assert!(
        !entry.predicate.contains("user_id::text"),
        "must not cast the indexed member column, got: {}",
        entry.predicate
    );
}

#[test]
fn starboard_config_attribution_stays_allowlisted_and_posts_stay_out_of_scope() {
    let erasure = plan();
    assert!(
        erasure.allowlist.iter().any(|exception| {
            exception.table == "starboard_config" && exception.column == "updated_by"
        }),
        "starboard_config.updated_by must stay retained attribution"
    );
    assert!(
        !erasure
            .tables
            .iter()
            .any(|entry| entry.table == "starboard_config" || entry.table == "starboard_posts"),
        "config and posts carry no member rows to erase"
    );
}
