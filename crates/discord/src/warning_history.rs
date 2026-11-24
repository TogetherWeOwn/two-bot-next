//! Pure formatting for retained warning metadata.

use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use twilight_model::http::interaction::InteractionResponseData;
use two_bot_core::{
    lfg_list::canonical_snowflake, member_moderation_store::WarningHistoryPage,
    message_safety::text_len,
};

const MAX_ROWS: usize = 5;
const MAX_ROW_UNITS: usize = 140;
const MAX_BODY_UNITS: usize = 1_900;
const MAX_TIMESTAMP_BYTES: usize = 32;
const REASONS_NOTICE: &str = "Reasons are not shown in this view.";

/// A page that cannot be represented in the warning history display format.
#[derive(Debug, PartialEq, Eq)]
pub struct WarningHistoryUnavailable;

/// Prepare content and empty allowed mentions for a future private reply.
/// The caller must supply a target already bound to the page by the read path.
/// The interaction layer owns visibility, the empty versus end outcome and controls.
pub fn render_warning_history(
    target: &str,
    page: &WarningHistoryPage,
) -> Result<InteractionResponseData, WarningHistoryUnavailable> {
    if target.len() > 20 || !canonical_snowflake(target) || page.entries.len() > MAX_ROWS {
        return Err(WarningHistoryUnavailable);
    }

    let mut body = format!("Recorded user ID {target}\n");
    for entry in &page.entries {
        let reference = &entry.reference;
        let recorded_at = &entry.recorded_at_utc;
        if !valid_reference(reference) || !valid_timestamp(recorded_at) {
            return Err(WarningHistoryUnavailable);
        }
        let actor = match entry.recorder_id {
            Some(id) if id != 0 => format!("Actor ID {id}"),
            _ => "Actor unavailable".to_owned(),
        };
        let row = format!("{reference} | {recorded_at} | {actor}\n");
        if text_len(&row) > MAX_ROW_UNITS {
            return Err(WarningHistoryUnavailable);
        }
        body.push_str(&row);
    }
    body.push_str(REASONS_NOTICE);
    if text_len(&body) > MAX_BODY_UNITS {
        return Err(WarningHistoryUnavailable);
    }
    Ok(InteractionResponseData {
        content: Some(body),
        allowed_mentions: Some(Default::default()),
        ..Default::default()
    })
}

fn valid_reference(reference: &str) -> bool {
    reference.strip_prefix("W-").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn valid_timestamp(value: &str) -> bool {
    value.len() <= MAX_TIMESTAMP_BYTES
        && value.ends_with('Z')
        && OffsetDateTime::parse(value, &Rfc3339).is_ok_and(|time| {
            time.year() > 0
                && time
                    .format(&Rfc3339)
                    .is_ok_and(|canonical| canonical == value)
        })
}

#[cfg(test)]
mod tests;
