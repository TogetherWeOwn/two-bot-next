//! Private active-list projection. No reads beyond the existing saved rows.

use super::{inline_prize, valid_id, GiveawayRow, FAILURE};
use two_bot_core::{giveaway_store::MAX_OPEN_GIVEAWAYS, message_safety};

const HEADER: &str = "Saved active giveaways (not live receipts; ? = unavailable prize):\n";
const PRIZE_CHARS: usize = 32;

pub(super) fn render_active_list(rows: &[GiveawayRow]) -> String {
    if rows.is_empty() {
        return "No open giveaways.".into();
    }
    if rows.len() > MAX_OPEN_GIVEAWAYS as usize || rows.iter().any(|row| !valid_id(&row.id)) {
        return FAILURE.into();
    }
    // Divide the final UTF-16 budget, not bytes or unescaped prize characters.
    // Only previews shrink: every ID, destination and deadline stays whole.
    let row_budget =
        (message_safety::CONTENT_LIMIT - message_safety::text_len(HEADER)) / rows.len();
    let mut text = HEADER.to_owned();
    for row in rows {
        let state = match row.status.as_str() {
            "open" => format!("open <t:{}:f>", row.ends_at.unix_timestamp()),
            "paused" => "paused (no countdown)".into(),
            _ => return FAILURE.into(),
        };
        let reference = if !saved_snowflake(&row.guild_id) || !saved_snowflake(&row.channel_id) {
            "destination unavailable (link omitted)".into()
        } else if saved_snowflake(&row.message_id) {
            format!(
                "<https://discord.com/channels/{}/{}/{}>",
                row.guild_id, row.channel_id, row.message_id
            )
        } else {
            format!("channel {}; message unconfirmed", row.channel_id)
        };
        let prefix = format!("`{}` ", row.id);
        let suffix = format!(" · {state} · {reference}\n");
        let framing = message_safety::text_len(&prefix) + message_safety::text_len(&suffix);
        let Some(prize_budget) = row_budget.checked_sub(framing).filter(|budget| *budget > 0)
        else {
            // Corrupt saved facts must be unavailable, never a partial success.
            return FAILURE.into();
        };
        text.push_str(&prefix);
        text.push_str(&list_prize(&row.prize, prize_budget));
        text.push_str(&suffix);
    }
    text
}

fn saved_snowflake(id: &str) -> bool {
    id.parse::<u64>()
        .is_ok_and(|value| value != 0 && value.to_string() == id)
}

/// Escape first and budget the mention-neutralized form that the executor
/// sends. Keeping whole source characters also keeps escape pairs intact.
/// At most 33 display characters are needed to detect the 32-character cut.
/// Removing line separators prevents a prize from forging an additional row.
pub(super) fn list_prize(prize: &str, budget: usize) -> String {
    let displayable: String = prize
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
        .collect();
    // Invisible-only text carries no displayable prize, like control-only text.
    if !message_safety::has_message_text(&displayable) {
        return "?".into();
    }
    let visible: String = displayable.chars().take(PRIZE_CHARS + 1).collect();
    let shortened = visible.chars().count() > PRIZE_CHARS;
    let visible = message_safety::neutralize_mentions(&visible);
    let count = visible.chars().count();
    let mut bound = count.min(PRIZE_CHARS);
    loop {
        let mut rendered = inline_prize(&visible, bound);
        if shortened || bound < count {
            rendered.push('…');
        }
        if message_safety::text_len(&rendered) <= budget {
            return rendered;
        }
        bound -= 1; // The caller reserves at least one unit for the ellipsis.
    }
}
