//! `AuditMirror` transport on [`ActionExecutor`] (TOG-10345).
//!
//! The executor owns pacing, the nonce/`allowed_mentions` wire shape and
//! Discord's error taxonomy; this module is the narrow adaptation between
//! [`DiscordError`] and the audit service's [`MirrorError`] classification,
//! plus the strict parsing that turns channel/history documents into the
//! core's mirror types. Everything the crash protocol needs to decide is
//! preserved: refusal vs rate-limit vs uncertain, proven-empty vs unreadable.

#[cfg(test)]
mod tests;

use two_bot_core::audit_mirror::{
    AuditMirror, MirrorChannel, MirrorError, MirrorMessage, MirrorOverwrite,
};

use crate::executor::{ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordError};

/// `DiscordError` → `MirrorError`: `Rejected` stays provably-unsent/refused,
/// `RateLimited` keeps its own verdict, and both `Timeout` and `Unavailable`
/// collapse into `Uncertain` — the audit protocol treats them identically
/// (boundary retained, reconcile-only).
fn mirror_error(error: DiscordError) -> MirrorError {
    match error {
        DiscordError::Rejected(detail) => MirrorError::Rejected(detail),
        DiscordError::RateLimited => MirrorError::RateLimited,
        DiscordError::Timeout => MirrorError::Uncertain("timeout".to_owned()),
        DiscordError::Unavailable(detail) => MirrorError::Uncertain(detail),
    }
}

/// Unreadable evidence is not proof of a permission refusal or marker absence.
fn unreadable(channel_id: &str, detail: &str) -> MirrorError {
    MirrorError::Uncertain(format!("unreadable channel {channel_id}: {detail}"))
}

impl AuditMirror for ActionExecutor {
    /// `POST /channels/{c}/messages` through the executor's paced lane. The
    /// wire shape (deterministic nonce + `enforce_nonce`, empty
    /// `allowed_mentions`, the 2000-utf16 bound) is owned by
    /// `execute_channel` — there is no private HTTP client here.
    async fn post_mirror_checked<Fut, E>(
        &self,
        channel_id: &str,
        content: &str,
        nonce: &str,
        authorize: Fut,
    ) -> Result<Result<String, MirrorError>, E>
    where
        Fut: std::future::Future<Output = Result<(), E>> + Send,
        E: Send,
    {
        let call = ChannelCall::PostMessage {
            channel_id: channel_id.to_owned(),
            content: content.to_owned(),
            nonce: Some(nonce.to_owned()),
        };
        let mut lane = self.paced_lane(false).await;
        authorize.await?;
        self.stamp_paced_lane(&mut lane, false);
        Ok(match self.execute_channel(&call).await {
            Ok(ChannelCallOutcome::Posted { message_id }) => Ok(message_id),
            // The executor's PostMessage arm only constructs `Posted`; other
            // variants are unreachable but must still map safely.
            Ok(_) => Ok(String::new()),
            Err(error) => Err(mirror_error(error)),
        })
    }

    /// `GET /channels/{c}` reduced to `guild_id` + the `@everyone` overwrite
    /// (type 0 whose id is the channel's own guild). A missing `guild_id`
    /// becomes `""` so the core fence classifies it `WrongGuild`; malformed
    /// overwrite rows are `Uncertain` — never silently "absent".
    async fn channel_document(&self, channel_id: &str) -> Result<MirrorChannel, MirrorError> {
        let doc = self
            .fetch_channel_document(channel_id)
            .await
            .map_err(mirror_error)?;
        let guild_id = doc
            .get("guild_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let everyone = match doc.get("permission_overwrites") {
            // No array at all: the document carries no overwrite evidence —
            // the core gate treats `everyone: None` as public.
            None => None,
            Some(value) => {
                let rows = value.as_array().ok_or_else(|| {
                    unreadable(channel_id, "permission_overwrites is not an array")
                })?;
                let mut found = None;
                for (index, entry) in rows.iter().enumerate() {
                    let row = entry.as_object().ok_or_else(|| {
                        unreadable(
                            channel_id,
                            &format!("permission_overwrites[{index}] is not an object"),
                        )
                    })?;
                    let kind = row.get("type").and_then(|v| v.as_u64()).ok_or_else(|| {
                        unreadable(
                            channel_id,
                            &format!("permission_overwrites[{index}] has a non-numeric type"),
                        )
                    })?;
                    let id = row.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                        unreadable(
                            channel_id,
                            &format!("permission_overwrites[{index}] has a non-string id"),
                        )
                    })?;
                    if kind == 0 && id == guild_id {
                        let mask = |field: &str| {
                            row.get(field)
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                                .ok_or_else(|| {
                                    unreadable(
                                        channel_id,
                                        &format!("@everyone overwrite has a non-string {field}"),
                                    )
                                })
                        };
                        found = Some(MirrorOverwrite {
                            allow: mask("allow")?,
                            deny: mask("deny")?,
                        });
                        break;
                    }
                }
                found
            }
        };
        Ok(MirrorChannel { guild_id, everyone })
    }

    /// `GET /channels/{c}/messages` reduced to `{id, author.id, content}`.
    /// Reject an unreadable page as uncertain in its entirety: filtering even
    /// one row would falsify the page's exhaustion, boundary or cursor evidence.
    async fn channel_history(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<Vec<MirrorMessage>, MirrorError> {
        let rows = self
            .fetch_channel_messages(channel_id, before, limit)
            .await
            .map_err(mirror_error)?;
        rows.iter()
            .map(|row| {
                let parse = || {
                    let id = row.get("id")?.as_str()?;
                    id.parse::<u64>().ok()?;
                    Some(MirrorMessage {
                        id: id.to_owned(),
                        author_id: row.get("author")?.get("id")?.as_str()?.to_owned(),
                        content: row.get("content")?.as_str()?.to_owned(),
                    })
                };
                parse().ok_or_else(|| unreadable(channel_id, "malformed history entry"))
            })
            .collect()
    }
}
