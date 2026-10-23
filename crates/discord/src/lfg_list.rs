//! Read-only discovery, separate from LFG mutation, delivery and audit paths.

use two_bot_core::{lfg_list, lfg_list::ListCursor, lfg_store};

pub(crate) async fn list(
    pool: &sqlx::PgPool,
    guild: &str,
    channel: &str,
    cursor: Option<&str>,
) -> String {
    let after = match cursor {
        Some(token) => match ListCursor::decode(token, guild, channel) {
            Some(after) => Some(after),
            None => return lfg_list::INVALID_PAGE.into(),
        },
        None => None,
    };
    let posts = match lfg_store::list_lfg_channel(pool, guild, channel, after.as_ref()).await {
        Ok(posts) => posts,
        Err(_) => return lfg_list::UNAVAILABLE.into(),
    };
    lfg_list::render_page(
        guild,
        channel,
        &posts,
        after.is_some(),
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )
    .unwrap_or_else(|_| lfg_list::UNAVAILABLE.into())
}
