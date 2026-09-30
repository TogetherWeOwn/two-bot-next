//! On-demand raid evidence export and audited, manually approved removal.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};
use two_bot_core::{
    containment::DANGEROUS_PERMISSIONS,
    moderation::{moderation_target_protection, ModerationPolicy, ModerationTarget},
    raid_removal::{validate_targets, RemovalMode, TargetState},
    KickOutcome,
};
use two_bot_discord::executor::ActionExecutor;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CohortRow {
    pub guild_id: String,
    pub member_id: String,
    pub joined_at: String,
    pub score: i32,
    pub source: String,
    pub reasons_json: String,
}

pub fn window(from: &str, to: &str) -> Result<(), String> {
    let parse = |s| time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339);
    let from = parse(from).map_err(|_| "--from must be RFC3339")?;
    let to = parse(to).map_err(|_| "--to must be RFC3339")?;
    if from >= to {
        return Err("--from must precede --to".into());
    }
    Ok(())
}

/// Read-only query: risk flags identify candidates, funnel activity protects
/// real participants. Fresh live protection is still mandatory at execution.
pub async fn list_flagged(
    pool: &PgPool,
    guild: &str,
    from: &str,
    to: &str,
) -> Result<Vec<CohortRow>, sqlx::Error> {
    window(from, to).map_err(sqlx::Error::InvalidArgument)?;
    let rows = sqlx::query(
        "SELECT DISTINCT ON (r.member_id) r.guild_id, r.member_id, r.joined_at, r.score, r.source, r.reasons_json
         FROM join_risk_flags r
         WHERE r.guild_id = $1 AND r.flagged AND NOT r.bulk_join_window
           AND r.joined_at::timestamptz >= $2::timestamptz AND r.joined_at::timestamptz < $3::timestamptz
           AND NOT EXISTS (SELECT 1 FROM members m WHERE m.guild_id = r.guild_id AND m.member_id = r.member_id
             AND (m.is_bot OR m.left_at IS NOT NULL OR m.first_message_at IS NOT NULL OR m.first_voice_at IS NOT NULL))
           AND NOT EXISTS (SELECT 1 FROM events e WHERE e.guild_id = r.guild_id AND e.member_id = r.member_id
             AND e.event_type IN ('first_message', 'third_message', 'first_voice_session', 'first_voice', 'voice_session_start'))
         ORDER BY r.member_id, r.joined_at::timestamptz, r.event_id"
    ).bind(guild).bind(from).bind(to).fetch_all(pool).await?;
    rows.iter()
        .map(|r| {
            Ok(CohortRow {
                guild_id: r.try_get("guild_id")?,
                member_id: r.try_get("member_id")?,
                joined_at: r.try_get("joined_at")?,
                score: r.try_get("score")?,
                source: r.try_get("source")?,
                reasons_json: r.try_get("reasons_json")?,
            })
        })
        .collect()
}

/// CSV deliberately carries only safe scalar provenance. Full evidence is JSON.
pub fn cohort_csv(rows: &[CohortRow]) -> Result<String, String> {
    let mut out = "guild_id,member_id,joined_at,score\n".to_owned();
    for r in rows {
        validate_targets(vec![r.guild_id.clone(), r.member_id.clone()])?;
        time::OffsetDateTime::parse(&r.joined_at, &time::format_description::well_known::Rfc3339)
            .map_err(|_| "invalid join timestamp")?;
        out.push_str(&format!(
            "{},{},{},{}\n",
            r.guild_id, r.member_id, r.joined_at, r.score
        ));
    }
    Ok(out)
}

/// Reject malformed entries and cross-guild evidence before any network call.
pub fn parse_targets(text: &str, guild: &str) -> Result<Vec<String>, String> {
    let text = text.trim();
    let ids = if text.starts_with('[') {
        let entries: Vec<Value> =
            serde_json::from_str(text).map_err(|_| "invalid JSON target list")?;
        entries
            .iter()
            .map(|v| {
                if let Some(id) = v.as_str() {
                    return Ok(id.to_owned());
                }
                if let Some(g) = v.get("guild_id") {
                    if g.as_str() != Some(guild) {
                        return Err("target list belongs to a different guild".to_owned());
                    }
                }
                v.get("member_id")
                    .or_else(|| v.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "target IDs must be strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if text.starts_with("guild_id,member_id,joined_at,score") {
        text.lines()
            .skip(1)
            .map(|line| {
                let cells: Vec<_> = line.trim_end_matches('\r').split(',').collect();
                if cells.len() != 4 || cells[0] != guild {
                    return Err("invalid or cross-guild CSV row".to_owned());
                }
                time::OffsetDateTime::parse(
                    cells[2],
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| "invalid CSV timestamp")?;
                cells[3].parse::<u32>().map_err(|_| "invalid CSV score")?;
                Ok(cells[1].to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        text.lines()
            .filter_map(|line| {
                let value = line.split('#').next().unwrap_or_default().trim();
                (!value.is_empty()).then(|| value.to_owned())
            })
            .collect()
    };
    validate_targets(ids).map_err(str::to_owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalOutcome {
    WouldKick,
    Kicked,
    AlreadyGone,
    Protected,
    Forbidden,
    RateLimited,
    Failed,
}
impl RemovalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WouldKick => "would_kick",
            Self::Kicked => "kicked",
            Self::AlreadyGone => "already_gone",
            Self::Protected => "protected",
            Self::Forbidden => "forbidden",
            Self::RateLimited => "rate_limited",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalRecord {
    pub v: u8,
    pub ts: String,
    pub run_id: String,
    pub guild_id: String,
    pub member_id: String,
    pub mode: RemovalMode,
    pub action: String,
    pub outcome: RemovalOutcome,
    pub status: Option<u16>,
    pub attempts: u32,
}

impl RemovalRecord {
    fn validate(&self) -> Result<(), String> {
        if self.v != 1 || self.action != "kick" {
            return Err("unsupported audit schema".into());
        }
        validate_targets(vec![self.guild_id.clone(), self.member_id.clone()])?;
        time::OffsetDateTime::parse(&self.ts, &time::format_description::well_known::Rfc3339)
            .map_err(|_| "invalid audit timestamp")?;
        let valid_mode = match self.mode {
            RemovalMode::DryRun => {
                self.outcome == RemovalOutcome::WouldKick
                    && self.attempts == 0
                    && self.status.is_none()
            }
            RemovalMode::Execute => self.outcome != RemovalOutcome::WouldKick,
        };
        if !valid_mode {
            return Err("unsupported audit mode/outcome combination".into());
        }
        Ok(())
    }
}

/// Lifetime inode lock and fsync-per-record. Never truncate prior evidence.
pub struct FileAudit {
    file: File,
    pub done: HashSet<String>,
}
impl FileAudit {
    pub fn open(path: &Path, guild: &str) -> Result<Self, String> {
        let mut opts = OpenOptions::new();
        opts.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path).map_err(|_| "cannot open audit file")?;
        file.try_lock()
            .map_err(|_| "audit lock unavailable; inspect concurrent run")?;
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|_| "cannot read audit file")?;
        if !text.is_empty() && !text.ends_with('\n') {
            return Err("unterminated audit record; repair evidence before resuming".into());
        }
        let mut done = HashSet::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // Fail closed on damaged evidence; never hide a successful kick.
            let r: RemovalRecord = serde_json::from_str(line)
                .map_err(|_| "invalid audit record; repair evidence before resuming")?;
            r.validate()?;
            if r.guild_id == guild
                && r.mode == RemovalMode::Execute
                && matches!(
                    r.outcome,
                    RemovalOutcome::Kicked | RemovalOutcome::AlreadyGone
                )
            {
                done.insert(r.member_id);
            }
        }
        Ok(Self { file, done })
    }
    pub fn append(&mut self, record: &RemovalRecord) -> Result<(), String> {
        record.validate()?;
        let mut bytes = serde_json::to_vec(record).map_err(|_| "cannot encode audit record")?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_all())
            .map_err(|_| "audit write/fsync failed; stopped before next target".into())
    }
}

#[derive(Debug, Default, Serialize)]
pub struct RemovalSummary {
    pub reached: usize,
    pub skipped_done: usize,
    pub failed: usize,
    pub aborted: bool,
}

fn required<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {key} in Discord safety response"))
}

/// Fresh owner, roles, bot membership and target facts. Dangerous-permission
/// staff roles are protected even if the configured protected list is empty.
async fn target_state(
    ex: &ActionExecutor,
    guild: &str,
    id: &str,
    protected: &HashSet<String>,
) -> Result<TargetState, String> {
    let get = |path: String| async move {
        ex.get_json_strict(&path)
            .await
            .map_err(|_| "Discord safety read failed; stop and check authorized access".to_owned())
    };
    let Some(member) = get(format!("/guilds/{guild}/members/{id}")).await? else {
        return Ok(TargetState::Missing);
    };
    let user = member.get("user").ok_or("missing target user")?;
    if required(user, "id")? != id {
        return Err("target identity mismatch".into());
    }
    let is_bot = user.get("bot").and_then(Value::as_bool).unwrap_or(false);
    let guild_info = get(format!("/guilds/{guild}"))
        .await?
        .ok_or("guild missing")?;
    let owner = required(&guild_info, "owner_id")?;
    let bot = get("/users/@me".into()).await?.ok_or("bot missing")?;
    let bot_id = required(&bot, "id")?;
    let roles = get(format!("/guilds/{guild}/roles"))
        .await?
        .ok_or("roles missing")?;
    let roles = roles.as_array().ok_or("invalid roles response")?;
    let bot_member = get(format!("/guilds/{guild}/members/{bot_id}"))
        .await?
        .ok_or("bot membership missing")?;
    let role_ids = |m: &Value| -> Result<Vec<String>, String> {
        m.get("roles")
            .and_then(Value::as_array)
            .ok_or("missing member roles")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or("invalid member role ID".into())
            })
            .collect()
    };
    let target_roles = role_ids(&member)?;
    let bot_roles = role_ids(&bot_member)?;
    let mut protected_roles = protected.clone();
    let mut known = HashSet::new();
    let (mut target_top, mut bot_top, mut bot_perms) = (0, 0, 0_u64);
    // Shared staff permissions, plus Manage Messages and Manage Guild Expressions.
    let dangerous = DANGEROUS_PERMISSIONS | (1 << 13) | (1 << 29);
    for role in roles {
        let rid = required(role, "id")?;
        let pos = role
            .get("position")
            .and_then(Value::as_i64)
            .ok_or("missing role position")?;
        let perms: u64 = required(role, "permissions")?
            .parse()
            .map_err(|_| "invalid role permissions")?;
        known.insert(rid.to_owned());
        if perms & dangerous != 0 {
            protected_roles.insert(rid.to_owned());
        }
        if rid == guild || target_roles.iter().any(|r| r == rid) {
            target_top = target_top.max(pos);
        }
        if rid == guild || bot_roles.iter().any(|r| r == rid) {
            bot_top = bot_top.max(pos);
            bot_perms |= perms;
        }
    }
    if !known.contains(guild)
        || target_roles
            .iter()
            .chain(&bot_roles)
            .any(|r| !known.contains(r))
    {
        return Err("unknown role in fresh safety snapshot".into());
    }
    if bot_perms & (8 | 2) == 0 {
        return Err("bot lacks Kick Members permission".into());
    }
    let mut target_roles = target_roles;
    target_roles.push(guild.to_owned());
    let target = ModerationTarget {
        user_id: id.into(),
        role_ids: target_roles,
        highest_role_position: target_top,
        is_bot,
        is_guild_owner: id == owner,
    };
    let policy = ModerationPolicy {
        owen_user_id: bot_id.into(),
        protected_role_ids: protected_roles,
        bot_user_id: Some(bot_id.into()),
    };
    if moderation_target_protection(&target, &policy).is_some() || target_top >= bot_top {
        Ok(TargetState::Protected)
    } else {
        Ok(TargetState::Eligible)
    }
}

pub struct RemovalRun<'a> {
    pub guild: &'a str,
    pub ids: &'a [String],
    pub mode: RemovalMode,
    pub reason: &'a str,
    pub run_id: &'a str,
    pub done: &'a HashSet<String>,
    pub protected: &'a HashSet<String>,
}

pub async fn remove_accounts(
    run: RemovalRun<'_>,
    executor: Option<&ActionExecutor>,
    mut sink: impl FnMut(&RemovalRecord) -> Result<(), String>,
) -> Result<RemovalSummary, String> {
    if run.mode == RemovalMode::Execute && executor.is_none() {
        return Err("execute requires an executor".into());
    }
    let mut summary = RemovalSummary::default();
    let mut consecutive = 0;
    for id in run.ids {
        if run.done.contains(id) {
            summary.skipped_done += 1;
            continue;
        }
        let mut stop = false;
        let (outcome, status, attempts) = if run.mode == RemovalMode::DryRun {
            (RemovalOutcome::WouldKick, None, 0)
        } else {
            let ex = executor.unwrap();
            let result = ex
                .kick_paced_guarded(run.guild, id, run.reason, |attempts| async move {
                    match target_state(ex, run.guild, id, run.protected).await {
                        Ok(TargetState::Eligible) => Ok(()),
                        Ok(state) => Err((Some(state), attempts)),
                        Err(_) => Err((None, attempts)),
                    }
                })
                .await;
            match result {
                Ok(result) => {
                    stop = matches!(result.status, Some(401 | 403));
                    let outcome = match result.outcome {
                        KickOutcome::Kicked => RemovalOutcome::Kicked,
                        KickOutcome::AlreadyGone => RemovalOutcome::AlreadyGone,
                        KickOutcome::Forbidden => RemovalOutcome::Forbidden,
                        KickOutcome::RateLimited => RemovalOutcome::RateLimited,
                        KickOutcome::Failed => RemovalOutcome::Failed,
                    };
                    (outcome, result.status, result.attempts)
                }
                Err((Some(TargetState::Missing), attempts)) => {
                    (RemovalOutcome::AlreadyGone, Some(404), attempts)
                }
                Err((Some(_), attempts)) => (RemovalOutcome::Protected, None, attempts),
                Err((None, attempts)) => {
                    stop = true;
                    (RemovalOutcome::Failed, None, attempts)
                }
            }
        };
        let failed = matches!(
            outcome,
            RemovalOutcome::Failed | RemovalOutcome::Forbidden | RemovalOutcome::RateLimited
        );
        let record = RemovalRecord {
            v: 1,
            ts: crate::cli::now_iso(),
            run_id: run.run_id.into(),
            guild_id: run.guild.into(),
            member_id: id.clone(),
            mode: run.mode,
            action: "kick".into(),
            outcome,
            status,
            attempts,
        };
        sink(&record)?;
        summary.reached += 1;
        if failed {
            summary.failed += 1;
            consecutive += 1;
        } else {
            consecutive = 0;
        }
        if stop || consecutive >= 3 {
            summary.aborted = true;
            break;
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_and_json_are_removable_without_expanding_the_list() {
        let rows = vec![CohortRow {
            guild_id: "100000000000000001".into(),
            member_id: "100000000000000002".into(),
            joined_at: "2026-09-01T00:00:00Z".into(),
            score: 3,
            source: "unknown".into(),
            reasons_json: "[]".into(),
        }];
        for text in [
            cohort_csv(&rows).unwrap(),
            serde_json::to_string(&rows).unwrap(),
        ] {
            assert_eq!(
                parse_targets(&text, &rows[0].guild_id).unwrap(),
                vec![rows[0].member_id.clone()]
            );
            assert!(parse_targets(&text, "100000000000000003").is_err());
        }
        assert!(parse_targets("[123456789012345678]", &rows[0].guild_id).is_err());
    }
    #[tokio::test]
    async fn dry_run_has_no_executor_and_audits_before_advancing() {
        let ids = vec!["100000000000000001".into(), "100000000000000002".into()];
        let mut records = vec![];
        let empty = HashSet::new();
        let run = RemovalRun {
            guild: "100000000000000003",
            ids: &ids,
            mode: RemovalMode::default(),
            reason: "reviewed raid",
            run_id: "test",
            done: &empty,
            protected: &empty,
        };
        let summary = remove_accounts(run, None, |r| {
            records.push(r.clone());
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(summary.reached, 2);
        assert!(records
            .iter()
            .all(|r| r.outcome == RemovalOutcome::WouldKick && r.attempts == 0));
        let run = RemovalRun {
            guild: "100000000000000003",
            ids: &ids,
            mode: RemovalMode::default(),
            reason: "reviewed raid",
            run_id: "test",
            done: &empty,
            protected: &empty,
        };
        let mut calls = 0;
        assert!(remove_accounts(run, None, |_| {
            calls += 1;
            Err("disk full".into())
        })
        .await
        .is_err());
        assert_eq!(calls, 1);
    }
}
