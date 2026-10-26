//! Private read paths, deliberately outside moderation mutation admission.

use super::{integer_option, invoking_channel_id, ChannelModerationRuntime, ChannelReply};
use twilight_model::application::interaction::{Interaction, InteractionData};
use two_bot_core::{channel_moderation as domain, HandlerId, InteractionRouter, SlashOutcome};

/// Rows rendered per inventory page: ten complete authorized records fit the
/// 2000 UTF-16-unit reply budget with bounded reasons (see
/// [`INVENTORY_REASON_UNITS`]).
const INVENTORY_ROWS_PER_PAGE: usize = 10;
/// Database overscan for one inventory read. Visibility filtering happens in
/// the runtime from gateway evidence, so this bound covers hidden-plus-cold
/// rows ahead of the visible set rather than page offsets.
const INVENTORY_SQL_LIMIT: i64 = 1000;
/// Display bound for one inventoried reason, in UTF-16 units at a character
/// boundary with an ellipsis marker. Ten rows of worst-case snowflake ids,
/// full instants, setters and reasons stay inside the reply budget.
const INVENTORY_REASON_UNITS: usize = 64;

pub(super) fn handler(name: &str) -> Option<HandlerId> {
    match name {
        "purge-preview" => Some(HandlerId::PurgePreview),
        "channel-inspect" => Some(HandlerId::ChannelInspect),
        "lockdown-records" => Some(HandlerId::LockdownRecords),
        _ => None,
    }
}

pub(super) async fn execute(
    runtime: &ChannelModerationRuntime,
    router: &InteractionRouter,
    interaction: &Interaction,
    handler: HandlerId,
    outcome: SlashOutcome,
) -> ChannelReply {
    match outcome {
        SlashOutcome::Handled { handler: routed }
            if routed == handler && router.handler_for(&routed).is_some() => {}
        SlashOutcome::Refuse { refusal } => {
            return ChannelReply::refused(format!(
                "{} Check the guild and your permissions, then try again.",
                refusal.message()
            ));
        }
        _ => return ChannelReply::refused("Channel inspection is unavailable; try again shortly."),
    }
    let (Some(guild), Some(channel), Some(actor)) = (
        interaction.guild_id,
        invoking_channel_id(interaction),
        interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref()),
    ) else {
        return ChannelReply::refused("A guild member and channel are required; try again there.");
    };
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return ChannelReply::refused("Use a channel read command and try again.");
    };
    if handler == HandlerId::PurgePreview {
        if data.options.iter().any(|option| option.name != "count") {
            return ChannelReply::refused("Supply only \"count\" between 1 and 100; try again.");
        }
        let count = match integer_option(data, "count").and_then(|count| {
            domain::validate_purge_count(count).map_err(|error| error.to_string())
        }) {
            Ok(count) => count,
            Err(text) => return ChannelReply::refused(format!("{text}; try again.")),
        };
        preview(runtime, &channel, count).await
    } else if handler == HandlerId::LockdownRecords {
        if data.options.iter().any(|option| option.name != "page") {
            return ChannelReply::refused("Supply only \"page\" between 1 and 100; try again.");
        }
        let page = if data.options.is_empty() {
            domain::MIN_RECORDS_PAGE
        } else {
            match integer_option(data, "page").and_then(|page| {
                domain::validate_inventory_page(page).map_err(|error| error.to_string())
            }) {
                Ok(page) => page,
                Err(text) => return ChannelReply::refused(format!("{text}; try again.")),
            }
        };
        inventory(runtime, guild.get(), actor.id.get(), page).await
    } else {
        if !data.options.is_empty() {
            return ChannelReply::refused("Channel inspect takes no options; try again.");
        }
        inspect(runtime, &channel, &guild.to_string()).await
    }
}

/// Shared saved-time display for the inventory and current-channel inspect:
/// the stored RFC3339 instant verbatim, so two locks on the same date stay
/// distinguishable. Malformed legacy provenance is `None` (rendered
/// `unknown`), never a fabricated or normalized instant.
pub(super) fn format_saved_instant(locked_at: &str) -> Option<String> {
    time::OffsetDateTime::parse(locked_at, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|_| locked_at.to_owned())
}

/// Shared setter display: a canonical non-zero snowflake, else `unknown`.
/// Malformed legacy ids are never echoed or fabricated into a mention.
pub(super) fn display_setter(setter: Option<String>) -> String {
    setter
        .filter(|id| {
            id.parse::<u64>()
                .is_ok_and(|n| n != 0 && n.to_string() == *id)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// One inventoried reason: single-line, bounded in UTF-16 units at a
/// character boundary, with an ellipsis marker when truncated. Channel ids
/// are never truncated; only reasons yield to the reply budget.
pub(super) fn inventory_reason(reason: &str) -> String {
    let single: String = reason
        .chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            _ => c,
        })
        .collect();
    let mut units = 0usize;
    let mut kept = String::new();
    for c in single.chars() {
        let width = c.len_utf16();
        if units + width > INVENTORY_REASON_UNITS {
            kept.push('…');
            return kept;
        }
        units += width;
        kept.push(c);
    }
    kept
}

async fn preview(runtime: &ChannelModerationRuntime, channel: &str, count: u64) -> ChannelReply {
    let sample = match runtime.executor.sample_purge_messages(channel, count).await {
        Ok(sample) => sample,
        Err(_) => {
            return ChannelReply::refused(
                "Could not read purge history; try again shortly. Nothing was deleted.",
            )
        }
    };
    let ids: Vec<u64> = sample.ids.iter().map(|id| id.get()).collect();
    let now_ms = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .try_into()
        .unwrap_or(0);
    let plan = domain::plan_purge(&ids, now_ms);
    let (span, times) = match (ids.first(), ids.last()) {
        (Some(first), Some(last)) => (
            format!("{first}..{last}"),
            format!(
                "<t:{}:f>..<t:{}:f>",
                domain::message_created_ms(*first) / 1000,
                domain::message_created_ms(*last) / 1000
            ),
        ),
        _ => ("none".to_owned(), "none".to_owned()),
    };
    ChannelReply {
        text: format!(
            "Purge preview — newest-page sample, listed: {} (up to {count})\n\
             targetable: {}\nspan: {span}\ntime span (newest to oldest): {times}\n\
             pinned: {}, bot: {}, system: {}\nbulk: {}, single: {}\n\
             Pinned and own-bot posts are excluded (categories can overlap). \
             System messages remain candidates; age excludes messages from bulk, not single deletion. \
             Sampling only, not a guarantee: messages, pins, permissions and age can change before /purge. Nothing was deleted.",
            sample.listed, ids.len(), sample.pinned, sample.own_bot, sample.system,
            plan.bulk.len(), plan.single.len(),
        ),
        outcome: "purge_preview".to_owned(),
        replayed: false,
    }
}

/// Guild inventory of saved lockdown recovery records: only rows whose
/// channel the actor may view under cached guild evidence, in stable
/// channel order with stateless pages. Saved evidence only, never live
/// overwrite verification; no live reads, claims, cleanup or audit writes.
async fn inventory(
    runtime: &ChannelModerationRuntime,
    guild_id: u64,
    actor_id: u64,
    page: u64,
) -> ChannelReply {
    use twilight_model::id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    };
    let unavailable =
        || ChannelReply::refused("Saved recovery records are unavailable; try again shortly.");
    let query_failed = || {
        ChannelReply::refused(
            "Could not read saved recovery records; try again shortly. Nothing was changed.",
        )
    };
    let Some(cache) = runtime.recovery_visibility.get() else {
        return unavailable();
    };
    let (Some(guild), Some(actor)) = (
        Id::<GuildMarker>::new_checked(guild_id),
        Id::<UserMarker>::new_checked(actor_id),
    ) else {
        return unavailable();
    };
    // Actor authority first: a member whose guild facts are cold cannot
    // authorize any row, so refuse before touching saved reasons.
    if cache.permissions().root(actor, guild).is_err() {
        return unavailable();
    }
    let guild_text = guild_id.to_string();
    let rows = match runtime
        .store
        .list_lockdown_inventory(&guild_text, INVENTORY_SQL_LIMIT)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return query_failed(),
    };
    let visible: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.channel_id.parse::<u64>().is_ok_and(|channel| {
                Id::<ChannelMarker>::new_checked(channel).is_some_and(|id| {
                    cache
                        .permissions()
                        .in_channel(actor, id)
                        .is_ok_and(|permissions| {
                            permissions.contains(twilight_model::guild::Permissions::VIEW_CHANNEL)
                        })
                })
            })
        })
        .collect();
    let total = visible.len();
    if total == 0 {
        return ChannelReply {
            text: "No saved lockdown recovery records in this guild. Saved evidence only, not live lockdown verification. For the current channel: /channel-inspect.".to_owned(),
            outcome: "lockdown_records".to_owned(),
            replayed: false,
        };
    }
    let pages = total.div_ceil(INVENTORY_ROWS_PER_PAGE);
    let index = (page as usize).saturating_sub(1);
    let Some(window) = visible.chunks(INVENTORY_ROWS_PER_PAGE).nth(index) else {
        return ChannelReply {
            text: format!("No records on page {page}. The saved recovery inventory holds {pages} pages ({total} records). For the current channel: /channel-inspect."),
            outcome: "lockdown_records".to_owned(),
            replayed: false,
        };
    };
    let mut text = format!("Saved lockdown recovery records — page {page} of {pages} ({total} records). Saved evidence only, not live lockdown verification.\n");
    for row in window {
        let Some(saved) = format_saved_instant(&row.locked_at) else {
            text.push_str(&format!(
                "• {} — saved unknown — set by unknown — reason: {}\n",
                row.channel_id,
                inventory_reason(&row.reason)
            ));
            continue;
        };
        let setter = match runtime
            .store
            .get_inventory_setter(&guild_text, &row.channel_id, &row.reason, &row.locked_at)
            .await
        {
            Ok(setter) => display_setter(setter),
            Err(_) => return query_failed(),
        };
        text.push_str(&format!(
            "• {} — saved {saved} — set by {setter} — reason: {}\n",
            row.channel_id,
            inventory_reason(&row.reason)
        ));
    }
    if index + 1 < pages {
        text.push_str(&format!(
            "Use /lockdown-records page: {} for the next page. For the current channel: /channel-inspect.",
            page + 1
        ));
    } else {
        text.push_str("For the current channel: /channel-inspect.");
    }
    ChannelReply {
        text,
        outcome: "lockdown_records".to_owned(),
        replayed: false,
    }
}

async fn inspect(runtime: &ChannelModerationRuntime, channel: &str, guild: &str) -> ChannelReply {
    let unavailable = || {
        ChannelReply::refused(
            "Could not read saved channel restrictions; try again shortly. Nothing was changed.",
        )
    };
    let record = match runtime.store.get_lockdown(channel).await {
        Ok(record) => record,
        Err(_) => return unavailable(),
    };
    let saved = if let Some(record) = record {
        if record.guild_id != guild {
            return ChannelReply::refused(
                "Saved restriction guild does not match; try again in the correct channel.",
            );
        }
        let (locked_at, actor) = match runtime.store.get_lockdown_details(&record).await {
            Ok(Some(details)) => details,
            // A concurrent change is unavailable evidence, never fabricated absence.
            _ => return unavailable(),
        };
        let set_at = format_saved_instant(&locked_at).unwrap_or_else(|| "unknown".to_owned());
        let actor = display_setter(actor);
        let reason: String = record.reason.chars().take(512).collect();
        format!("lockdown: active (saved recovery state, not live overwrite verification)\nreason: {reason}\nset at: {set_at}\nset by: {actor}")
    } else {
        "lockdown: none\nset at: unknown\nset by: unknown".to_owned()
    };
    let slowmode = match runtime.executor.fetch_channel_document(channel).await {
        Ok(doc) => {
            if doc["id"].as_str() != Some(channel)
                || doc["guild_id"].as_str() != Some(guild)
                || !matches!(doc["type"].as_u64(), Some(0 | 5))
            {
                return ChannelReply::refused("Could not verify this guild channel; try again in a text channel. Nothing was changed.");
            }
            match doc.get("rate_limit_per_user") {
                None => "off".to_owned(),
                Some(value) => match value.as_u64() {
                    Some(0) => "off".to_owned(),
                    Some(seconds) if seconds <= domain::MAX_SLOWMODE_SECONDS => {
                        format!("{seconds}s")
                    }
                    _ => "unknown — try again shortly".to_owned(),
                },
            }
        }
        Err(_) => "unknown — try again shortly".to_owned(),
    };
    ChannelReply {
        text: format!("Channel restrictions snapshot\n{saved}\nslowmode: {slowmode}\nslowmode set at: unknown\nslowmode set by: unknown\nRead-only snapshot; restrictions may change. Nothing was changed."),
        outcome: "channel_inspect".to_owned(),
        replayed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_instants_echo_verbatim_and_malformed_stay_unknown() {
        for instant in [
            "2026-10-20T10:09:10.123456+02:00",
            "2026-10-20T08:09:10.123456Z",
            "2026-10-20T08:09:10.123456+00:00",
            "2026-10-01T00:00:00.000Z",
        ] {
            assert_eq!(format_saved_instant(instant).as_deref(), Some(instant));
        }
        // Same-date locks stay distinguishable through the shared display.
        assert_ne!(
            format_saved_instant("2026-10-20T10:09:10.123456+02:00"),
            format_saved_instant("2026-10-20T10:10:11.654321+02:00")
        );
        for malformed in ["", "legacy-not-a-time", "2026-10-20", "1970-01-01"] {
            assert_eq!(format_saved_instant(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn setters_render_only_as_canonical_snowflakes() {
        assert_eq!(
            display_setter(Some("333333333333333333".to_owned())),
            "333333333333333333"
        );
        for bad in [
            None,
            Some("".to_owned()),
            Some("0".to_owned()),
            Some("000333333333333333333".to_owned()),
            Some("not-an-id".to_owned()),
            Some("18446744073709551616".to_owned()),
        ] {
            assert_eq!(display_setter(bad.clone()), "unknown", "{bad:?}");
        }
    }

    #[test]
    fn inventory_reasons_stay_single_line_and_inside_the_unit_budget() {
        assert_eq!(inventory_reason("visible reason"), "visible reason");
        assert_eq!(inventory_reason("a\nb\rc"), "a b c");
        let astral = "🦀".repeat(900);
        let kept = inventory_reason(&astral);
        assert!(kept.ends_with('…'));
        assert!(kept.encode_utf16().count() <= INVENTORY_REASON_UNITS + 1);
        // Ten worst-case rows plus header and continuation stay in budget.
        let row: String = format!(
            "• {} — saved {} — set by {} — reason: {}\n",
            "9".repeat(20),
            "2026-10-20T10:09:10.123456789+02:00",
            "9".repeat(20),
            kept,
        );
        let page = format!(
            "Saved lockdown recovery records — page 100 of 100 (1000 records). Saved evidence only, not live lockdown verification.\n{}Use /lockdown-records page: 100 for the next page. For the current channel: /channel-inspect.",
            row.repeat(INVENTORY_ROWS_PER_PAGE),
        );
        assert!(page.encode_utf16().count() <= 2000);
    }
}
