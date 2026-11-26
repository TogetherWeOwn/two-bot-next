//! Offline reconciliation for the two PRE ledger endpoints. Set
//! PREDECESSOR_PLAN_SOURCE to the root of a stable private Git checkout
//! and run the case for the intended PRE endpoint. The opt-in path uses
//! Python 3 and local Git objects to verify the checkout before loading SQL.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use sqlx::migrate::Migrator;
use two_bot_cutover::staging_migrate::{manifest_hash, reconcile, Chain, LedgerRow};

// Migration projections from fd8e8f240190a6d690beca9ca6648e2568d0fbaf's
// reviewed SQL, with a fixed synthetic source label and no pending rows.
const PRE_SOURCE_LABEL: &str = "synthetic-pre-inventory";
const CUTOVER_0473: &str = "186ebda9ebbc1beab29be4c9e7298fb05e83c9f84460c514707ddaa2f1ad4724";
const CUTOVER_0475: &str = "bd463cd55505f968149f2dd65a845fbba3ebd2efbaecabf9aa4d4b9ea2b34754";
const STORE_0465: &str = "746e2a68cf83ccd51945aa8b3e1d7b06d96bf423ba027113a23db2e8888fb7b7";

fn through(source: &Migrator, version: i64) -> Migrator {
    Migrator::with_migrations(
        source
            .iter()
            .filter(|migration| migration.version <= version)
            .cloned()
            .collect(),
    )
}

fn synthetic_ledger(source: &Migrator, tip: i64, count: usize, projection: &str) -> Vec<LedgerRow> {
    assert_eq!(manifest_hash(PRE_SOURCE_LABEL, &[], source), projection);
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

fn check_plan_catalog(
    candidate: &Migrator,
    ledger: &[LedgerRow],
    expected_projection: &str,
) -> Result<(), String> {
    if candidate.iter().count() != ledger.len()
        || manifest_hash(PRE_SOURCE_LABEL, &[], candidate) != expected_projection
    {
        return Err("PRE planner catalog projection mismatch".to_owned());
    }
    if !reconcile(ledger, candidate)?.is_empty() {
        return Err("PRE planner catalog has pending migrations".to_owned());
    }
    Ok(())
}

fn check_plan_catalogs(
    cutover: &Migrator,
    store: &Migrator,
    cutover_ledger: &[LedgerRow],
    store_ledger: &[LedgerRow],
    cutover_projection: &str,
) -> Result<(), String> {
    check_plan_catalog(cutover, cutover_ledger, cutover_projection)?;
    check_plan_catalog(store, store_ledger, STORE_0465)
}

fn relabel_tip(source: &Migrator, ledger: &[LedgerRow]) -> Migrator {
    let mut migrations: Vec<_> = source.iter().cloned().collect();
    migrations.last_mut().expect("catalog tip").description = "different migration name".into();
    let candidate = Migrator::with_migrations(migrations);
    assert_eq!(reconcile(ledger, &candidate), Ok(vec![]));
    candidate
}

fn verify_checkout(root: &Path, cutover_tip: i64) {
    let verifier =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/retained_pre_catalog.py");
    let status = Command::new("python3")
        .args(["-E", "-B"])
        .arg(verifier)
        .arg("--repo")
        .arg(root)
        .arg("--shape")
        .arg(format!("{cutover_tip:04}/0465"))
        .status()
        .expect("run local PRE checkout verifier");
    assert!(status.success(), "PRE checkout verification refused");
}

async fn assert_pre(cutover_tip: i64, cutover_count: usize, cutover_projection: &str) {
    let cutover = Chain::Cutover.migrator().expect("embedded Cutover source");
    let store = Chain::Store.migrator().expect("embedded Store source");
    let pre_cutover = through(&cutover, cutover_tip);
    let pre_store = through(&store, 465);
    let cutover_ledger =
        synthetic_ledger(&pre_cutover, cutover_tip, cutover_count, cutover_projection);
    let store_ledger = synthetic_ledger(&pre_store, 465, 20, STORE_0465);

    let swapped = through(&cutover, if cutover_tip == 473 { 475 } else { 473 });
    for (candidate_cutover, candidate_store) in [
        (&swapped, &pre_store),
        (&relabel_tip(&pre_cutover, &cutover_ledger), &pre_store),
        (&pre_cutover, &relabel_tip(&pre_store, &store_ledger)),
    ] {
        assert_eq!(
            check_plan_catalogs(
                candidate_cutover,
                candidate_store,
                &cutover_ledger,
                &store_ledger,
                cutover_projection,
            ),
            Err("PRE planner catalog projection mismatch".to_owned())
        );
    }

    // The retained image predates applied 0472. Its own inventory cannot
    // produce the clean qualification plan, even if the Worker can boot it.
    assert_eq!(
        reconcile(&cutover_ledger, &through(&cutover, 470)),
        Err("ledger version 472 is unknown to this source".to_owned()),
    );
    let (plan_cutover, plan_store) = if let Some(root) = env::var_os("PREDECESSOR_PLAN_SOURCE") {
        let root = PathBuf::from(root);
        verify_checkout(&root, cutover_tip);
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

    check_plan_catalogs(
        &plan_cutover,
        &plan_store,
        &cutover_ledger,
        &store_ledger,
        cutover_projection,
    )
    .expect("exact Cutover and Store planner catalogs");
}

#[tokio::test]
async fn predecessor_plan_accepts_pre_0473() {
    assert_pre(473, 115, CUTOVER_0473).await;
}

#[tokio::test]
async fn predecessor_plan_accepts_pre_0475() {
    assert_pre(475, 116, CUTOVER_0475).await;
}
