//! AFK status with mention replies.
//!
//! `/afk [message]` records one row per member in `afk_status`
//! (migration 0458); a later message that mentions an AFK member answers
//! once with the note plus elapsed time, and the member's own next message
//! clears the status. This module owns the pure contract (note validation,
//! elapsed rendering, reply texts) and the store; execution lives in the
//! bot crate's `CommandRuntime` (`afk` routes to [`crate::HandlerId::Afk`]).
//!
//! Anti-abuse bounds: notes are capped at [`AFK_NOTE_MAX_CHARS`]
//! characters and must carry no mention syntax (`@everyone`, `@here`,
//! `<@…>`), and each guild holds at most [`AFK_GUILD_ROW_CAP`] rows.
//! Mention replies are additionally rate-limited per
//! (AFK member, mentioner) pair by the bot crate
//! ([`AFK_MENTION_COOLDOWN_SECS`]). Nothing here logs note contents.

/// Longest AFK note, in characters. The publish definition declares the
/// same bound via `max_length`, and the handler re-checks it: Discord
/// picker hiding is not authorization.
pub const AFK_NOTE_MAX_CHARS: usize = 200;

/// Most AFK rows one guild may hold. Past the cap, new setters are refused
/// while updates to existing rows still succeed (an update cannot grow the
/// table).
pub const AFK_GUILD_ROW_CAP: i64 = 100;

/// Mention-reply cooldown per (AFK member, mentioner) pair, in seconds.
/// An immediate repeat by the same member is suppressed while a different
/// member still answers.
pub const AFK_MENTION_COOLDOWN_SECS: u64 = 300;

/// Why an AFK note was refused. Refusal texts are owned (never the
/// unknown-command literal), name the bound, and never echo the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfkNoteError {
    /// Over [`AFK_NOTE_MAX_CHARS`] characters.
    TooLong { chars: usize },
    /// Carries `@everyone`, `@here` or `<@…>` mention syntax.
    Mentions,
}

impl std::fmt::Display for AfkNoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Names the length bound (`too long`) for the over-cap case.
            Self::TooLong { chars } => write!(
                f,
                "That AFK note is too long ({chars}/{AFK_NOTE_MAX_CHARS} characters)."
            ),
            Self::Mentions => write!(f, "AFK notes can't contain mentions."),
        }
    }
}

impl std::error::Error for AfkNoteError {}

/// A decoded `afk_status` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfkStatus {
    pub guild_id: String,
    pub user_id: String,
    /// The setter's note, if any. Stored notes never contain mention
    /// syntax: hostile notes are refused before the write.
    pub note: Option<String>,
    /// When the status was set, epoch millis.
    pub set_at_ms: i64,
}

/// Refusal when the guild already holds [`AFK_GUILD_ROW_CAP`] rows.
pub const AFK_GUILD_CAP_REPLY: &str =
    "Too many members are AFK in this server right now; try again soon.";

/// Quiet auto-clear confirmation: names the cleared state, pings nobody.
pub const AFK_CLEARED_REPLY: &str = "Welcome back — your AFK status has been cleared.";

/// Handler-side fallback when the store is unreachable. Generic on
/// purpose: no note content, no member detail.
pub const AFK_STORE_UNAVAILABLE_REPLY: &str = "Couldn't update your AFK status; try again shortly.";

/// Validate one AFK note: bounded length, no mention syntax.
pub fn validate_note(note: &str) -> Result<(), AfkNoteError> {
    let chars = note.chars().count();
    if chars > AFK_NOTE_MAX_CHARS {
        return Err(AfkNoteError::TooLong { chars });
    }
    if note.contains("@everyone") || note.contains("@here") || note.contains("<@") {
        return Err(AfkNoteError::Mentions);
    }
    Ok(())
}

/// Render the elapsed fragment for a mention reply: `just now` under five
/// seconds, else `<n> <unit> ago` in seconds, minutes or hours.
#[must_use]
pub fn render_elapsed(set_at_ms: i64, now_ms: i64) -> String {
    fn ago(n: i64, unit: &str) -> String {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    }
    let secs = now_ms.saturating_sub(set_at_ms).max(0) / 1000;
    if secs < 5 {
        return "just now".to_owned();
    }
    if secs < 60 {
        return ago(secs, "second");
    }
    let mins = secs / 60;
    if mins < 60 {
        return ago(mins, "minute");
    }
    ago(mins / 60, "hour")
}

/// Set/update confirmation. Names the AFK state and echoes a supplied note
/// verbatim; set and update wordings differ.
#[must_use]
pub fn set_confirmation(note: Option<&str>, updated: bool) -> String {
    match (note, updated) {
        (Some(note), false) => format!("You're now AFK: {note}"),
        (Some(note), true) => format!("Your AFK status is updated: {note}"),
        (None, false) => "You're now AFK.".to_owned(),
        (None, true) => "Your AFK status is updated; no note set.".to_owned(),
    }
}

/// One mention auto-reply: the AFK member's name (never a re-ping), the
/// state name, the elapsed fragment and the note verbatim.
#[must_use]
pub fn mention_reply(display_name: &str, note: Option<&str>, elapsed: &str) -> String {
    match note {
        Some(note) => format!("{display_name} is AFK ({elapsed}): {note}"),
        None => format!("{display_name} is AFK ({elapsed})."),
    }
}

/// `afk_status` store (migration 0458). Timestamps cross the boundary as
/// millisecond Unix times (`i64`); SQL converts via
/// `to_timestamp(ms / 1000.0)` and reads back via
/// `(extract(epoch from …) * 1000)::bigint`, like the sticky store.
#[cfg(feature = "db")]
pub mod store {
    use super::AfkStatus;
    use sqlx::{Pool, Postgres};

    /// One decoded `afk_status` row (column order of the selects below).
    type AfkRow = (String, String, Option<String>, i64);

    fn decode((guild_id, user_id, note, set_at_ms): AfkRow) -> AfkStatus {
        AfkStatus {
            guild_id,
            user_id,
            note,
            set_at_ms,
        }
    }

    /// Fetch the AFK statuses for a message's mentioned members in one
    /// round trip. An empty id list reads nothing.
    pub async fn get_afk_for(
        pool: &Pool<Postgres>,
        guild_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<AfkStatus>, sqlx::Error> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<AfkRow> = sqlx::query_as(
            "SELECT guild_id, user_id, note,
                    (extract(epoch from set_at) * 1000)::BIGINT AS set_at_ms
               FROM afk_status WHERE guild_id = $1 AND user_id = ANY($2)",
        )
        .bind(guild_id)
        .bind(user_ids)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(decode).collect())
    }

    /// Set or replace one member's AFK status. Serialize setters per guild
    /// across pools/processes, then count and write in a fresh READ COMMITTED
    /// snapshot: the count must run after the previous lock holder commits.
    /// Existing members may update even at `row_cap`; new members return
    /// `None` at the cap. Otherwise return whether the row was created
    /// (`xmax = 0` distinguishes insert from update atomically).
    pub async fn set_afk(
        pool: &Pool<Postgres>,
        guild_id: &str,
        user_id: &str,
        note: Option<&str>,
        now_ms: i64,
        row_cap: i64,
    ) -> Result<Option<bool>, sqlx::Error> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("afk-status-cap:{guild_id}"))
            .execute(&mut *tx)
            .await?;
        let created: Option<(bool,)> = sqlx::query_as(
            "INSERT INTO afk_status (guild_id, user_id, note, set_at)
             SELECT $1, $2, $3, to_timestamp($4::DOUBLE PRECISION / 1000.0)
              WHERE (SELECT COUNT(*) FROM afk_status WHERE guild_id = $1) < $5
                 OR EXISTS (SELECT 1 FROM afk_status WHERE guild_id = $1 AND user_id = $2)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET
               note   = excluded.note,
               set_at = excluded.set_at
             RETURNING (xmax = 0) AS created",
        )
        .bind(guild_id)
        .bind(user_id)
        .bind(note)
        .bind(now_ms)
        .bind(row_cap)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(created.map(|(created,)| created))
    }

    /// Clear one member's AFK status. Returns whether a row was removed.
    pub async fn clear_afk(
        pool: &Pool<Postgres>,
        guild_id: &str,
        user_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM afk_status WHERE guild_id = $1 AND user_id = $2")
            .bind(guild_id)
            .bind(user_id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_bounded_and_mention_free() {
        assert!(validate_note("grabbing lunch back soon").is_ok());
        assert!(validate_note("").is_ok());
        assert!(validate_note(&"x".repeat(AFK_NOTE_MAX_CHARS)).is_ok());
        assert_eq!(
            validate_note(&"x".repeat(AFK_NOTE_MAX_CHARS + 1)),
            Err(AfkNoteError::TooLong {
                chars: AFK_NOTE_MAX_CHARS + 1
            })
        );
        assert_eq!(
            validate_note(&"x".repeat(2000)),
            Err(AfkNoteError::TooLong { chars: 2000 })
        );
        for hostile in [
            "ping @everyone at dawn",
            "alert @here now",
            "see <@99> for details",
            "see <@!99> for details",
            "ping <@&7> now",
        ] {
            assert_eq!(
                validate_note(hostile),
                Err(AfkNoteError::Mentions),
                "{hostile:?}"
            );
        }
        // Near-misses stay settable: the bound is mention syntax, not
        // the words alone.
        assert!(validate_note("everyone is welcome").is_ok());
        assert!(validate_note("meet here at noon").is_ok());
    }

    #[test]
    fn refusal_texts_are_owned_and_bound_naming() {
        let too_long = AfkNoteError::TooLong { chars: 2000 }.to_string();
        assert!(too_long.to_lowercase().contains("too long"), "{too_long:?}");
        assert!(!too_long.contains("<@"));
        assert_eq!(
            AfkNoteError::Mentions.to_string(),
            "AFK notes can't contain mentions."
        );
        assert!(!AFK_GUILD_CAP_REPLY.contains("<@"));
        assert!(!AFK_CLEARED_REPLY.contains("<@"));
        assert!(AFK_CLEARED_REPLY.to_lowercase().contains("afk"));
    }

    #[test]
    fn elapsed_renders_just_now_then_units() {
        assert_eq!(render_elapsed(1_000, 1_000), "just now");
        assert_eq!(render_elapsed(0, 1_100), "just now");
        assert_eq!(render_elapsed(0, 4_999), "just now");
        assert_eq!(render_elapsed(0, 5_000), "5 seconds ago");
        assert_eq!(render_elapsed(0, 61_000), "1 minute ago");
        assert_eq!(render_elapsed(0, 120_000), "2 minutes ago");
        assert_eq!(render_elapsed(0, 3_600_000), "1 hour ago");
        assert_eq!(render_elapsed(0, 7_200_000), "2 hours ago");
        // Clock skew never renders a negative duration.
        assert_eq!(render_elapsed(9_999, 1_000), "just now");
    }

    #[test]
    fn confirmations_name_the_state_and_echo_the_note() {
        let set = set_confirmation(Some("grabbing lunch"), false);
        assert!(set.contains("grabbing lunch"));
        assert!(set.to_lowercase().contains("afk"));
        let updated = set_confirmation(Some("in a meeting"), true);
        assert!(updated.contains("in a meeting"));
        assert_ne!(set, updated);
        assert!(set_confirmation(None, false).to_lowercase().contains("afk"));
        assert!(set_confirmation(None, true).to_lowercase().contains("afk"));
    }

    #[test]
    fn mention_replies_carry_name_state_elapsed_and_note() {
        let reply = mention_reply("member", Some("grabbing lunch"), "5 seconds ago");
        assert!(reply.contains("member"));
        assert!(reply.contains("grabbing lunch"));
        assert!(reply.contains("5 seconds ago"));
        assert!(reply.to_lowercase().contains("afk"));
        assert!(!reply.contains("<@"));
        let noteless = mention_reply("member", None, "just now");
        assert!(noteless.to_lowercase().contains("afk"));
        assert!(noteless.contains("just now"));
    }
}
