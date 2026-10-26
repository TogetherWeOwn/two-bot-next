//! Read-only saved-roster inspection, separate from LFG mutation, signup
//! cooldown, audit and message-refresh paths.
//!
//! Authorization order is contractual: the router has already revalidated
//! the configured guild, the announcements gate and Manage Events for this
//! invocation; here the guild-scoped post read plus the invoking-channel
//! check must pass before any role or signup row is read, so unknown,
//! other-guild and other-channel IDs share one private miss.

use two_bot_core::{lfg_inspect, lfg_inspect::InspectSelection, lfg_store};

pub(crate) async fn inspect(
    pool: &sqlx::PgPool,
    guild: &str,
    channel: &str,
    post_id: &str,
    selection: &InspectSelection,
) -> String {
    let post = match lfg_store::get_lfg(pool, guild, post_id).await {
        Ok(Some(post)) if post.channel_id == channel => post,
        Ok(_) => return lfg_inspect::MISS.into(),
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    let roles = match lfg_store::list_lfg_roles(pool, post_id).await {
        Ok(roles) => roles,
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    let signups = match lfg_store::list_lfg_signups(pool, post_id).await {
        Ok(signups) => signups,
        Err(_) => return lfg_inspect::UNAVAILABLE.into(),
    };
    lfg_inspect::render(&post, &roles, &signups, selection)
}
