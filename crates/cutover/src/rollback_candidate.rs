//! Offline verification of evidence for one pinned gateway image.
//!
//! The caller supplies independently reviewed pins and a trusted, complete
//! database readout for that artifact. Build provenance, evidence collection
//! and rollout qualification belong to the caller.

use sha2::{Digest, Sha256, Sha384};
use sqlx::migrate::Migrator;
use two_bot_core::database_roles::verification_sql;

use crate::staging_migrate::{manifest_hash, reconcile, LedgerRow};

/// The candidate's reviewed access requirement. The existing role contract
/// reserves destructive reservation access and the erasure audit for the
/// separate operator path. A gateway that needs these privileges cannot pass.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PrivacyAccess {
    SeparateOperator,
    GatewayNeedsOperatorPrivileges,
}

/// Pins established together from the exact candidate, independently of the
/// supplied evidence. Ledger hashes use the existing migration manifest
/// format with an empty pending list. The candidate must share the grant
/// contract compiled into this verifier. A different historical grant
/// contract requires a verifier built with that contract.
pub struct CandidatePin<'a> {
    pub source_sha: &'a str,
    pub image_digest: &'a str,
    pub cutover_sha256: &'a str,
    pub store_sha256: &'a str,
    pub roles_sha256: &'a str,
    pub privacy_access: PrivacyAccess,
}

pub struct ArtifactEvidence<'a> {
    /// Bytes of the exact image manifest addressed by the digest, before any
    /// parsing or reformatting. Registry resolution and artifact-to-source
    /// provenance belong to the caller establishing the pins.
    pub image_manifest: &'a [u8],
    pub cutover: &'a Migrator,
    pub store: &'a Migrator,
}

/// Complete readout from one consistent snapshot, collected for the named
/// artifact. A role result is present only if the full matrix check finished.
/// History absence requires an explicit read of the 0409 table in that
/// snapshot, including when evaluating an application archive restore.
pub struct DatabaseEvidence<'a> {
    pub source_sha: &'a str,
    pub image_digest: &'a str,
    pub cutover: &'a [LedgerRow],
    pub store: &'a [LedgerRow],
    /// Digest of the rendered verifier and whether it returned zero findings.
    pub roles: Option<(&'a str, bool)>,
    pub history_rows_present: Option<bool>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("missing or mismatched artifact identity")]
    Artifact,
    #[error("cutover migration evidence does not match")]
    CutoverLedger,
    #[error("store migration evidence does not match")]
    StoreLedger,
    #[error("missing or unsuitable database access evidence")]
    Access,
    #[error("absence of unattributed Store 0409 history is unproven")]
    History,
}

/// The rendered SQL includes both the verifier and its expected grants.
#[must_use]
pub fn roles_sha256() -> String {
    hex::encode(Sha256::digest(verification_sql()))
}

/// Verify the evidence without opening files, connecting to any service, or
/// exercising candidate behavior. Success describes only these checks.
pub fn verify(
    pin: &CandidatePin<'_>,
    artifact: &ArtifactEvidence<'_>,
    database: &DatabaseEvidence<'_>,
) -> Result<(), Refusal> {
    const MAX_IMAGE_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
    if !is_git_sha(pin.source_sha)
        || database.source_sha != pin.source_sha
        || database.image_digest != pin.image_digest
        || !(1..=MAX_IMAGE_MANIFEST_BYTES).contains(&artifact.image_manifest.len())
        || pin.image_digest
            != format!(
                "sha256:{}",
                hex::encode(Sha256::digest(artifact.image_manifest))
            )
    {
        return Err(Refusal::Artifact);
    }
    if !ledger_matches(
        pin.source_sha,
        pin.cutover_sha256,
        artifact.cutover,
        database.cutover,
    ) {
        return Err(Refusal::CutoverLedger);
    }
    if !ledger_matches(
        pin.source_sha,
        pin.store_sha256,
        artifact.store,
        database.store,
    ) {
        return Err(Refusal::StoreLedger);
    }
    if pin.roles_sha256 != roles_sha256()
        || database.roles != Some((pin.roles_sha256, true))
        || pin.privacy_access != PrivacyAccess::SeparateOperator
    {
        return Err(Refusal::Access);
    }
    // Store 0409 persists no guild binding. Matching observations elsewhere
    // leave its provenance unresolved. The v17 application archive excludes
    // these rows and native PostgreSQL restore preserves them, so the refusal
    // relies on the observed table, never the archive inventory.
    if database.history_rows_present != Some(false) {
        return Err(Refusal::History);
    }
    Ok(())
}

fn is_git_sha(value: &str) -> bool {
    value.len() == 40
        && value.bytes().any(|byte| byte != b'0')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ledger_matches(
    source_sha: &str,
    expected_hash: &str,
    migrator: &Migrator,
    ledger: &[LedgerRow],
) -> bool {
    let expected_rows = migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .count();
    expected_rows > 0
        && ledger.len() == expected_rows
        && migrator.iter().all(|migration| {
            migration.migration_type.is_down_migration()
                || Sha384::digest(migration.sql.as_str()).as_slice() == migration.checksum.as_ref()
        })
        && expected_hash == manifest_hash(source_sha, &[], migrator)
        && reconcile(ledger, migrator).is_ok_and(|pending| pending.is_empty())
}
