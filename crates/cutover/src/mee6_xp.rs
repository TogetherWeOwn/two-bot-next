//! MEE6 XP import: parse, plan, and reconcile.
//!
//! Ports `src/leveling/importManifest.ts`. Merge semantics (per the schema
//! comment at legacy `migrations/0010_leveling.sql:4-6`): `message_xp` /
//! `voice_xp` are never written by the import; `imported_xp` is OVERWRITTEN
//! by the export's number; `xp` is the sum. Overwrite is symmetric, so rows
//! that would LOWER a member's imported XP are declined by default and named
//! in the manifest (`--allow-lower` opts back in). The duplicate collapse is
//! max-wins and happens before the lowering check.
//!
//! The manifest proves the write matched the plan: row/XP reconciliation on
//! both sides of the write plus an inventory of the live rows the import
//! lands on top of. `reconciled: false` is the loud failure — callers exit
//! non-zero on it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;
use two_bot_core::leveling::{level_for_xp, MAX_STORED_XP};

use crate::{is_snowflake, CutoverDb};

/// Manifest version (legacy `MANIFEST_VERSION`).
pub const MANIFEST_VERSION: u32 = 1;

/// Why a row present in the export was not applied (legacy `SkipReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    DuplicateRow,
    WouldLowerImportedXp,
    ExceedsXpCeiling,
}

/// One skipped row, always naming the number that lost (legacy `SkippedRow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRow {
    pub member_id: String,
    pub xp: u64,
    pub reason: SkipReason,
    pub detail: String,
}

/// File identity: the hash covers the exact bytes parsed (legacy
/// `FileDigest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDigest {
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
}

/// Live-table stock-take the import lands on (legacy `LevelInventory`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelInventory {
    pub guild_id: String,
    pub member_rows: u64,
    pub total_xp: u64,
    pub total_organic_xp: u64,
    pub total_imported_xp: u64,
}

/// Row accounting; both identities must hold or the manifest is unfaithful
/// (legacy `ImportAccounting`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAccounting {
    pub rows_in: usize,
    pub duplicate_rows: usize,
    pub unique_members_in: usize,
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped_members: usize,
    pub balances: bool,
}

/// Full reconciliation account (legacy `ImportManifest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportManifest {
    pub manifest_version: u32,
    pub guild_id: String,
    pub mode: String,
    pub file: FileDigest,
    pub total_xp_in: u64,
    pub unique_xp_in: u64,
    pub accounting: ImportAccounting,
    pub rows_written: usize,
    pub imported_xp_written: u64,
    pub skipped: Vec<SkippedRow>,
    pub skipped_by_reason: BTreeMap<String, usize>,
    pub inventory_before: LevelInventory,
    pub inventory_after: Option<LevelInventory>,
    pub total_xp_after_projected: u64,
    pub total_xp_after_measured: Option<u64>,
    pub reconciled: bool,
    pub reconciliation_errors: Vec<String>,
    pub import_summary: Option<ImportSummary>,
}

/// Per-run audit row written to `level_import_runs` (legacy `ImportSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub source_rows: usize,
    pub unique_members: usize,
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub duplicate_rows: usize,
    pub total_imported_xp: u64,
}

/// One validated export row (legacy `Mee6ImportRow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mee6ImportRow {
    pub member_id: String,
    pub xp: u64,
    pub level: Option<u64>,
}

/// Every malformed row, not just the first (legacy `Mee6ExportError`).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("MEE6 export is not importable:\n  {}", problems.join("\n  "))]
pub struct Mee6ExportError {
    pub problems: Vec<String>,
}

/// SHA-256 hex of bytes already in hand.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Parse and fully validate a MEE6 export: an array of players, or
/// `{players:[...]}`; each player needs `id`/`user_id` and `xp`. An optional
/// `level` must agree with the MEE6 curve for that XP.
pub fn parse_mee6_export(text: &str) -> Result<Vec<Mee6ImportRow>, Mee6ExportError> {
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| Mee6ExportError {
        problems: vec![format!("file is not valid JSON: {e}")],
    })?;
    let players = if let Some(arr) = parsed.as_array() {
        arr.clone()
    } else if let Some(arr) = parsed.get("players").and_then(|p| p.as_array()) {
        arr.clone()
    } else {
        return Err(Mee6ExportError {
            problems: vec!["export must be an array or an object with a players array".to_owned()],
        });
    };

    let mut problems = Vec::new();
    let mut rows = Vec::new();
    for (index, raw) in players.iter().enumerate() {
        let at = format!("row {}", index + 1);
        let obj = match raw.as_object() {
            Some(o) => o,
            None => {
                problems.push(format!("{at} is not an object"));
                continue;
            }
        };
        let member_id = obj
            .get("id")
            .or_else(|| obj.get("user_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let Some(member_id) = member_id else {
            problems.push(format!("{at} has no id or user_id"));
            continue;
        };
        if !is_snowflake(&member_id) {
            problems.push(format!(
                "{at} has an invalid Discord member id: {member_id}"
            ));
            continue;
        }
        let xp = obj.get("xp").and_then(as_u64);
        let Some(xp) = xp else {
            problems.push(format!("{at} has invalid xp"));
            continue;
        };
        let level = match obj.get("level") {
            None => None,
            Some(v) => match as_u64(v) {
                Some(l) => Some(l),
                None => {
                    problems.push(format!("{at} has invalid level"));
                    continue;
                }
            },
        };
        if let Some(l) = level {
            if level_for_xp(xp) != l {
                problems.push(format!(
                    "{at} states level {l} but {xp} XP is level {}",
                    level_for_xp(xp)
                ));
                continue;
            }
        }
        rows.push(Mee6ImportRow {
            member_id,
            xp,
            level,
        });
    }
    if !problems.is_empty() {
        return Err(Mee6ExportError { problems });
    }
    Ok(rows)
}

/// Planned write set without touching the table (legacy `planMee6Import`).
pub struct ImportPlan {
    pub apply: Vec<Mee6ImportRow>,
    pub accounting: ImportAccounting,
    pub skipped: Vec<SkippedRow>,
    pub inventory_before: LevelInventory,
    pub total_xp_in: u64,
    pub unique_xp_in: u64,
    pub imported_xp_written: u64,
    pub total_xp_after_projected: u64,
}

struct ExistingLevel {
    organic_xp: u64,
    imported_xp: u64,
    total_xp: u64,
}

/// Classify every row against the live table without writing anything.
pub async fn plan_mee6_import(
    db: &CutoverDb,
    guild_id: &str,
    rows: &[Mee6ImportRow],
    allow_lower: bool,
) -> Result<ImportPlan, sqlx::Error> {
    let mut skipped: Vec<SkippedRow> = Vec::new();
    let mut winner: HashMap<&str, u64> = HashMap::new();
    let mut total_xp_in: u64 = 0;

    for row in rows {
        total_xp_in = total_xp_in.saturating_add(row.xp);
        match winner.get(row.member_id.as_str()) {
            None => {
                winner.insert(row.member_id.as_str(), row.xp);
            }
            Some(&held) => {
                let kept = held.max(row.xp);
                let dropped = held.min(row.xp);
                winner.insert(row.member_id.as_str(), kept);
                skipped.push(SkippedRow {
                    member_id: row.member_id.clone(),
                    xp: dropped,
                    reason: SkipReason::DuplicateRow,
                    detail: format!(
                        "member listed more than once; kept the highest XP {kept}, dropped {dropped}"
                    ),
                });
            }
        }
    }
    let duplicate_rows = skipped.len();

    let inventory_before = mee6_xp_inventory(db, guild_id).await?;
    let member_ids: Vec<&str> = winner.keys().copied().collect();
    let existing = existing_levels(db, guild_id, &member_ids).await?;

    let mut declined: Vec<String> = Vec::new();
    let (mut inserted, mut updated, mut unchanged) = (0usize, 0usize, 0usize);
    let mut unique_xp_in: u64 = 0;
    let mut imported_xp_written: u64 = 0;
    let mut projected_delta: i128 = 0;

    let mut ordered: Vec<(&str, u64)> = winner.into_iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    for (member_id, xp) in ordered {
        unique_xp_in = unique_xp_in.saturating_add(xp);
        match existing.get(member_id) {
            None => {
                inserted += 1;
                imported_xp_written = imported_xp_written.saturating_add(xp);
                projected_delta += xp as i128;
            }
            Some(live) => {
                if live.imported_xp == xp {
                    unchanged += 1;
                } else if xp < live.imported_xp && !allow_lower {
                    declined.push(member_id.to_owned());
                    skipped.push(SkippedRow {
                        member_id: member_id.to_owned(),
                        xp,
                        reason: SkipReason::WouldLowerImportedXp,
                        detail: format!(
                            "export XP {xp} is below the {} already imported for this member; \
                             applying it would drop the stored total from {} to {}. \
                             Pass --allow-lower to apply it anyway.",
                            live.imported_xp,
                            live.total_xp,
                            live.organic_xp.saturating_add(xp),
                        ),
                    });
                } else if live.organic_xp.saturating_add(xp) > MAX_STORED_XP {
                    declined.push(member_id.to_owned());
                    skipped.push(SkippedRow {
                        member_id: member_id.to_owned(),
                        xp,
                        reason: SkipReason::ExceedsXpCeiling,
                        detail: format!(
                            "organic XP {} plus imported {xp} exceeds the {MAX_STORED_XP} ceiling",
                            live.organic_xp
                        ),
                    });
                } else {
                    updated += 1;
                    imported_xp_written = imported_xp_written.saturating_add(xp);
                    projected_delta += xp as i128 - live.imported_xp as i128;
                }
            }
        }
    }

    let skipped_members = declined.len();
    let unique_members_in = member_ids.len();
    let balances = rows.len() == duplicate_rows + unique_members_in
        && unique_members_in == inserted + updated + unchanged + skipped_members;

    let declined_set: std::collections::HashSet<&str> =
        declined.iter().map(String::as_str).collect();
    let apply: Vec<Mee6ImportRow> = rows
        .iter()
        .filter(|r| !declined_set.contains(r.member_id.as_str()))
        .cloned()
        .collect();

    Ok(ImportPlan {
        apply,
        accounting: ImportAccounting {
            rows_in: rows.len(),
            duplicate_rows,
            unique_members_in,
            inserted,
            updated,
            unchanged,
            skipped_members,
            balances,
        },
        skipped,
        total_xp_after_projected: (inventory_before.total_xp as i128 + projected_delta).max(0)
            as u64,
        inventory_before,
        total_xp_in,
        unique_xp_in,
        imported_xp_written,
    })
}

/// Live-table stock-take the import lands on top of (legacy `inventory`).
pub async fn mee6_xp_inventory(
    db: &CutoverDb,
    guild_id: &str,
) -> Result<LevelInventory, sqlx::Error> {
    // SUM() over BIGINT yields NUMERIC: cast back for the Rust side.
    let row: (i64, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*),
                COALESCE(SUM(xp), 0)::BIGINT,
                COALESCE(SUM(message_xp + voice_xp), 0)::BIGINT,
                COALESCE(SUM(imported_xp), 0)::BIGINT
           FROM member_levels WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_one(db.pool())
    .await?;
    Ok(LevelInventory {
        guild_id: guild_id.to_owned(),
        member_rows: row.0 as u64,
        total_xp: row.1.unwrap_or(0) as u64,
        total_organic_xp: row.2.unwrap_or(0) as u64,
        total_imported_xp: row.3.unwrap_or(0) as u64,
    })
}

async fn existing_levels(
    db: &CutoverDb,
    guild_id: &str,
    member_ids: &[&str],
) -> Result<HashMap<String, ExistingLevel>, sqlx::Error> {
    let mut out = HashMap::new();
    for chunk in member_ids.chunks(500) {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT member_id, xp, message_xp, voice_xp, imported_xp FROM member_levels WHERE guild_id = ",
        );
        qb.push_bind(guild_id);
        qb.push(" AND member_id IN (");
        let mut sep = qb.separated(", ");
        for id in chunk {
            sep.push_bind(*id);
        }
        qb.push(")");
        let rows: Vec<(String, i64, i64, i64, i64)> =
            qb.build_query_as().fetch_all(db.pool()).await?;
        for (member_id, xp, message_xp, voice_xp, imported_xp) in rows {
            out.insert(
                member_id,
                ExistingLevel {
                    organic_xp: (message_xp as u64).saturating_add(voice_xp as u64),
                    imported_xp: imported_xp as u64,
                    total_xp: xp as u64,
                },
            );
        }
    }
    Ok(out)
}

fn tally(skipped: &[SkippedRow]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = [
        ("duplicate_row", 0),
        ("would_lower_imported_xp", 0),
        ("exceeds_xp_ceiling", 0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for row in skipped {
        let key = match row.reason {
            SkipReason::DuplicateRow => "duplicate_row",
            SkipReason::WouldLowerImportedXp => "would_lower_imported_xp",
            SkipReason::ExceedsXpCeiling => "exceeds_xp_ceiling",
        };
        *counts.get_mut(key).expect("keys fixed") += 1;
    }
    counts
}

/// Plan, optionally write, then prove the write matched the plan (legacy
/// `runMee6Import`). One read of the file, hashed and parsed together, so
/// the manifest cannot attest a checksum of bytes other than the imported.
pub async fn run_mee6_import(
    db: &CutoverDb,
    guild_id: &str,
    file_path: &str,
    file_bytes: &[u8],
    apply: bool,
    allow_lower: bool,
    imported_at: &str,
) -> Result<ImportManifest, ImportError> {
    let file = FileDigest {
        path: file_path.to_owned(),
        bytes: file_bytes.len(),
        sha256: sha256_hex(file_bytes),
    };
    let text = std::str::from_utf8(file_bytes).map_err(|e| {
        ImportError::Export(Mee6ExportError {
            problems: vec![format!("file is not valid UTF-8: {e}")],
        })
    })?;
    let rows = parse_mee6_export(text).map_err(ImportError::Export)?;
    let plan = plan_mee6_import(db, guild_id, &rows, allow_lower)
        .await
        .map_err(ImportError::Db)?;

    let mut reconciliation_errors: Vec<String> = Vec::new();
    if !plan.accounting.balances {
        reconciliation_errors.push(format!(
            "row accounting does not balance: {:?}",
            plan.accounting
        ));
    }

    let (import_summary, inventory_after, total_xp_after_measured) = if apply {
        let summary = apply_mee6_import(db, guild_id, &plan.apply, imported_at).await?;
        let after = mee6_xp_inventory(db, guild_id)
            .await
            .map_err(ImportError::Db)?;
        let measured = after.total_xp;
        if measured != plan.total_xp_after_projected {
            reconciliation_errors.push(format!(
                "total XP after the write is {measured}, projected {}",
                plan.total_xp_after_projected
            ));
        }
        for (field, planned, actual) in [
            ("inserted", plan.accounting.inserted, summary.inserted),
            ("updated", plan.accounting.updated, summary.updated),
            ("unchanged", plan.accounting.unchanged, summary.unchanged),
        ] {
            if actual != planned {
                reconciliation_errors.push(format!(
                    "service reported {field}={actual}, planned {planned}"
                ));
            }
        }
        (Some(summary), Some(after), Some(measured))
    } else {
        (None, None, None)
    };

    Ok(ImportManifest {
        manifest_version: MANIFEST_VERSION,
        guild_id: guild_id.to_owned(),
        mode: if apply {
            "apply".to_owned()
        } else {
            "dry-run".to_owned()
        },
        file,
        total_xp_in: plan.total_xp_in,
        unique_xp_in: plan.unique_xp_in,
        rows_written: plan.accounting.inserted + plan.accounting.updated,
        imported_xp_written: plan.imported_xp_written,
        skipped_by_reason: tally(&plan.skipped),
        skipped: plan.skipped,
        inventory_before: plan.inventory_before,
        inventory_after,
        total_xp_after_projected: plan.total_xp_after_projected,
        total_xp_after_measured,
        reconciled: reconciliation_errors.is_empty(),
        reconciliation_errors,
        accounting: plan.accounting,
        import_summary,
    })
}

/// Import failure: malformed export vs invalid rows vs database error.
#[derive(Debug, Error)]
pub enum ImportError {
    #[error(transparent)]
    Export(#[from] Mee6ExportError),
    #[error("{0}")]
    Invalid(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Write the planned rows (legacy `LevelingService.importMee6`): max-wins
/// duplicate collapse, overwrite of `imported_xp` preserving organic columns,
/// ceiling guards, and the `level_import_runs` audit row — all in one
/// transaction.
pub async fn apply_mee6_import(
    db: &CutoverDb,
    guild_id: &str,
    rows: &[Mee6ImportRow],
    imported_at: &str,
) -> Result<ImportSummary, ImportError> {
    let mut by_member: BTreeMap<&str, u64> = BTreeMap::new();
    let mut duplicate_rows = 0usize;
    for row in rows {
        if !is_snowflake(&row.member_id) {
            return Err(ImportError::Invalid(format!(
                "invalid Discord member id: {}",
                row.member_id
            )));
        }
        if row.xp > MAX_STORED_XP {
            return Err(ImportError::Invalid(format!(
                "xp must be an integer between 0 and {MAX_STORED_XP}"
            )));
        }
        if let Some(l) = row.level {
            if level_for_xp(row.xp) != l {
                return Err(ImportError::Invalid(format!(
                    "MEE6 row level does not match XP for member {}",
                    row.member_id
                )));
            }
        }
        if by_member.contains_key(row.member_id.as_str()) {
            duplicate_rows += 1;
        }
        let held = by_member.get(row.member_id.as_str()).copied().unwrap_or(0);
        by_member.insert(row.member_id.as_str(), held.max(row.xp));
    }

    let mut tx = db.pool().begin().await?;
    let (mut inserted, mut updated, mut unchanged) = (0usize, 0usize, 0usize);
    let mut total_imported_xp: u64 = 0;

    for (member_id, xp) in &by_member {
        total_imported_xp = total_imported_xp.checked_add(*xp).ok_or_else(|| {
            ImportError::Invalid("total imported XP exceeds the safe integer range".to_owned())
        })?;
        let created: Option<(String,)> = sqlx::query_as(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
             VALUES ($1, $2, $3, 0, 0, $4, $5::timestamptz)
             ON CONFLICT (guild_id, member_id) DO NOTHING RETURNING member_id",
        )
        .bind(guild_id)
        .bind(*member_id)
        .bind(*xp as i64)
        .bind(*xp as i64)
        .bind(imported_at)
        .fetch_optional(&mut *tx)
        .await?;
        if created.is_some() {
            inserted += 1;
            continue;
        }
        let changed: Option<(i64,)> = sqlx::query_as(
            "UPDATE member_levels SET imported_xp = $1, xp = message_xp + voice_xp + $2, updated_at = $3::timestamptz
             WHERE guild_id = $4 AND member_id = $5 AND imported_xp <> $6
               AND message_xp + voice_xp <= $7 RETURNING imported_xp",
        )
        .bind(*xp as i64)
        .bind(*xp as i64)
        .bind(imported_at)
        .bind(guild_id)
        .bind(*member_id)
        .bind(*xp as i64)
        .bind(MAX_STORED_XP as i64 - *xp as i64)
        .fetch_optional(&mut *tx)
        .await?;
        if changed.is_some() {
            updated += 1;
            continue;
        }
        let current: Option<(i64,)> = sqlx::query_as(
            "SELECT imported_xp FROM member_levels WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(guild_id)
        .bind(*member_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current.map(|(v,)| v as u64) == Some(*xp) {
            unchanged += 1;
            continue;
        }
        return Err(ImportError::Invalid(format!(
            "imported XP plus organic XP exceeds {MAX_STORED_XP} for member {member_id}"
        )));
    }

    sqlx::query(
        "INSERT INTO level_import_runs
           (guild_id, source, source_rows, unique_members, inserted, updated, unchanged,
            duplicate_rows, total_imported_xp, imported_at)
         VALUES ($1, 'mee6', $2, $3, $4, $5, $6, $7, $8, $9::timestamptz)",
    )
    .bind(guild_id)
    .bind(rows.len() as i64)
    .bind(by_member.len() as i64)
    .bind(inserted as i64)
    .bind(updated as i64)
    .bind(unchanged as i64)
    .bind(duplicate_rows as i64)
    .bind(total_imported_xp as i64)
    .bind(imported_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(ImportSummary {
        source_rows: rows.len(),
        unique_members: by_member.len(),
        inserted,
        updated,
        unchanged,
        duplicate_rows,
        total_imported_xp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn parse_accepts_array_and_envelope() {
        let rows = parse_mee6_export(r#"[{"id":"100000000000000001","xp":100}]"#).unwrap();
        assert_eq!(rows.len(), 1);
        let rows = parse_mee6_export(
            r#"{"players":[{"user_id":"100000000000000001","xp":100,"level":1}]}"#,
        )
        .unwrap();
        assert_eq!(rows[0].level, Some(1));
    }

    #[test]
    fn parse_reports_every_problem() {
        let err = parse_mee6_export(
            r#"[{"xp":1},{"id":"nope","xp":1},{"id":"100000000000000001","xp":-1},{"id":"100000000000000001","xp":100,"level":9}]"#,
        )
        .unwrap_err();
        assert_eq!(err.problems.len(), 4);
    }

    #[test]
    fn parse_rejects_level_xp_mismatch() {
        // 100 XP is level 1 on the MEE6 curve, not level 5.
        let err =
            parse_mee6_export(r#"[{"id":"100000000000000001","xp":100,"level":5}]"#).unwrap_err();
        assert!(err.problems[0].contains("states level 5"));
    }

    #[test]
    fn is_snowflake_bounds() {
        assert!(is_snowflake("100000000000000001"));
        assert!(!is_snowflake("123"));
        assert!(!is_snowflake("10000000000000000123456789"));
        assert!(!is_snowflake("not-a-snowflake"));
    }
}
