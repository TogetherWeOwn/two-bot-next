//! Source: TogetherWeOwn/two-bot @ 96777468472f23a02a1e97a43ffab3912fe5df2a,
//! migrations/0001–0042 (54 SQL files, including repeated numeric prefixes).
//! Target: the embedded cutover migrations. Same-name != compatible schema.

use super::{Column, CopyMode, Sequence, Table};
use serde::Serialize;

macro_rules! table {
    ($group:literal, $name:literal, [$($key:literal),+], {$($column:ident: $ty:literal),+ $(,)?}, $mode:ident, $sequence:expr, $refusal:expr) => {
        Table {
            group: $group, source: $name, target: $name,
            keys: &[$($key),+], conflict: &[$($key),+],
            columns: &[$(Column { source: stringify!($column), target: stringify!($column), pg_type: $ty }),+],
            mode: CopyMode::$mode, sequence: $sequence, source_refusal: $refusal,
        }
    };
}

const fn sequence(name: &'static str, column: &'static str) -> Option<Sequence> {
    Some(Sequence { name, column })
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub name: &'static str,
    pub status: &'static str,
    pub reason: &'static str,
    pub tables: &'static [Table],
}

pub const GROUPS: &[Group] = &[
    Group { name: "funnel", status: "ready", reason: "legacy 0001/0007–0009/0038 → next 0001/0190", tables: &[
        table!("funnel", "events", ["id"], {
            id: "bigint", event_type: "text", member_id: "text", guild_id: "text",
            occurred_at: "timestamptz", recorded_at: "timestamptz", source: "text",
            metadata: "text", idempotency_key: "text"
        }, Upsert, sequence("events_id_seq", "id"), None),
        table!("funnel", "members", ["guild_id", "member_id"], {
            guild_id: "text", member_id: "text", joined_at: "timestamptz", join_source: "text",
            first_message_at: "timestamptz", first_voice_at: "timestamptz", last_active_at: "timestamptz",
            left_at: "timestamptz", inactive_flagged_at: "timestamptz", is_bot: "boolean",
            gate_cleared_at: "timestamptz", third_message_at: "timestamptz"
        }, Upsert, None, None),
        table!("funnel", "invite_snapshots", ["guild_id", "code"], {
            guild_id: "text", code: "text", uses: "integer", inviter_id: "text", channel_id: "text", updated_at: "timestamptz"
        }, Upsert, None, None),
    ]},
    Group { name: "leveling", status: "ready", reason: "legacy 0010/0011 leveling → next 0002", tables: &[
        table!("leveling", "member_levels", ["guild_id", "member_id"], {
            guild_id: "text", member_id: "text", xp: "bigint", message_xp: "bigint", voice_xp: "bigint",
            imported_xp: "bigint", updated_at: "timestamptz"
        }, Upsert, None, None),
        table!("leveling", "xp_cooldowns", ["guild_id", "member_id", "source"], {
            guild_id: "text", member_id: "text", source: "text", last_awarded_at: "timestamptz"
        }, Upsert, None, None),
        table!("leveling", "xp_awards", ["id"], {
            id: "bigint", guild_id: "text", member_id: "text", source: "text", xp: "integer",
            occurred_at: "timestamptz", channel_id: "text"
        }, Upsert, sequence("xp_awards_id_seq", "id"), None),
        table!("leveling", "level_role_rewards", ["guild_id", "level"], {
            guild_id: "text", level: "integer", role_id: "text"
        }, Upsert, None, None),
        table!("leveling", "level_import_runs", ["id"], {
            id: "bigint", guild_id: "text", source: "text", source_rows: "integer", unique_members: "integer",
            inserted: "integer", updated: "integer", unchanged: "integer", duplicate_rows: "integer",
            total_imported_xp: "bigint", imported_at: "timestamptz"
        }, Upsert, sequence("level_import_runs_id_seq", "id"), None),
    ]},
    Group { name: "website_contract", status: "ready", reason: "legacy 0003/0005 → next 0300; parent ladder before children", tables: &[
        table!("website_contract", "web_contract_meta", ["singleton"], {
            singleton: "boolean", contract_version: "text", guild_id: "text"
        }, Upsert, None, None),
        table!("website_contract", "guild_counters", ["guild_id"], {
            guild_id: "text", human_member_count: "integer", human_member_count_at: "text",
            online_count: "integer", online_count_at: "text"
        }, Upsert, None, None),
        table!("website_contract", "rank_ladder", ["rank_key"], {
            rank_key: "text", rank_label: "text", rank_order: "integer", role_id: "text"
        }, Upsert, None, None),
        table!("website_contract", "rank_snapshots", ["guild_id", "rank_key"], {
            guild_id: "text", rank_key: "text", member_count: "integer", holders_count: "integer", snapshot_at: "text"
        }, Upsert, None, None),
        table!("website_contract", "member_ranks", ["guild_id", "member_id"], {
            guild_id: "text", member_id: "text", rank_key: "text", updated_at: "text"
        }, Upsert, None, None),
        table!("website_contract", "scheduled_events", ["guild_id", "event_id"], {
            guild_id: "text", event_id: "text", name: "text", starts_at: "text", channel_id: "text",
            description: "text", status: "text", updated_at: "text"
        }, Upsert, None, None),
        table!("website_contract", "counter_snapshots", ["guild_id"], {
            guild_id: "text", human_member_count: "integer", human_member_count_at: "text",
            online_count: "integer", online_count_at: "text"
        }, Upsert, None, None),
        table!("website_contract", "member_exclusions", ["guild_id", "member_id"], {
            guild_id: "text", member_id: "text", reason: "text", updated_at: "text"
        }, Upsert, None, None),
    ]},
    Group { name: "presence", status: "ready", reason: "legacy 0004/0041 → next 0310", tables: &[
        table!("presence", "presence_probe", ["guild_id", "observed_at"], {
            guild_id: "text", observed_at: "text", approximate_presence_count: "integer",
            bot_floor: "integer", bot_floor_scan_truncated: "boolean"
        }, Upsert, None, None),
    ]},
    Group { name: "community", status: "ready", reason: "legacy 0018 scorecard → next 0160/0311", tables: &[
        table!("community", "community_facts", ["id"], {
            id: "bigint", guild_id: "text", event_type: "text", source_event_id: "text", actor_id: "text",
            occurred_at: "text", recorded_at: "text", source: "text", classifier_version: "text",
            classification: "text", matched_rule: "text", metadata: "text", idempotency_key: "text"
        }, Upsert, sequence("community_facts_id_seq", "id"), None),
        table!("community", "community_stream_heartbeats", ["guild_id", "stream"], {
            guild_id: "text", stream: "text", covered_from: "text", covered_through: "text", updated_at: "text"
        }, Upsert, None, None),
        table!("community", "community_scorecard_runs", ["id"], {
            id: "bigint", guild_id: "text", week_start: "text", week_end: "text", classifier_version: "text",
            watermark: "bigint", input_count: "integer", input_hash: "text", idempotency_key: "text",
            revision: "integer", run_status: "text", coverage_state: "text", evidence_state: "text",
            scorecard_json: "text", intervention_code: "text", generated_at: "text"
        }, Upsert, sequence("community_scorecard_runs_id_seq", "id"), None),
        table!("community", "community_scorecard_alerts", ["guild_id", "alert_key"], {
            guild_id: "text", week_start: "text", alert_key: "text", created_at: "text"
        }, Upsert, None, None),
    ]},
    Group { name: "guild_settings", status: "ready", reason: "legacy 0026–0042 settings constraints → next 0330; retain target revision trigger", tables: &[
        table!("guild_settings", "guild_settings", ["guild_id", "key"], {
            guild_id: "text", key: "text", value: "jsonb", version: "bigint", updated_at: "timestamptz", updated_by: "text"
        }, Upsert, sequence("guild_settings_version_seq", "version"), None),
        table!("guild_settings", "guild_settings_audit", ["id"], {
            id: "bigint", guild_id: "text", key: "text", old_value: "jsonb", new_value: "jsonb", actor: "text", at: "timestamptz"
        }, AppendOnly, sequence("guild_settings_audit_id_seq", "id"), None),
    ]},
    Group { name: "operational_audit", status: "ready", reason: "legacy 0011–0017/0027 audit → next 0340; refuse nonterminal deliveries", tables: &[
        table!("operational_audit", "operational_audit_log", ["entry_id"], {
            entry_id: "text", event_kind: "text", guild_id: "text", occurred_at: "timestamptz", actor_id: "text",
            target_id: "text", source_channel_id: "text", destination_channel_id: "text", message_id: "text",
            action: "text", metadata_json: "text", created_at: "timestamptz", mirror_channel_id: "text",
            delivery_state: "text", delivery_attempts: "integer", delivery_attempted_at: "timestamptz",
            delivery_last_error: "text", delivery_lease_until: "timestamptz", mirrored_at: "timestamptz",
            delivery_nonce: "text", mirror_message_id: "text", delivery_search_before: "text",
            delivery_claim_token: "text", mirror_checked_at: "timestamptz"
        }, Upsert, None, Some("delivery_state NOT IN ('none', 'delivered', 'quarantined')")),
        table!("operational_audit", "audit_kill_switch", ["id"], {
            id: "integer", engaged_at: "timestamptz", engaged_by: "text"
        }, Upsert, None, None),
    ]},
    Group { name: "internal_actions", status: "pending", reason: "legacy 0002 raw key/nonce, result JSON and request-id log are incompatible with next 0350 hashed guards/scalar intent ledger; dedicated semantic migration required", tables: &[] },
    Group { name: "gateway_sessions", status: "pending", reason: "next 0320 exists, but legacy 0001–0042 has no Postgres gateway_sessions table; do not invent file-session conversion", tables: &[] },
    Group { name: "moderation", status: "pending", reason: "warnings/scheduled unbans lack target DDL; channel recovery uses new generations and requires a dedicated ledger mapping", tables: &[] },
    Group { name: "automations", status: "pending", reason: "commands/scheduled messages lack target DDL; sticky recovery needs source DDL and lease/generation mapping", tables: &[] },
    Group { name: "tickets", status: "pending", reason: "ticket/transcript target migrations are not shipped", tables: &[] },
    Group { name: "automod", status: "pending", reason: "automod/anti-nuke/containment target migrations are not shipped", tables: &[] },
    Group { name: "self_roles", status: "pending", reason: "self-role panel/audit/recovery target migrations are not shipped", tables: &[] },
    Group { name: "feeds", status: "pending", reason: "feed relay/delivery target migrations are not shipped", tables: &[] },
    Group { name: "invite_campaigns", status: "pending", reason: "invite_campaigns target migration is not shipped", tables: &[] },
    Group { name: "lfg_rsvp_temp_voice", status: "pending", reason: "LFG/RSVP need authoritative legacy runtime DDL; temp-voice target migration is not shipped", tables: &[] },
    Group { name: "onboarding_rota", status: "retired", reason: "rota is explicitly retired; no row-copy target", tables: &[] },
];

/// `ready` explicitly selects compatible groups; the plan still includes every
/// pending/retired group. `all` refuses rather than claiming complete parity.
pub fn select(names: &[String]) -> Result<Vec<Table>, String> {
    let mut selected = Vec::new();
    for group in GROUPS {
        if names
            .iter()
            .any(|n| n == group.name || n == "all" || (n == "ready" && group.status == "ready"))
        {
            if group.status != "ready" {
                return Err(format!(
                    "{} group {}: {}",
                    group.status, group.name, group.reason
                ));
            }
            selected.extend_from_slice(group.tables);
        }
    }
    for name in names {
        if name != "all" && name != "ready" && !GROUPS.iter().any(|g| g.name == name) {
            return Err("unknown table group (see --plan)".to_owned());
        }
    }
    if selected.is_empty() {
        return Err("select at least one table group".to_owned());
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_and_unknown_groups_refuse_instead_of_skipping() {
        for group in GROUPS.iter().filter(|g| g.status != "ready") {
            assert!(select(&[group.name.to_owned()])
                .unwrap_err()
                .contains(group.name));
        }
        assert!(select(&["all".to_owned()]).is_err());
        assert!(select(&["funnel".to_owned(), "unknown".to_owned()]).is_err());
        assert_eq!(select(&["ready".to_owned()]).unwrap().len(), 25);
    }

    #[test]
    fn registry_has_unique_tables_keys_and_explicit_column_mapping() {
        let mut names = std::collections::HashSet::new();
        for table in select(&["ready".to_owned()]).unwrap() {
            assert!(names.insert(table.target));
            assert!(!table.keys.is_empty());
            for key in table.keys {
                assert!(table.columns.iter().any(|c| c.source == *key));
            }
            for key in table.conflict {
                assert!(table.columns.iter().any(|c| c.target == *key));
            }
        }
    }
}
