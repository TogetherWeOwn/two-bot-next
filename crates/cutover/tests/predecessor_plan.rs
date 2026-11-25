//! Offline reconciliation for the two PRE ledger endpoints. Set
//! TWO_BOT_PREDECESSOR_PLAN_SOURCE to the source root of the proposed
//! qualification planner and run the case for the intended PRE endpoint.

use std::{env, path::PathBuf};

use sqlx::migrate::Migrator;
use two_bot_cutover::staging_migrate::{reconcile, Chain, LedgerRow};

fn through(source: &Migrator, version: i64) -> Migrator {
    Migrator::with_migrations(
        source
            .iter()
            .filter(|migration| migration.version <= version)
            .cloned()
            .collect(),
    )
}

fn synthetic_ledger(source: &Migrator, tip: i64, count: usize) -> Vec<LedgerRow> {
    let rows: Vec<_> = source
        .iter()
        .map(|migration| LedgerRow {
            version: migration.version,
            description: migration.description.to_string(),
            success: true,
            checksum_hex: hex::encode(&migration.checksum),
        })
        .collect();
    assert_eq!(rows.len(), count);
    assert_eq!(rows.last().expect("non-empty PRE ledger").version, tip);
    rows
}

async fn assert_pre(cutover_tip: i64, cutover_count: usize) {
    let cutover = Chain::Cutover.migrator().expect("embedded Cutover source");
    let store = Chain::Store.migrator().expect("embedded Store source");
    let pre_cutover = through(&cutover, cutover_tip);
    let pre_store = through(&store, 465);
    let cutover_ledger = synthetic_ledger(&pre_cutover, cutover_tip, cutover_count);
    let store_ledger = synthetic_ledger(&pre_store, 465, 20);

    // The retained image predates applied 0472. Its own inventory cannot
    // produce the clean qualification plan, even if the Worker can boot it.
    assert_eq!(
        reconcile(&cutover_ledger, &through(&cutover, 470)),
        Err("ledger version 472 is unknown to this source".to_owned()),
    );
    let (plan_cutover, plan_store) =
        if let Some(root) = env::var_os("TWO_BOT_PREDECESSOR_PLAN_SOURCE") {
            let root = PathBuf::from(root);
            (
                Migrator::new(root.join("crates/cutover/migrations"))
                    .await
                    .expect("candidate Cutover inventory"),
                Migrator::new(root.join("crates/store/migrations"))
                    .await
                    .expect("candidate Store inventory"),
            )
        } else {
            // CI exercises the PRE prefixes. The full main inventory also
            // contains later migrations and cannot produce a clean PRE plan.
            assert!(reconcile(&cutover_ledger, &cutover).unwrap().contains(&480));
            assert!(reconcile(&store_ledger, &store).unwrap().contains(&478));
            (pre_cutover, pre_store)
        };

    assert_eq!(reconcile(&cutover_ledger, &plan_cutover), Ok(vec![]));
    assert_eq!(reconcile(&store_ledger, &plan_store), Ok(vec![]));
}

#[tokio::test]
async fn predecessor_plan_accepts_pre_0473() {
    assert_pre(473, 115).await;
}

#[tokio::test]
async fn predecessor_plan_accepts_pre_0475() {
    assert_pre(475, 116).await;
}
