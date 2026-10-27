//! Private read paths, deliberately outside moderation mutation admission.

use super::{integer_option, invoking_channel_id, ChannelModerationRuntime, ChannelReply};
use twilight_model::application::interaction::{Interaction, InteractionData};
use two_bot_core::{channel_moderation as domain, HandlerId, InteractionRouter, SlashOutcome};

pub(super) fn handler(name: &str) -> Option<HandlerId> {
    match name {
        "purge-preview" => Some(HandlerId::PurgePreview),
        "channel-inspect" => Some(HandlerId::ChannelInspect),
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
    let (Some(guild), Some(channel), Some(_actor)) = (
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
    } else {
        if !data.options.is_empty() {
            return ChannelReply::refused("Channel inspect takes no options; try again.");
        }
        inspect(runtime, &channel, &guild.to_string()).await
    }
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
        let date =
            time::OffsetDateTime::parse(&locked_at, &time::format_description::well_known::Rfc3339)
                .map_or_else(|_| "unknown".to_owned(), |at| at.date().to_string());
        let actor = actor
            .filter(|id| {
                id.parse::<u64>()
                    .is_ok_and(|n| n != 0 && n.to_string() == *id)
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let reason: String = record.reason.chars().take(512).collect();
        format!("lockdown: active (saved recovery state, not live overwrite verification)\nreason: {reason}\nset at: {date}\nset by: {actor}")
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
