//! Reward-role pre-flight for the MEE6 import.
//!
//! Ports `src/leveling/rewardImport.ts`. The XP importer moves XP and
//! nothing else; the export's `role_rewards` ladder needs its own
//! pre-flight: which rewards would actually land. Pure — data in, report
//! out, no database and no network. There is deliberately no apply path in
//! this module; writes go through `replace_role_rewards` in `db`.
//!
//! Ordering worth defending: a reward is checked for grantability FIRST, and
//! only an otherwise-importable reward can be called a duplicate. A level or
//! a role is only "taken" by a reward that actually mapped, so duplicates are
//! judged against what would be WRITTEN, not what the file lists twice.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

use crate::is_snowflake;

/// Report version (legacy `REWARD_REPORT_VERSION`).
pub const REWARD_REPORT_VERSION: u32 = 1;

/// One level → role pair as it appeared in the export (legacy
/// `Mee6RoleReward`). Ids are the identity; names are advisory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mee6RoleReward {
    pub level: u64,
    pub role_id: String,
    pub role_name: Option<String>,
}

/// Why a reward cannot land (legacy `RewardSkipReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewardSkipReason {
    RoleAbsent,
    RoleEveryone,
    RoleManaged,
    AboveBotRole,
    DuplicateLevel,
    DuplicateRole,
}

/// One mappable reward (legacy `MappedReward`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappedReward {
    pub level: u64,
    pub role_id: String,
    pub role_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    pub position: i64,
}

/// One unmappable reward, always naming the concrete thing that lost
/// (legacy `UnmappedReward`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnmappedReward {
    pub level: u64,
    pub role_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_name: Option<String>,
    pub reason: RewardSkipReason,
    pub detail: String,
}

/// Counts; `rewards_in == mapped + unmapped` or the report is unfaithful
/// (legacy `RewardCounts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardCounts {
    pub rewards_in: usize,
    pub mapped: usize,
    pub unmapped: usize,
    pub by_reason: BTreeMap<String, usize>,
    pub balances: bool,
}

/// One level → role pair as stored (legacy `LevelRoleReward`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelRoleReward {
    pub level: u64,
    pub role_id: String,
}

/// Delta against stored rewards (legacy `RewardDelta`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardDelta {
    pub added: Vec<LevelRoleReward>,
    pub changed: Vec<RewardChange>,
    pub removed: Vec<LevelRoleReward>,
    pub unchanged: Vec<LevelRoleReward>,
}

/// One level re-pointed at another role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardChange {
    pub level: u64,
    pub from: String,
    pub to: String,
}

/// Full dry-run report (legacy `RewardImportReport`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardImportReport {
    pub report_version: u32,
    pub guild_id: String,
    pub mode: String,
    pub counts: RewardCounts,
    pub mapped: Vec<MappedReward>,
    pub unmapped: Vec<UnmappedReward>,
    pub apply: Vec<LevelRoleReward>,
    pub bot_role_id: Option<String>,
    pub bot_position: Option<i64>,
    pub owner_bypass: bool,
    pub delta: Option<RewardDelta>,
}

/// Every malformed reward at once (legacy `Mee6RewardExportError`).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("MEE6 role_rewards are not importable:\n  {}", problems.join("\n  "))]
pub struct Mee6RewardExportError {
    pub problems: Vec<String>,
}

/// One guild role from a snapshot (same shape as legacy `PartialRole` /
/// `audit/raw/roles.json`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SnapshotRole {
    pub id: String,
    pub name: String,
    pub position: i64,
    #[serde(default)]
    pub managed: bool,
    #[serde(default)]
    pub tags: Option<SnapshotRoleTags>,
}

/// Role tags carrying the bot owner marker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SnapshotRoleTags {
    #[serde(default)]
    pub bot_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawReward {
    rank: Option<serde_json::Value>,
    level: Option<serde_json::Value>,
    role: Option<serde_json::Value>,
    role_id: Option<serde_json::Value>,
    #[serde(rename = "roleId")]
    role_id_camel: Option<serde_json::Value>,
}

fn read_role_id(raw: &RawReward) -> (Option<String>, Option<String>) {
    if let Some(role) = raw.role.as_ref() {
        if let Some(obj) = role.as_object() {
            let id = obj.get("id").and_then(|v| v.as_str()).map(str::to_owned);
            let name = obj.get("name").and_then(|v| v.as_str()).map(str::to_owned);
            return (id, name);
        }
        if let Some(s) = role.as_str() {
            return (Some(s.to_owned()), None);
        }
    }
    let id = raw
        .role_id
        .as_ref()
        .or(raw.role_id_camel.as_ref())
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    (id, None)
}

fn as_level(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => {
            let f = n.as_u64()?;
            // Reject non-integers (e.g. 5.5 parses as f64, not u64).
            if n.as_f64().is_some_and(|x| x.fract() != 0.0) {
                return None;
            }
            Some(f)
        }
        _ => None,
    }
}

/// Parse and fully validate the `role_rewards` section of a MEE6 export. An
/// export with no `role_rewards` key parses to an empty list; a present but
/// non-array key is malformed.
pub fn parse_mee6_role_rewards(text: &str) -> Result<Vec<Mee6RoleReward>, Mee6RewardExportError> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Mee6RewardExportError {
            problems: vec![format!("file is not valid JSON: {e}")],
        })?;
    let obj = parsed.as_object().ok_or_else(|| Mee6RewardExportError {
        problems: vec!["export must be an object carrying a role_rewards array".to_owned()],
    })?;
    let section = match obj.get("role_rewards") {
        None => return Ok(Vec::new()),
        Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(v) => v,
    };
    let arr = section.as_array().ok_or_else(|| Mee6RewardExportError {
        problems: vec!["role_rewards must be an array".to_owned()],
    })?;

    let mut problems = Vec::new();
    let mut rewards = Vec::new();
    for (index, raw) in arr.iter().enumerate() {
        let at = format!("role_rewards[{index}]");
        let entry: RawReward = match serde_json::from_value(raw.clone()) {
            Ok(e) => e,
            Err(_) => {
                problems.push(format!("{at} is not an object"));
                continue;
            }
        };
        if !raw.is_object() {
            problems.push(format!("{at} is not an object"));
            continue;
        }
        let level_raw = entry.rank.as_ref().or(entry.level.as_ref());
        let Some(level_raw) = level_raw else {
            problems.push(format!("{at} has no rank or level"));
            continue;
        };
        let level = as_level(level_raw).filter(|l| *l > 0);
        let Some(level) = level else {
            problems.push(format!("{at} has an invalid level: {level_raw}"));
            continue;
        };
        let (id, name) = read_role_id(&entry);
        let Some(id) = id else {
            problems.push(format!("{at} has no role id"));
            continue;
        };
        if !is_snowflake(&id) {
            problems.push(format!("{at} has an invalid Discord role id: {id}"));
            continue;
        }
        rewards.push(Mee6RoleReward {
            level,
            role_id: id,
            role_name: name,
        });
    }
    if !problems.is_empty() {
        return Err(Mee6RewardExportError { problems });
    }
    Ok(rewards)
}

/// Parse a roles snapshot: a bare array (as `audit/raw/roles.json` is) or a
/// `{roles:[...]}` envelope.
pub fn parse_roles_snapshot(text: &str) -> Result<Vec<SnapshotRole>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("roles snapshot is not valid JSON: {e}"))?;
    let arr = if let Some(a) = parsed.as_array() {
        a.clone()
    } else if let Some(a) = parsed.get("roles").and_then(|r| r.as_array()) {
        a.clone()
    } else {
        return Err(
            "roles snapshot must be an array of roles or an object with a roles array".to_owned(),
        );
    };
    let mut problems = Vec::new();
    let mut out = Vec::new();
    for (index, raw) in arr.iter().enumerate() {
        match serde_json::from_value::<SnapshotRole>(raw.clone()) {
            Ok(role) => out.push(role),
            Err(_) => {
                let obj = raw.as_object();
                let has_id = obj.is_some_and(|o| o.get("id").is_some_and(|v| v.is_string()));
                let has_name = obj.is_some_and(|o| o.get("name").is_some_and(|v| v.is_string()));
                let name_hint: &str = obj
                    .and_then(|o| o.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                if !raw.is_object() {
                    problems.push(format!("roles[{index}] is not an object"));
                } else if !has_id || !has_name {
                    problems.push(format!("roles[{index}] needs a string id and name"));
                } else {
                    problems.push(format!(
                        "roles[{index}] (\"{name_hint}\") needs an integer position"
                    ));
                }
            }
        }
    }
    if !problems.is_empty() {
        return Err(format!(
            "roles snapshot is malformed:\n  {}",
            problems.join("\n  ")
        ));
    }
    Ok(out)
}

/// Classify every reward against the live guild. Writes nothing.
#[allow(clippy::too_many_arguments)]
pub fn plan_reward_role_import(
    guild_id: &str,
    rewards: &[Mee6RoleReward],
    roles: &[SnapshotRole],
    bot_id: &str,
    owner_id: Option<&str>,
    stored_rewards: Option<&[LevelRoleReward]>,
) -> RewardImportReport {
    let by_id: HashMap<&str, &SnapshotRole> = roles.iter().map(|r| (r.id.as_str(), r)).collect();
    let bot_role = roles.iter().find(|r| {
        r.tags
            .as_ref()
            .and_then(|t| t.bot_id.as_deref())
            .is_some_and(|id| id == bot_id)
    });
    let owner_bypass = owner_id.is_some_and(|o| o == bot_id);

    let mut mapped: Vec<MappedReward> = Vec::new();
    let mut unmapped: Vec<UnmappedReward> = Vec::new();

    // Level-first ordering makes the report byte-identical for any
    // permutation of the same export, so two runs are diffable.
    let mut ordered: Vec<&Mee6RoleReward> = rewards.iter().collect();
    ordered.sort_by(|a, b| {
        a.level
            .cmp(&b.level)
            .then_with(|| a.role_id.cmp(&b.role_id))
    });

    let mut level_claimed: HashMap<u64, &str> = HashMap::new();
    let mut role_claimed_at: HashMap<&str, u64> = HashMap::new();

    for reward in ordered {
        let (level, role_id, role_name) = (
            reward.level,
            reward.role_id.as_str(),
            reward.role_name.clone(),
        );

        let Some(live) = by_id.get(role_id) else {
            let name_suffix = role_name
                .as_deref()
                .map(|n| format!(" (\"{n}\")"))
                .unwrap_or_default();
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name,
                reason: RewardSkipReason::RoleAbsent,
                detail: format!(
                    "no role {role_id}{name_suffix} exists in guild {guild_id}; it was deleted after the export, or the export came from another guild",
                ),
            });
            continue;
        };
        if role_id == guild_id {
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name: Some(live.name.clone()),
                reason: RewardSkipReason::RoleEveryone,
                detail: format!(
                    "role \"{}\" ({role_id}) is the guild's @everyone role; it cannot be individually granted, even by the guild owner",
                    live.name
                ),
            });
            continue;
        }
        if live.managed {
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name: Some(live.name.clone()),
                reason: RewardSkipReason::RoleManaged,
                detail: format!(
                    "role \"{}\" ({role_id}) is managed by an integration; Discord does not let any bot grant it, so the reward would be stored and never land",
                    live.name
                ),
            });
            continue;
        }
        if !owner_bypass && bot_role.is_none_or(|b| live.position >= b.position) {
            let detail = match bot_role {
                Some(b) => format!(
                    "role \"{}\" is at position {}, at or above the bot's own role \"{}\" at {}; a bot grants only strictly below itself. Drag the bot's role above \"{}\" in Server Settings > Roles.",
                    live.name, live.position, b.name, b.position, live.name
                ),
                None => format!(
                    "the bot has no managed role in guild {guild_id} and does not own it, so it can grant nothing - re-invite the bot with the scoped permission link"
                ),
            };
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name: Some(live.name.clone()),
                reason: RewardSkipReason::AboveBotRole,
                detail,
            });
            continue;
        }

        if let Some(holder) = level_claimed.get(&level) {
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name: Some(live.name.clone()),
                reason: RewardSkipReason::DuplicateLevel,
                detail: format!(
                    "level {level} is already the reward for role {holder}; the table's PRIMARY KEY (guild_id, level) keeps one row per level, so importing this would silently drop role \"{}\" ({role_id})",
                    live.name
                ),
            });
            continue;
        }
        if let Some(holder) = role_claimed_at.get(role_id) {
            unmapped.push(UnmappedReward {
                level,
                role_id: role_id.to_owned(),
                role_name: Some(live.name.clone()),
                reason: RewardSkipReason::DuplicateRole,
                detail: format!(
                    "role \"{}\" ({role_id}) is already the reward for level {holder}; the table's UNIQUE (guild_id, role_id) would reject this row and abort the whole import",
                    live.name
                ),
            });
            continue;
        }

        level_claimed.insert(level, role_id);
        role_claimed_at.insert(role_id, level);
        mapped.push(MappedReward {
            level,
            role_id: role_id.to_owned(),
            renamed_from: role_name.filter(|n| *n != live.name),
            role_name: live.name.clone(),
            position: live.position,
        });
    }

    let apply: Vec<LevelRoleReward> = mapped
        .iter()
        .map(|r| LevelRoleReward {
            level: r.level,
            role_id: r.role_id.clone(),
        })
        .collect();

    let delta = stored_rewards.map(|stored| {
        let stored_map: HashMap<u64, &str> = stored
            .iter()
            .map(|r| (r.level, r.role_id.as_str()))
            .collect();
        let planned_map: HashMap<u64, &str> = apply
            .iter()
            .map(|r| (r.level, r.role_id.as_str()))
            .collect();
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let mut unchanged = Vec::new();
        for row in &apply {
            match stored_map.get(&row.level) {
                None => added.push(row.clone()),
                Some(before) if *before == row.role_id => unchanged.push(row.clone()),
                Some(before) => changed.push(RewardChange {
                    level: row.level,
                    from: (*before).to_owned(),
                    to: row.role_id.clone(),
                }),
            }
        }
        // `replace_role_rewards` deletes first, so stored-but-unplanned rows
        // are deletions the operator must see before applying.
        let removed: Vec<LevelRoleReward> = stored
            .iter()
            .filter(|r| !planned_map.contains_key(&r.level))
            .cloned()
            .collect();
        RewardDelta {
            added,
            changed,
            removed,
            unchanged,
        }
    });

    let mut by_reason: BTreeMap<String, usize> = [
        ("role_absent", 0),
        ("role_everyone", 0),
        ("role_managed", 0),
        ("above_bot_role", 0),
        ("duplicate_level", 0),
        ("duplicate_role", 0),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    for u in &unmapped {
        let key = match u.reason {
            RewardSkipReason::RoleAbsent => "role_absent",
            RewardSkipReason::RoleEveryone => "role_everyone",
            RewardSkipReason::RoleManaged => "role_managed",
            RewardSkipReason::AboveBotRole => "above_bot_role",
            RewardSkipReason::DuplicateLevel => "duplicate_level",
            RewardSkipReason::DuplicateRole => "duplicate_role",
        };
        *by_reason.get_mut(key).expect("keys fixed") += 1;
    }

    RewardImportReport {
        report_version: REWARD_REPORT_VERSION,
        guild_id: guild_id.to_owned(),
        mode: "dry-run".to_owned(),
        counts: RewardCounts {
            rewards_in: rewards.len(),
            mapped: mapped.len(),
            unmapped: unmapped.len(),
            by_reason,
            balances: rewards.len() == mapped.len() + unmapped.len(),
        },
        mapped,
        unmapped,
        apply,
        bot_role_id: bot_role.map(|r| r.id.clone()),
        bot_position: bot_role.map(|r| r.position),
        owner_bypass,
        delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = include_str!("../tests/fixtures/mee6-export-role-rewards.json");
    const ROLES: &str = include_str!("../tests/fixtures/mee6-guild-roles.json");

    const GUILD: &str = "900000000000000000";
    const BOT_ID: &str = "900000000000000001";

    fn roles() -> Vec<SnapshotRole> {
        parse_roles_snapshot(ROLES).unwrap()
    }

    #[test]
    fn fixture_is_one_real_export() {
        let xp = crate::parse_mee6_export(EXPORT).unwrap();
        let rewards = parse_mee6_role_rewards(EXPORT).unwrap();
        assert_eq!(xp.len(), 3);
        assert_eq!(rewards.len(), 7);
    }

    #[test]
    fn every_reward_classified_once_with_reason() {
        let report = plan_reward_role_import(
            GUILD,
            &parse_mee6_role_rewards(EXPORT).unwrap(),
            &roles(),
            BOT_ID,
            None,
            None,
        );
        assert_eq!(report.mode, "dry-run");
        assert_eq!(report.counts.rewards_in, 7);
        assert_eq!(report.counts.mapped, 2);
        assert_eq!(report.counts.unmapped, 5);
        assert!(report.counts.balances);
        assert_eq!(report.counts.by_reason["role_absent"], 1);
        assert_eq!(report.counts.by_reason["role_managed"], 1);
        assert_eq!(report.counts.by_reason["above_bot_role"], 1);
        assert_eq!(report.counts.by_reason["duplicate_level"], 1);
        assert_eq!(report.counts.by_reason["duplicate_role"], 1);
        assert_eq!(
            report
                .mapped
                .iter()
                .map(|r| (r.level, r.role_id.as_str(), r.role_name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (5, "900000000000000020", "Level 5"),
                (10, "900000000000000021", "Level Ten"),
            ]
        );
        assert_eq!(
            report
                .unmapped
                .iter()
                .map(|r| (r.level, r.reason))
                .collect::<Vec<_>>(),
            vec![
                (5, RewardSkipReason::DuplicateLevel),
                (15, RewardSkipReason::RoleAbsent),
                (20, RewardSkipReason::RoleManaged),
                (25, RewardSkipReason::AboveBotRole),
                (30, RewardSkipReason::DuplicateRole),
            ]
        );
        assert_eq!(
            report.apply,
            vec![
                LevelRoleReward {
                    level: 5,
                    role_id: "900000000000000020".to_owned()
                },
                LevelRoleReward {
                    level: 10,
                    role_id: "900000000000000021".to_owned()
                },
            ]
        );
    }

    #[test]
    fn everyone_is_ungrantable_even_below_bot_or_with_owner_bypass() {
        for owner_id in [None, Some(BOT_ID)] {
            let ordinary = parse_mee6_role_rewards(EXPORT).unwrap();
            let baseline =
                plan_reward_role_import(GUILD, &ordinary, &roles(), BOT_ID, owner_id, None);
            let mut rewards = ordinary.clone();
            for level in [1, 2] {
                rewards.push(Mee6RoleReward {
                    level,
                    role_id: GUILD.to_owned(),
                    role_name: Some("advisory export name".to_owned()),
                });
            }
            let report = plan_reward_role_import(GUILD, &rewards, &roles(), BOT_ID, owner_id, None);
            assert_eq!(report.owner_bypass, owner_id.is_some());
            assert_eq!(report.bot_position, Some(50));
            assert_eq!(report.mapped, baseline.mapped);
            assert_eq!(report.apply, baseline.apply);
            assert!(report.apply.iter().all(|r| r.role_id != GUILD));
            assert_eq!(report.counts.rewards_in, 9);
            assert_eq!(report.counts.mapped, baseline.counts.mapped);
            assert_eq!(report.counts.unmapped, baseline.counts.unmapped + 2);
            assert!(report.counts.unmapped > 0); // --require-all-mapped must fail.
            assert!(report.counts.balances);
            assert_eq!(report.counts.by_reason["role_everyone"], 2);
            for (reason, count) in baseline.counts.by_reason {
                if reason != "role_everyone" {
                    assert_eq!(report.counts.by_reason[&reason], count);
                }
            }
            assert_eq!(&report.unmapped[2..], baseline.unmapped.as_slice());
            for skipped in &report.unmapped[..2] {
                assert_eq!(skipped.role_id, GUILD);
                assert_eq!(skipped.role_name.as_deref(), Some("@everyone"));
                assert!(skipped.detail.contains("cannot be individually granted"));
            }
            let json = serde_json::to_value(&report).unwrap();
            assert_eq!(json["unmapped"][0]["reason"], "role_everyone");
            assert_eq!(json["counts"]["byReason"]["role_everyone"], 2);
            assert_eq!(
                serde_json::from_value::<RewardImportReport>(json).unwrap(),
                report
            );
        }
    }

    #[test]
    fn everyone_does_not_claim_a_level_or_become_a_duplicate() {
        let rewards = vec![
            Mee6RoleReward {
                level: 5,
                role_id: GUILD.to_owned(),
                role_name: None,
            },
            Mee6RoleReward {
                level: 5,
                role_id: GUILD.to_owned(),
                role_name: None,
            },
            Mee6RoleReward {
                level: 5,
                role_id: "900000000000000020".to_owned(),
                role_name: None,
            },
        ];
        for owner_id in [None, Some(BOT_ID)] {
            let report = plan_reward_role_import(GUILD, &rewards, &roles(), BOT_ID, owner_id, None);
            assert_eq!(report.counts.rewards_in, 3);
            assert_eq!(report.counts.mapped, 1);
            assert_eq!(report.counts.unmapped, 2);
            assert!(report.counts.balances);
            assert_eq!(report.counts.by_reason["role_everyone"], 2);
            assert_eq!(report.counts.by_reason["duplicate_level"], 0);
            assert_eq!(report.counts.by_reason["duplicate_role"], 0);
            assert_eq!(
                report.apply,
                vec![LevelRoleReward {
                    level: 5,
                    role_id: "900000000000000020".to_owned(),
                }]
            );
            let reversed: Vec<_> = rewards.iter().rev().cloned().collect();
            assert_eq!(
                report,
                plan_reward_role_import(GUILD, &reversed, &roles(), BOT_ID, owner_id, None)
            );
        }
    }

    #[test]
    fn renamed_role_maps_and_says_so() {
        let report = plan_reward_role_import(
            GUILD,
            &parse_mee6_role_rewards(EXPORT).unwrap(),
            &roles(),
            BOT_ID,
            None,
            None,
        );
        let renamed = report.mapped.iter().find(|r| r.level == 10).unwrap();
        assert_eq!(renamed.renamed_from.as_deref(), Some("Level 10"));
        assert_eq!(
            report
                .mapped
                .iter()
                .find(|r| r.level == 5)
                .unwrap()
                .renamed_from,
            None
        );
    }

    #[test]
    fn report_identical_for_any_permutation() {
        let rewards = parse_mee6_role_rewards(EXPORT).unwrap();
        let reversed: Vec<_> = rewards.iter().rev().cloned().collect();
        assert_eq!(
            plan_reward_role_import(GUILD, &reversed, &roles(), BOT_ID, None, None),
            plan_reward_role_import(GUILD, &rewards, &roles(), BOT_ID, None, None)
        );
    }

    #[test]
    fn owner_bypasses_hierarchy() {
        let report = plan_reward_role_import(
            GUILD,
            &parse_mee6_role_rewards(EXPORT).unwrap(),
            &roles(),
            BOT_ID,
            Some(BOT_ID),
            None,
        );
        assert!(report.owner_bypass);
        assert_eq!(report.counts.by_reason["above_bot_role"], 0);
        assert_eq!(report.counts.mapped, 3);
    }

    #[test]
    fn bot_with_no_role_grants_nothing() {
        let no_bot: Vec<SnapshotRole> = roles()
            .into_iter()
            .filter(|r| r.tags.as_ref().and_then(|t| t.bot_id.as_deref()) != Some(BOT_ID))
            .collect();
        let report = plan_reward_role_import(
            GUILD,
            &parse_mee6_role_rewards(EXPORT).unwrap(),
            &no_bot,
            BOT_ID,
            None,
            None,
        );
        assert_eq!(report.bot_role_id, None);
        assert_eq!(report.counts.mapped, 0);
        assert_eq!(report.counts.by_reason["above_bot_role"], 5);
        assert!(report.unmapped[0].detail.contains("re-invite the bot"));
    }

    #[test]
    fn role_at_exactly_bot_position_refused() {
        let mut adjusted = roles();
        for r in &mut adjusted {
            if r.name == "Level 5" {
                r.position = 50;
            }
        }
        let report = plan_reward_role_import(
            GUILD,
            &[Mee6RoleReward {
                level: 5,
                role_id: "900000000000000020".to_owned(),
                role_name: None,
            }],
            &adjusted,
            BOT_ID,
            None,
            None,
        );
        assert_eq!(report.counts.mapped, 0);
        assert_eq!(report.unmapped[0].reason, RewardSkipReason::AboveBotRole);
    }

    #[test]
    fn duplicates_judged_on_what_would_be_written() {
        let report = plan_reward_role_import(
            GUILD,
            &[
                Mee6RoleReward {
                    level: 5,
                    role_id: "900000000000000099".to_owned(),
                    role_name: None,
                },
                Mee6RoleReward {
                    level: 5,
                    role_id: "900000000000000020".to_owned(),
                    role_name: None,
                },
            ],
            &roles(),
            BOT_ID,
            None,
            None,
        );
        assert_eq!(report.counts.mapped, 1);
        assert_eq!(report.counts.by_reason["duplicate_level"], 0);
        assert_eq!(report.counts.by_reason["role_absent"], 1);
    }

    #[test]
    fn delta_names_adds_changes_removals() {
        let report = plan_reward_role_import(
            GUILD,
            &parse_mee6_role_rewards(EXPORT).unwrap(),
            &roles(),
            BOT_ID,
            None,
            Some(&[
                LevelRoleReward {
                    level: 5,
                    role_id: "900000000000000024".to_owned(),
                },
                LevelRoleReward {
                    level: 99,
                    role_id: "900000000000000023".to_owned(),
                },
            ]),
        );
        assert_eq!(
            report.delta.unwrap(),
            RewardDelta {
                added: vec![LevelRoleReward {
                    level: 10,
                    role_id: "900000000000000021".to_owned()
                }],
                changed: vec![RewardChange {
                    level: 5,
                    from: "900000000000000024".to_owned(),
                    to: "900000000000000020".to_owned()
                }],
                removed: vec![LevelRoleReward {
                    level: 99,
                    role_id: "900000000000000023".to_owned()
                }],
                unchanged: vec![],
            }
        );
    }

    #[test]
    fn malformed_export_reports_every_problem() {
        let bad = serde_json::json!({
            "role_rewards": [
                {"rank": 0, "role": {"id": "900000000000000020"}},
                {"rank": 5, "role": {"id": "not-a-snowflake"}},
                {"rank": 7},
                "nonsense",
            ]
        });
        let err = parse_mee6_role_rewards(&bad.to_string()).unwrap_err();
        assert_eq!(err.problems.len(), 4);
        assert!(err.problems[0].contains("invalid level: 0"));
        assert!(err.problems[1].contains("invalid Discord role id: not-a-snowflake"));
        assert!(err.problems[2].contains("no role id"));
        assert!(err.problems[3].contains("is not an object"));
    }

    #[test]
    fn missing_and_nonarray_sections() {
        assert!(parse_mee6_role_rewards(r#"{"players":[]}"#)
            .unwrap()
            .is_empty());
        assert!(parse_mee6_role_rewards(r#"{"role_rewards":null}"#)
            .unwrap()
            .is_empty());
        assert!(parse_mee6_role_rewards(r#"{"role_rewards":{}}"#).is_err());
        assert!(parse_mee6_role_rewards("[").is_err());
        assert!(parse_mee6_role_rewards("[]").is_err());
    }
}
