//! Message-milestone scan CLI (legacy `scripts/backfill-messages.ts`).
//!
//! Companion to `backfill`: walks conversation-channel history and records
//! each member's earliest three messages, which makes AM7's text half exact
//! rather than an upper bound. Read-only against Discord; safe to re-run —
//! a milestone only ever moves earlier, never later.
//!
//! `backfill-messages --dry-run` reports and writes nothing.

use std::collections::HashMap;
use twilight_model::channel::ChannelType;
use twilight_model::id::Id;
use two_bot_cutover::cli::{open_db, Args, ScanReport};
use two_bot_cutover::{
    find_early_messages, fold_messages, is_conversation_channel, iso_to_millis, record_earliest,
    touch_activity, FunnelWrite, MemberMessages, RestClient, ScannedMessage, FORUM_CHANNEL_TYPES,
    THREAD_CHANNEL_TYPES,
};

const MESSAGE_RUNGS: [&str; 3] = ["first_message", "second_message", "third_message"];

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = Args::parse(&argv);
    let dry_run = args.has("dry-run");
    let max_pages: usize = args
        .get("max-pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let since_ms = args.get("since").and_then(iso_to_millis);

    let token = std::env::var("DISCORD_TOKEN")
        .or_else(|_| std::env::var("DISCORD_BOT_TOKEN"))
        .unwrap_or_default();
    if token.is_empty() {
        eprintln!("Missing DISCORD_TOKEN (or DISCORD_BOT_TOKEN).");
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
    let guild_id = std::env::var("DISCORD_GUILD_ID").unwrap_or_default();
    if guild_id.is_empty() {
        eprintln!("Missing DISCORD_GUILD_ID.");
        std::process::exit(2);
    }
    if guild_id == two_bot_cutover::LIVE_GUILD_ID && !args.has("allow-live-guild") {
        eprintln!(
            "Refusing live guild {}. Use --allow-live-guild only for an owner-approved rollout.",
            two_bot_cutover::LIVE_GUILD_ID
        );
        std::process::exit(2);
    }

    let t0 = std::time::Instant::now();
    let rest = RestClient::with_proxy(token, args.values.get("discord-base").cloned());
    let db = open_db(&args, false).await;
    let guild: Id<twilight_model::id::marker::GuildMarker> = Id::new(guild_id.parse().unwrap_or(0));

    println!(
        "\nTWO message backfill{}",
        if dry_run {
            "  (DRY RUN - nothing will be written)"
        } else {
            ""
        }
    );
    println!("  guild {guild_id}   max {max_pages} pages/channel\n");

    let channels = rest
        .guild_channels(guild)
        .await
        .unwrap_or(None)
        .unwrap_or_default();
    let cat_name: HashMap<String, String> = channels
        .iter()
        .filter(|c| c.kind == ChannelType::GuildCategory)
        .filter_map(|c| Some((c.id.get().to_string(), c.name.clone()?)))
        .collect();

    let conversation: Vec<_> = channels
        .iter()
        .filter(|c| {
            let code = channel_type_code(&c.kind);
            let name = c.name.as_deref().unwrap_or("");
            let cat = c
                .parent_id
                .as_ref()
                .and_then(|p| cat_name.get(&p.get().to_string()));
            is_conversation_channel(name, cat.map(String::as_str).unwrap_or(""), code)
        })
        .collect();
    let channels_considered = conversation.len();

    // Forum posts live in threads, and threads are separate channels.
    let mut thread_ids: Vec<String> = Vec::new();
    if let Ok(Some(active)) = rest.active_threads(guild).await {
        for t in &active.threads {
            if THREAD_CHANNEL_TYPES.contains(&channel_type_code(&t.kind)) {
                thread_ids.push(t.id.get().to_string());
            }
        }
    }
    for forum in conversation
        .iter()
        .filter(|c| channel_type_code(&c.kind) == FORUM_CHANNEL_TYPES[0])
    {
        if let Ok(Some(arch)) = rest.public_archived_threads(forum.id).await {
            for t in &arch.threads {
                thread_ids.push(t.id.get().to_string());
            }
        }
    }

    let mut early: HashMap<String, MemberMessages> = HashMap::new();
    let mut last_active: HashMap<String, String> = HashMap::new();
    let (mut channels_scanned, mut threads_scanned, mut messages_read) = (0usize, 0usize, 0usize);
    let mut truncated: Vec<String> = Vec::new();
    let mut scan_report = ScanReport::default();
    let mut scanned_back_to: Option<String> = None;

    // Text channels first, then threads (forum posts are threads only).
    let mut targets: Vec<(Id<twilight_model::id::marker::ChannelMarker>, bool)> = conversation
        .iter()
        .filter(|c| channel_type_code(&c.kind) != FORUM_CHANNEL_TYPES[0])
        .map(|c| (c.id, false))
        .collect();
    for id in thread_ids {
        if let Ok(n) = id.parse::<u64>() {
            targets.push((Id::new(n), true));
        }
    }

    for (target, is_thread) in targets {
        let page = match rest.scan_channel(target, max_pages, since_ms).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("scan of channel {} failed: {e}", target.get());
                scan_report.record(
                    &target.get().to_string(),
                    two_bot_cutover::ScanCompletion::RequestFailed,
                );
                continue;
            }
        };
        scan_report.record(&target.get().to_string(), page.completion);
        if is_thread {
            threads_scanned += 1;
        } else {
            channels_scanned += 1;
        }
        if page.truncated {
            truncated.push(target.get().to_string());
        }
        messages_read += page.messages.len();
        if let Some(back) = page.scanned_back_to {
            if scanned_back_to.as_ref().is_none_or(|o| back < *o) {
                scanned_back_to = Some(back);
            }
        }
        let batch: Vec<ScannedMessage> = page
            .messages
            .iter()
            .map(|m| ScannedMessage {
                id: m.id.get().to_string(),
                at: m.timestamp.iso_8601().to_string(),
                author_id: Some(m.author.id.get().to_string()),
                author_is_bot: m.author.bot,
            })
            .collect();
        fold_messages(
            &mut early,
            &mut last_active,
            &target.get().to_string(),
            &batch,
        );
    }

    let (_early_map, summary) = find_early_messages(
        &early,
        channels_considered,
        channels_scanned,
        threads_scanned,
        messages_read,
        truncated.clone(),
        scanned_back_to.clone(),
    );

    let pad = |n: usize| format!("{n:>5}");
    println!(
        "  conversation channels {}   (log/bot channels skipped)",
        pad(summary.channels_considered)
    );
    println!("  channels scanned      {}", pad(summary.channels_scanned));
    println!("  threads scanned       {}", pad(summary.threads_scanned));
    println!("  messages read         {}", pad(summary.messages_read));
    println!("  members who ever posted {}", pad(summary.authors_seen));
    println!(
        "  members with 3+ posts   {}   (AM7 text bar, {})",
        pad(summary.authors_with_full_ladder),
        if scan_report.has_interruptions() || !summary.truncated.is_empty() {
            "lower bound - incomplete history"
        } else {
            "exactly"
        }
    );
    println!(
        "  oldest message reached  {}",
        summary
            .scanned_back_to
            .as_deref()
            .unwrap_or("n/a")
            .get(..10)
            .unwrap_or("n/a")
    );

    if !dry_run {
        let mut written = 0usize;
        let mut ladders = 0usize;
        for m in early.values() {
            for (i, msg) in m.rungs.iter().enumerate() {
                let (is_new, _) = record_earliest(
                    &db,
                    &FunnelWrite {
                        member_id: Some(m.member_id.clone()),
                        guild_id: guild_id.clone(),
                        event_type: MESSAGE_RUNGS[i].to_owned(),
                        occurred_at: msg.at.clone(),
                        source: format!("channel:{}", msg.channel_id),
                        metadata: Some(r#"{"backfill":"message_scan"}"#.to_owned()),
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
            if m.rungs.len() == 3 {
                ladders += 1;
            }
        }
        for (member, at) in &last_active {
            touch_activity(&db, &guild_id, member, at)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("touchActivity failed: {e}");
                    std::process::exit(1);
                });
        }
        println!(
            "\n  written               {} new message milestone events",
            pad(written)
        );
        println!("  third_message on file {} members", pad(ladders));
        println!("  (a re-run writes 0 and is a no-op, as intended)");
    }

    print!("{}", scan_report.render());
    if !summary.truncated.is_empty() {
        println!(
            "\n  INCOMPLETE: hit the {max_pages}-page cap on {} channel(s).\n  A capped scan sees a subset of each member's posts, so the milestones we\n  recorded are at or LATER than the true ones - never earlier. AM7 can\n  therefore miss a member here, but it cannot wrongly admit one, and a\n  deeper re-run only moves the milestones towards the truth.\n  Re-run with --max-pages={}.\n  {}",
            summary.truncated.len(),
            max_pages * 4,
            summary.truncated.join(", ")
        );
    }
    println!(
        "\n  {} Discord requests in {:.1}s.\n",
        rest.requests(),
        t0.elapsed().as_secs_f64()
    );
    db.close().await;
}

fn channel_type_code(kind: &ChannelType) -> u8 {
    match kind {
        ChannelType::GuildText => 0,
        ChannelType::GuildAnnouncement => 5,
        ChannelType::GuildForum => 15,
        ChannelType::AnnouncementThread => 10,
        ChannelType::PublicThread => 11,
        ChannelType::PrivateThread => 12,
        _ => 255,
    }
}
