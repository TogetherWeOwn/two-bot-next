//! Host-less join capture CLI (legacy `scripts/capture.ts`).
//!
//! Attribution needs somebody reading the invite counters more often than
//! people join. This samples the live bot's invite logic coarsely: diff the
//! counters, list the members, attribute the window's joins, store the new
//! counters LAST so a crash mid-write re-reads the same window next run.
//! Read-only against Discord; every write is idempotent on
//! (member, joined_at), first attribution wins.
//!
//! `capture --dry-run` reports and writes nothing.

use std::collections::BTreeMap;
use twilight_model::id::Id;
use two_bot_cutover::cli::{now_iso, open_db, Args};
use two_bot_cutover::{
    attribute_joins, invite_growth, mark_bot, record_event, FunnelWrite, InviteState, RestClient,
};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let dry_run = args.has("dry-run");

    let token = std::env::var("DISCORD_TOKEN")
        .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
        .unwrap_or_default();
    let guild_id = std::env::var("DISCORD_GUILD_ID").unwrap_or_default();
    if token.is_empty() || guild_id.is_empty() {
        eprintln!("Missing DISCORD_BOT_TOKEN or DISCORD_GUILD_ID.");
        std::process::exit(2);
    }
    if guild_id == two_bot_cutover::LIVE_GUILD_ID && !args.has("allow-live-guild") {
        eprintln!(
            "Refusing live guild {}. Use --allow-live-guild only for an owner-approved rollout.",
            two_bot_cutover::LIVE_GUILD_ID
        );
        std::process::exit(2);
    }
    if std::env::var("TWO_DATABASE_URL")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        eprintln!("Missing TWO_DATABASE_URL.");
        std::process::exit(2);
    }

    // Stamp the window BEFORE reading anything: a join landing between this
    // instant and the fetch falls in the next window, and double-counting is
    // not a risk (member_join is keyed on guild/member/joined_at).
    let captured_at = now_iso();

    let rest = RestClient::from_env(token, args.values.get("discord-base").cloned())
        .await
        .unwrap_or_else(|error| {
            eprintln!("send admission bootstrap failed: {error}");
            std::process::exit(1);
        });
    let db = open_db(&args, false).await;
    let guild: Id<twilight_model::id::marker::GuildMarker> = Id::new(guild_id.parse().unwrap_or(0));

    println!(
        "\nTWO capture{}",
        if dry_run {
            "  (DRY RUN - nothing will be written)"
        } else {
            ""
        }
    );
    println!("  guild {guild_id}\n");

    // --- 1. previous window --------------------------------------------------
    // `uses` is INTEGER (INT4) per the legacy DDL; decode as i32.
    // A failed snapshot read is fatal, not an empty baseline: silently
    // re-baselining would misattribute the whole next window.
    let prev_rows: Vec<(String, i32, String)> =
        sqlx::query_as("SELECT code, uses, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM invite_snapshots WHERE guild_id = $1")
            .bind(&guild_id)
            .fetch_all(db.pool())
            .await
            .unwrap_or_else(|e| {
                eprintln!("invite snapshot read failed: {e}");
                std::process::exit(1);
            });
    let mut since: Option<String> = None;
    let mut prev_uses: BTreeMap<String, u64> = BTreeMap::new();
    for (code, uses, updated_at) in &prev_rows {
        if since.as_ref().is_none_or(|s| updated_at > s) {
            since = Some(updated_at.clone());
        }
        prev_uses.insert(code.clone(), *uses as u64);
    }

    // --- 2. invite counters ----------------------------------------------------
    let raw_invites = match rest.guild_invites(guild).await {
        Ok(Some(inv)) => inv,
        _ => {
            eprintln!(
                "Could not read the invite list. That is the Manage Server permission -\n\
                 without it every join records as `unknown`."
            );
            std::process::exit(1);
        }
    };
    let current: Vec<InviteState> = raw_invites
        .iter()
        .map(|i| InviteState {
            code: i.code.clone(),
            uses: i.uses.unwrap_or(0),
        })
        .collect();
    let growth = invite_growth(&prev_uses, &current);
    let total_growth: u64 = growth.values().sum();

    // --- 3. new members ----------------------------------------------------------
    let members = match rest.fetch_all_members(guild).await {
        Ok(Some(m)) => m,
        _ => {
            eprintln!("Read zero members. That is Server Members Intent being OFF.");
            std::process::exit(1);
        }
    };
    if members.is_empty() {
        eprintln!("Read zero members. That is Server Members Intent being OFF.");
        std::process::exit(1);
    }
    let has_vanity = rest
        .guild(guild)
        .await
        .ok()
        .flatten()
        .and_then(|g| g.vanity_url_code)
        .is_some();

    let mut new_joins: Vec<(String, String)> = Vec::new();
    let mut bots = 0usize;
    for m in &members {
        let id = m.user.id.get().to_string();
        if m.user.bot {
            bots += 1;
            if !dry_run {
                mark_bot(&db, &guild_id, &id).await.unwrap_or_else(|e| {
                    eprintln!("markBot failed: {e}");
                    std::process::exit(1);
                });
            }
            continue;
        }
        let Some(joined) = m.joined_at.as_ref().map(|t| t.iso_8601().to_string()) else {
            continue;
        };
        // First ever capture has no `since`; the member list is history, not
        // this window, and backfill owns history. Baseline only.
        if since.as_ref().is_some_and(|s| joined > *s) {
            new_joins.push((id, joined));
        }
    }
    new_joins.sort_by(|a, b| a.1.cmp(&b.1));

    // --- 4. attribute + write ------------------------------------------------------
    let attributions = attribute_joins(&growth, new_joins.len(), has_vanity);
    let mut written = 0usize;
    if !dry_run {
        for ((id, joined_at), attr) in new_joins.iter().zip(attributions.iter()) {
            let metadata = serde_json::json!({
                "capture": true,
                "attribution_exact": attr.exact,
                "window": {"from": since, "to": captured_at},
            });
            let (is_new, _) = record_event(
                &db,
                &FunnelWrite {
                    member_id: Some(id.clone()),
                    guild_id: guild_id.clone(),
                    event_type: "member_join".to_owned(),
                    occurred_at: joined_at.clone(),
                    source: attr.source.clone(),
                    metadata: Some(metadata.to_string()),
                },
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("write failed: {e}");
                std::process::exit(1);
            });
            if is_new {
                written += 1;
            }
        }
        // New counters LAST: a crash mid-write re-reads the same window.
        store_counters(&db, &guild_id, &raw_invites, &captured_at).await;
    }

    // --- 5. report -------------------------------------------------------------------
    let label = match since.as_deref() {
        None => "first capture - baseline only".to_owned(),
        Some(s) => format!("window {s} -> {captured_at}"),
    };
    println!("  {label}");
    let mut grew: Vec<String> = growth.keys().cloned().collect();
    grew.sort();
    println!(
        "  invites              {} readable, {} moved{}",
        current.len(),
        grew.len(),
        if grew.is_empty() {
            String::new()
        } else {
            format!(
                " ({})",
                grew.iter()
                    .map(|c| format!("{c} +{}", growth.get(c).unwrap_or(&0)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    println!(
        "  members              {} total ({bots} bots)",
        members.len()
    );
    println!("  new joins in window  {}", new_joins.len());
    if !new_joins.is_empty() {
        let mut by_source: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for a in &attributions {
            let e = by_source.entry(a.source.as_str()).or_insert((0, 0));
            e.0 += 1;
            if a.exact {
                e.1 += 1;
            }
        }
        let split: Vec<String> = by_source
            .iter()
            .map(|(s, (n, exact))| {
                format!(
                    "{s} x{n}{}",
                    if exact == n {
                        ""
                    } else {
                        " (~ who-got-which not observed)"
                    }
                )
            })
            .collect();
        println!(
            "  attributed as        {}",
            split.join("\n                       ")
        );
        println!(
            "  written              {} new events",
            if dry_run {
                "0 (dry run)".to_owned()
            } else {
                written.to_string()
            }
        );
    }
    if since.is_some() && total_growth != new_joins.len() as u64 {
        println!(
            "\n  note: invite counters moved {total_growth}, member list gained {}.\n        Likely a join+leave inside the window, or a join through the vanity URL.\n        Neither is recoverable later - shorter windows are the only lever.",
            new_joins.len()
        );
    }
    println!(
        "\n  {} Discord requests. Nothing was posted, no roles changed.\n",
        rest.requests()
    );
    db.close().await;
}

async fn store_counters(
    db: &two_bot_cutover::CutoverDb,
    guild_id: &str,
    invites: &[twilight_model::guild::invite::Invite],
    captured_at: &str,
) {
    let rows: Vec<two_bot_cutover::invite_store::CounterRow> = invites
        .iter()
        .map(|inv| two_bot_cutover::invite_store::CounterRow {
            code: inv.code.clone(),
            uses: inv.uses.unwrap_or(0) as i64,
            inviter_id: inv.inviter.as_ref().map(|u| u.id.get().to_string()),
            channel_id: inv.channel.as_ref().map(|c| c.id.get().to_string()),
        })
        .collect();
    two_bot_cutover::invite_store::store_counters(db.pool(), guild_id, &rows, captured_at)
        .await
        .unwrap_or_else(|e| {
            eprintln!("invite snapshot write failed: {e}");
            std::process::exit(1);
        });
}
