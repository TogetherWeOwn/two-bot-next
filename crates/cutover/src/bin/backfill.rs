//! History backfill CLI (legacy `scripts/backfill.ts`).
//!
//! Recovers joins, leaves, gate clearings and voice sessions from Discord's
//! member list (`joined_at` + `pending`) and the server's own log channels,
//! reconciles the sources (log wins within 5 minutes), and writes
//! idempotently. Read-only against Discord; safe to re-run.
//!
//! `backfill --dry-run` reports and writes nothing. Env: `DISCORD_TOKEN` (or
//! `DISCORD_BOT_TOKEN`), `DISCORD_GUILD_ID`, `TWO_DATABASE_URL`.

use std::collections::HashSet;
use twilight_model::channel::ChannelType;
use twilight_model::id::Id;
use two_bot_cutover::cli::{open_db, Args, ScanReport};
use two_bot_cutover::{
    mark_bot, member_log_kind_for_channel, parse_member_log_message, parse_voice_message,
    plan_backfill_merge, record_earliest, record_event, touch_activity, DedupableEvent, EmbedView,
    FunnelWrite, ListedMember, MessageView, RestClient,
};

fn usage() -> ! {
    eprintln!(
        "Usage: backfill [--dry-run] [--max-pages=N] [--discord-base <url>] [--allow-live-guild]\n\
         Env: DISCORD_TOKEN (or DISCORD_BOT_TOKEN), DISCORD_GUILD_ID, TWO_DATABASE_URL."
    );
    std::process::exit(2);
}

/// Live-guild fence for the env-guild CLIs: unlike the `--guild` CLIs these
/// read the guild from `DISCORD_GUILD_ID`, so the fence runs against the env
/// value before any database or file opens.
fn fence_live_guild(guild_id: &str, args: &Args) {
    if guild_id == two_bot_cutover::LIVE_GUILD_ID && !args.has("allow-live-guild") {
        eprintln!(
            "Refusing live guild {}. Use --allow-live-guild only for an owner-approved rollout.",
            two_bot_cutover::LIVE_GUILD_ID
        );
        std::process::exit(2);
    }
}

fn token() -> String {
    std::env::var("DISCORD_TOKEN")
        .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
        .unwrap_or_default()
}

fn to_embed_view(e: &twilight_model::channel::message::Embed) -> EmbedView {
    EmbedView {
        title: e.title.clone(),
        description: e.description.clone(),
        footer_text: e.footer.as_ref().map(|f| f.text.clone()),
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let dry_run = args.has("dry-run");
    let max_pages: usize = args
        .get("max-pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);

    let token = token();
    if token.is_empty() {
        eprintln!("Missing DISCORD_TOKEN (or DISCORD_BOT_TOKEN).");
        usage();
    }
    let guild_id = std::env::var("DISCORD_GUILD_ID").unwrap_or_default();
    if guild_id.is_empty() || !two_bot_cutover::is_snowflake(&guild_id) {
        eprintln!("Missing DISCORD_GUILD_ID. Backfill targets exactly one server on purpose.");
        usage();
    }
    fence_live_guild(&guild_id, &args);
    if std::env::var("TWO_DATABASE_URL")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        eprintln!("Missing TWO_DATABASE_URL.");
        usage();
    }

    let t0 = std::time::Instant::now();
    let rest = RestClient::from_env(token, args.values.get("discord-base").cloned())
        .await
        .unwrap_or_else(|error| {
            eprintln!("send admission bootstrap failed: {error}");
            std::process::exit(1);
        });
    let db = open_db(&args, false).await;
    let guild: Id<twilight_model::id::marker::GuildMarker> = Id::new(guild_id.parse().unwrap_or(0));

    println!(
        "\nTWO backfill{}",
        if dry_run {
            "  (DRY RUN - nothing will be written)"
        } else {
            ""
        }
    );
    println!("  guild {guild_id}   max {max_pages} pages/channel\n");

    // --- 1. current members ------------------------------------------------
    let members = match rest.fetch_all_members(guild).await {
        Ok(Some(m)) => m,
        Ok(None) | Err(_) => {
            eprintln!(
                "Read zero members. That usually means Server Members Intent is OFF -\n\
                 the REST member list needs it too, not just the gateway."
            );
            std::process::exit(1);
        }
    };
    if members.is_empty() {
        eprintln!("Read zero members. That usually means Server Members Intent is OFF.");
        std::process::exit(1);
    }

    let mut bot_ids: HashSet<String> = HashSet::new();
    let mut listed: Vec<ListedMember> = Vec::with_capacity(members.len());
    for m in &members {
        let id = m.user.id.get().to_string();
        if m.user.bot {
            bot_ids.insert(id.clone());
            if !dry_run {
                if let Err(e) = mark_bot(&db, &guild_id, &id).await {
                    eprintln!("markBot failed: {e}");
                    std::process::exit(1);
                }
            }
            continue;
        }
        listed.push(ListedMember {
            id,
            joined_at: m.joined_at.as_ref().map(|t| t.iso_8601().to_string()),
            // twilight's `pending` is `bool` with serde default: absent
            // (screening off) reads as false, which takes the same
            // non-stuck branch legacy's `pending === undefined` takes.
            pending: Some(m.pending),
            is_bot: false,
        });
    }
    let humans = listed.len();

    // --- 2. log channels ----------------------------------------------------
    let channels = rest
        .guild_channels(guild)
        .await
        .unwrap_or(None)
        .unwrap_or_default();
    let candidates: Vec<_> = channels
        .iter()
        .filter(|c| {
            matches!(
                c.kind,
                ChannelType::GuildText | ChannelType::GuildAnnouncement
            ) && c.name.as_deref().is_some_and(|n| {
                let l = n.to_lowercase();
                [
                    "log", "invite", "wick", "join", "leave", "member", "voice", "logger",
                ]
                .iter()
                .any(|k| l.contains(k))
            })
        })
        .collect();

    let mut log_joins: Vec<DedupableEvent> = Vec::new();
    let mut log_leaves: Vec<DedupableEvent> = Vec::new();
    let mut voice_events: Vec<FunnelWrite> = Vec::new();
    let mut truncated: Vec<String> = Vec::new();
    let mut scan_report = ScanReport::default();
    let mut scanned_messages = 0usize;
    let mut oldest_seen: Option<String> = None;
    let mut skipped = 0usize;

    for ch in candidates.iter() {
        let name = ch.name.clone().unwrap_or_else(|| ch.id.get().to_string());
        let channel_kind = member_log_kind_for_channel(&name);
        // Probe one page: the bar is a HIT RATE (5%), not a single hit.
        let probe = match rest.scan_channel(ch.id, 1, None).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("probe of #{name} failed: {e}");
                scan_report.record(&name, two_bot_cutover::ScanCompletion::RequestFailed);
                continue;
            }
        };
        if probe.completion.interrupted() {
            scan_report.record(&name, probe.completion);
            continue;
        }
        let probe_views: Vec<MessageView> = probe.messages.iter().map(to_message_view).collect();
        let hits = probe_views
            .iter()
            .filter(|m| {
                parse_member_log_message(m, channel_kind).is_some()
                    || parse_voice_message(m).is_some()
            })
            .count();
        let yield_rate = if probe.messages.is_empty() {
            0.0
        } else {
            hits as f64 / probe.messages.len() as f64
        };
        if hits == 0 || yield_rate < 0.05 {
            if hits > 0 {
                println!(
                    "  {name:20} skipped - only {:.0}% of its newest page is funnel data (mixed feed, covered by the dedicated channels)",
                    yield_rate * 100.0
                );
            }
            skipped += 1;
            continue;
        }

        let page = match rest.scan_channel(ch.id, max_pages, None).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("scan of #{name} failed: {e}");
                scan_report.record(&name, two_bot_cutover::ScanCompletion::RequestFailed);
                continue;
            }
        };
        scan_report.record(&name, page.completion);
        if page.truncated {
            truncated.push(name.clone());
        }
        if page.messages.is_empty() {
            continue;
        }
        scanned_messages += page.messages.len();
        if let Some(back) = page.scanned_back_to {
            if oldest_seen.as_ref().is_none_or(|o| back < *o) {
                oldest_seen = Some(back);
            }
        }
        let mut hits = 0usize;
        for raw in &page.messages {
            let view = to_message_view(raw);
            if let Some(mem) = parse_member_log_message(&view, channel_kind) {
                if bot_ids.contains(&mem.member_id) {
                    continue;
                }
                hits += 1;
                let (event_type, kind) = match mem.kind {
                    two_bot_cutover::MemberLogKind::Join => ("member_join", "join"),
                    two_bot_cutover::MemberLogKind::Leave => ("member_leave", "leave"),
                };
                let _ = kind;
                (if event_type == "member_join" {
                    &mut log_joins
                } else {
                    &mut log_leaves
                })
                .push(DedupableEvent {
                    event_type: event_type.to_owned(),
                    member_id: Some(mem.member_id),
                    occurred_at: mem.occurred_at,
                    source: format!("backfill:log:{name}"),
                });
                continue;
            }
            if let Some(voice) = parse_voice_message(&view) {
                // 'leave' tells us a session ended; the START is what the
                // funnel measures. Counting leaves would re-stamp the session.
                if voice.kind == two_bot_cutover::VoiceKind::Leave
                    || bot_ids.contains(&voice.member_id)
                {
                    continue;
                }
                hits += 1;
                voice_events.push(FunnelWrite {
                    member_id: Some(voice.member_id),
                    guild_id: guild_id.clone(),
                    event_type: "first_voice_session".to_owned(),
                    occurred_at: voice.occurred_at,
                    source: voice
                        .channel_id
                        .map(|c| format!("channel:{c}"))
                        .unwrap_or_else(|| format!("backfill:log:{name}")),
                    metadata: Some(r#"{"backfill":true}"#.to_owned()),
                });
            }
        }
        if hits > 0 {
            println!(
                "  {name:20} {:>5} messages  {:>5} usable{}",
                page.messages.len(),
                hits,
                if page.truncated { "  (TRUNCATED)" } else { "" }
            );
        }
    }
    let total_targets = candidates.len();
    println!(
        "  scanned              {:>5} messages across {} of {total_targets} candidate channels ({} held no join/leave/voice entries)\n",
        scanned_messages,
        total_targets.saturating_sub(skipped),
        skipped
    );

    // --- 3. reconcile --------------------------------------------------------
    let merge = plan_backfill_merge(&log_joins, &log_leaves, &listed);
    println!(
        "  member list          {:>5} members  ({humans} human, {} bots)",
        members.len(),
        bot_ids.len()
    );
    println!(
        "  rules gate           {:>5} through  ({} still behind it and unable to interact)",
        merge.gate_clearings, merge.gate_stuck
    );

    // --- 4. write ------------------------------------------------------------
    let mut inserted = [0usize; 4];
    let mut already = 0usize;
    if !dry_run {
        for e in &merge.ordered {
            let write = FunnelWrite {
                member_id: e.member_id.clone(),
                guild_id: guild_id.clone(),
                event_type: e.event_type.clone(),
                occurred_at: e.occurred_at.clone(),
                source: e.source.clone(),
                metadata: Some(if e.event_type == "gate_cleared" {
                    r#"{"backfill":true,"timestampIsJoinTime":true}"#.to_owned()
                } else {
                    r#"{"backfill":true}"#.to_owned()
                }),
            };
            let (is_new, _) = if e.event_type == "first_voice_session" {
                record_earliest(&db, &write).await.unwrap_or_else(|err| {
                    eprintln!("write failed: {err}");
                    std::process::exit(1);
                })
            } else {
                record_event(&db, &write).await.unwrap_or_else(|err| {
                    eprintln!("write failed: {err}");
                    std::process::exit(1);
                })
            };
            if is_new {
                match e.event_type.as_str() {
                    "member_join" => inserted[0] += 1,
                    "member_leave" => inserted[1] += 1,
                    "gate_cleared" => inserted[2] += 1,
                    _ => inserted[3] += 1,
                }
            } else {
                already += 1;
            }
            if e.event_type == "first_voice_session" {
                if let Some(m) = e.member_id.as_deref() {
                    touch_activity(&db, &guild_id, m, &e.occurred_at)
                        .await
                        .unwrap_or_else(|err| {
                            eprintln!("touchActivity failed: {err}");
                            std::process::exit(1);
                        });
                }
            }
        }
        // Voice sessions found in logs (session starts beyond the first).
        for v in &voice_events {
            let (is_new, _) = record_earliest(&db, v).await.unwrap_or_else(|err| {
                eprintln!("write failed: {err}");
                std::process::exit(1);
            });
            if is_new {
                inserted[3] += 1;
            } else {
                already += 1;
            }
            if let Some(m) = v.member_id.as_deref() {
                touch_activity(&db, &guild_id, m, &v.occurred_at)
                    .await
                    .unwrap_or_else(|err| {
                        eprintln!("touchActivity failed: {err}");
                        std::process::exit(1);
                    });
            }
        }
    }

    // --- 5. invite baseline --------------------------------------------------
    let raw_invites = rest.guild_invites(guild).await.unwrap_or(None);
    let invite_note = match raw_invites {
        None => "FAILED to read invite list - check Manage Server".to_owned(),
        Some(invites) if dry_run => {
            format!("{} invites readable (not stored - dry run)", invites.len())
        }
        Some(invites) => {
            if !dry_run {
                store_invite_baseline(&db, &guild_id, &invites, &now_iso_cli()).await;
            }
            let used = invites.iter().filter(|i| i.uses.unwrap_or(0) > 0).count();
            format!(
                "{} invites stored as the attribution baseline ({used} with uses on the clock)",
                invites.len()
            )
        }
    };

    // --- 6. report -------------------------------------------------------------
    let times: Vec<&str> = {
        let mut t: Vec<&str> = merge
            .ordered
            .iter()
            .map(|e| e.occurred_at.as_str())
            .collect();
        t.sort_unstable();
        t
    };
    let span = if times.is_empty() {
        "none".to_owned()
    } else {
        format!(
            "{} .. {}",
            &times[0][..10.min(times[0].len())],
            &times[times.len() - 1][..10.min(times[times.len() - 1].len())]
        )
    };
    println!(
        "  recovered events     {:>5}   spanning {span}",
        merge.ordered.len()
    );
    println!(
        "    joins              {:>5}   ({} from logs, {} from the member list)",
        merge.log_joins + merge.member_list_joins,
        merge.log_joins,
        merge.member_list_joins
    );
    println!("    leaves             {:>5}", merge.log_leaves);
    println!(
        "    gate clearings     {:>5}   (binary only - occurred_at is the join time)",
        merge.gate_clearings
    );
    println!(
        "    de-duplicated      {:>5}   ({} joins and {} leaves logged twice by different bots, {} member-list joins already in a log)",
        merge.join_dupes_collapsed + merge.leave_dupes_collapsed + merge.member_list_superseded_by_log,
        merge.join_dupes_collapsed,
        merge.leave_dupes_collapsed,
        merge.member_list_superseded_by_log
    );
    println!("    voice sessions     {:>5}", voice_events.len());
    if !dry_run {
        println!(
            "\n  written              {:>5} new ({} joins, {} leaves, {} gate clearings, {} voice)",
            inserted.iter().sum::<usize>(),
            inserted[0],
            inserted[1],
            inserted[2],
            inserted[3]
        );
        println!("  already on file      {already:>5}   (re-run is a no-op, as intended)");
    }
    println!("  invites              {invite_note}");
    print!("{}", scan_report.render());
    if !truncated.is_empty() {
        println!(
            "\n  INCOMPLETE: hit the {max_pages}-page cap on {}. There is older history we did not read. Re-run with --max-pages={}.",
            truncated.join(", "),
            max_pages * 4
        );
    }
    println!(
        "\n  {} Discord requests in {:.1}s.\n",
        rest.requests(),
        t0.elapsed().as_secs_f64()
    );
    db.close().await;
}

fn to_message_view(raw: &twilight_model::channel::message::Message) -> MessageView {
    MessageView {
        id: raw.id.get().to_string(),
        timestamp: raw.timestamp.iso_8601().to_string(),
        content: Some(raw.content.clone()),
        author_id: Some(raw.author.id.get().to_string()),
        author_is_bot: raw.author.bot,
        embeds: raw.embeds.iter().map(to_embed_view).collect(),
    }
}

async fn store_invite_baseline(
    db: &two_bot_cutover::CutoverDb,
    guild_id: &str,
    invites: &[twilight_model::guild::invite::Invite],
    now: &str,
) {
    use std::collections::HashSet;
    let mut seen: HashSet<&str> = HashSet::new();
    for inv in invites {
        seen.insert(inv.code.as_str());
        sqlx::query(
            "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
             ON CONFLICT (guild_id, code) DO UPDATE SET
               uses = excluded.uses, inviter_id = excluded.inviter_id,
               channel_id = excluded.channel_id, updated_at = excluded.updated_at",
        )
        .bind(guild_id)
        .bind(&inv.code)
        .bind(inv.uses.unwrap_or(0) as i64)
        .bind(inv.inviter.as_ref().map(|u| u.id.get().to_string()))
        .bind(inv.channel.as_ref().map(|c| c.id.get().to_string()))
        .bind(now)
        .execute(db.pool())
        .await
        .unwrap_or_else(|e| {
            eprintln!("invite baseline write failed: {e}");
            std::process::exit(1);
        });
    }
    let prev: Vec<(String,)> =
        sqlx::query_as("SELECT code FROM invite_snapshots WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_all(db.pool())
            .await
            .unwrap_or_default();
    for (code,) in prev {
        if !seen.contains(code.as_str()) {
            sqlx::query("DELETE FROM invite_snapshots WHERE guild_id = $1 AND code = $2")
                .bind(guild_id)
                .bind(&code)
                .execute(db.pool())
                .await
                .unwrap_or_else(|e| {
                    eprintln!("invite cleanup failed: {e}");
                    std::process::exit(1);
                });
        }
    }
}

fn now_iso_cli() -> String {
    two_bot_cutover::cli::now_iso()
}
