//! Early-message ladder scan.
//!
//! Ports the pure half of `src/backfill/messages.ts`. The scan keeps each
//! member's earliest THREE messages — the first separates "joined, status
//! unknown" from "joined and never said a word", the third is AM7's text
//! bar — across channels and out of order. Ordering is by timestamp with the
//! snowflake as tiebreak, so two messages in the same millisecond get a
//! stable order rather than depending on scan order.
//!
//! Truncation is safe in one direction: a capped scan sees a subset of a
//! member's messages, so the third-earliest found is at or LATER than the
//! true third — never earlier. A re-run only moves milestones towards the
//! truth.

use std::collections::HashMap;

/// Ladder depth: the AM7 text bar (legacy `LADDER = MESSAGE_RUNGS.length`).
pub const LADDER: usize = 3;

/// Channel types that can hold member conversation (legacy `TEXT_TYPES` +
/// `FORUM_TYPES` as Discord type codes).
pub const TEXT_CHANNEL_TYPES: [u8; 2] = [0, 5];
/// Forum channels: posts live in threads (legacy `FORUM_TYPES`).
pub const FORUM_CHANNEL_TYPES: [u8; 1] = [15];
/// Thread types (legacy `THREAD_TYPES`).
pub const THREAD_CHANNEL_TYPES: [u8; 3] = [10, 11, 12];

/// Bot-output channels are skipped as a cost decision, not a correctness
/// one: every message in them is bot-authored and would be discarded anyway,
/// and they are the highest-volume channels (legacy `LOGGY`).
fn is_loggy(name: &str) -> bool {
    let lower = name.to_lowercase();
    for needle in ["log", "wick", "audit", "modmail", "network-status"] {
        if lower.contains(needle) {
            return true;
        }
    }
    // `^\d+-[a-z]`: starts with digits, dash, then a lowercase letter.
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0
        && bytes.get(i) == Some(&b'-')
        && bytes.get(i + 1).is_some_and(|b| b.is_ascii_lowercase())
    {
        return true;
    }
    false
}

/// True when a channel holds member conversation rather than bot output
/// (legacy `isLogChannel`, inverted to a positive predicate).
#[must_use]
pub fn is_conversation_channel(name: &str, category_name: &str, channel_type: u8) -> bool {
    if channel_type == FORUM_CHANNEL_TYPES[0] {
        return !is_loggy(name) && !is_loggy(category_name);
    }
    if !TEXT_CHANNEL_TYPES.contains(&channel_type) {
        return false;
    }
    !is_loggy(name) && !is_loggy(category_name)
}

/// Back-compat alias matching the legacy name: true when the channel is bot
/// output to skip.
#[must_use]
pub fn is_log_channel(name: &str, category_name: &str) -> bool {
    is_loggy(name) || is_loggy(category_name)
}

/// One of a member's earliest messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlyMessage {
    /// Discord message snowflake: dedupes double-seen messages + tiebreak.
    pub id: String,
    pub at: String,
    pub channel_id: String,
}

/// A member's earliest messages, ascending, at most `LADDER` of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberMessages {
    pub member_id: String,
    pub rungs: Vec<EarlyMessage>,
}

/// Scan totals (legacy `MessageScanSummary`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageScanSummary {
    pub channels_considered: usize,
    pub channels_scanned: usize,
    pub threads_scanned: usize,
    pub messages_read: usize,
    pub authors_seen: usize,
    pub authors_with_full_ladder: usize,
    pub first_messages_written: usize,
    pub truncated: Vec<String>,
    pub scanned_back_to: Option<String>,
}

/// Keep `m` holding the earliest `LADDER` messages shown so far (legacy
/// `offer`). A message already on the ladder is the same message reaching us
/// twice (a forum post is both a thread and a channel), not a second one.
pub fn offer_message(m: &mut MemberMessages, msg: EarlyMessage) {
    if m.rungs.iter().any(|r| r.id == msg.id) {
        return;
    }
    let after = |a: &EarlyMessage, b: &EarlyMessage| a.at > b.at || (a.at == b.at && a.id > b.id);
    let mut i = m.rungs.len();
    while i > 0 && after(&m.rungs[i - 1], &msg) {
        i -= 1;
    }
    if i >= LADDER {
        return;
    }
    m.rungs.insert(i, msg);
    m.rungs.truncate(LADDER);
}

/// Fold one page of messages (newest-first or not — order does not matter)
/// into the ladder maps. Bots never reach the ladder. Returns message count.
pub fn fold_messages(
    early: &mut HashMap<String, MemberMessages>,
    last_active: &mut HashMap<String, String>,
    channel_id: &str,
    messages: &[ScannedMessage],
) -> usize {
    let mut n = 0;
    for msg in messages {
        n += 1;
        let Some(author) = msg.author_id.as_deref() else {
            continue;
        };
        if msg.author_is_bot {
            continue;
        }
        let prev = last_active.get(author);
        if prev.is_none_or(|p| msg.at > *p) {
            last_active.insert(author.to_owned(), msg.at.clone());
        }
        let m = early
            .entry(author.to_owned())
            .or_insert_with(|| MemberMessages {
                member_id: author.to_owned(),
                rungs: Vec::new(),
            });
        offer_message(
            m,
            EarlyMessage {
                id: msg.id.clone(),
                at: msg.at.clone(),
                channel_id: channel_id.to_owned(),
            },
        );
    }
    n
}

/// One scanned message: the fields the ladder reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedMessage {
    pub id: String,
    pub at: String,
    pub author_id: Option<String>,
    pub author_is_bot: bool,
}

/// Summarise a finished scan (legacy `findEarlyMessages` tail).
#[must_use]
pub fn find_early_messages(
    early: &HashMap<String, MemberMessages>,
    channels_considered: usize,
    channels_scanned: usize,
    threads_scanned: usize,
    messages_read: usize,
    truncated: Vec<String>,
    scanned_back_to: Option<String>,
) -> (HashMap<String, MemberMessages>, MessageScanSummary) {
    let authors_with_full_ladder = early.values().filter(|m| m.rungs.len() == LADDER).count();
    let summary = MessageScanSummary {
        channels_considered,
        channels_scanned,
        threads_scanned,
        messages_read,
        authors_seen: early.len(),
        authors_with_full_ladder,
        first_messages_written: 0,
        truncated,
        scanned_back_to,
    };
    (early.clone(), summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(id: &str, author: &str, at: &str) -> ScannedMessage {
        ScannedMessage {
            id: id.to_owned(),
            at: at.to_owned(),
            author_id: Some(author.to_owned()),
            author_is_bot: false,
        }
    }

    fn scan(
        pages: &[(&str, Vec<ScannedMessage>)],
    ) -> (HashMap<String, MemberMessages>, MessageScanSummary) {
        let mut early = HashMap::new();
        let mut last_active = HashMap::new();
        let mut read = 0;
        for (ch, msgs) in pages {
            read += fold_messages(&mut early, &mut last_active, ch, msgs);
        }
        find_early_messages(&early, pages.len(), pages.len(), 0, read, Vec::new(), None)
    }

    #[test]
    fn keeps_earliest_three_across_channels_out_of_order() {
        let (early, _) = scan(&[
            (
                "c1",
                vec![
                    post("5", "alice", "2026-03-05T00:00:00.000Z"),
                    post("1", "alice", "2026-03-01T00:00:00.000Z"),
                    post("4", "alice", "2026-03-04T00:00:00.000Z"),
                ],
            ),
            (
                "c2",
                vec![
                    post("3", "alice", "2026-03-03T00:00:00.000Z"),
                    post("2", "alice", "2026-03-02T00:00:00.000Z"),
                ],
            ),
        ]);
        assert_eq!(
            early["alice"]
                .rungs
                .iter()
                .map(|r| r.at.as_str())
                .collect::<Vec<_>>(),
            vec![
                "2026-03-01T00:00:00.000Z",
                "2026-03-02T00:00:00.000Z",
                "2026-03-03T00:00:00.000Z"
            ]
        );
    }

    #[test]
    fn two_posts_is_two_rungs_no_third_invented() {
        let (early, summary) = scan(&[(
            "c1",
            vec![
                post("1", "bob", "2026-03-01T00:00:00.000Z"),
                post("2", "bob", "2026-03-02T00:00:00.000Z"),
            ],
        )]);
        assert_eq!(early["bob"].rungs.len(), 2);
        assert_eq!(summary.authors_seen, 1);
        assert_eq!(summary.authors_with_full_ladder, 0);
    }

    #[test]
    fn message_seen_twice_is_one_rung() {
        let dupe = post("1", "carol", "2026-03-01T00:00:00.000Z");
        let (early, _) = scan(&[
            ("c1", vec![dupe.clone()]),
            (
                "c2",
                vec![dupe, post("2", "carol", "2026-03-02T00:00:00.000Z")],
            ),
        ]);
        assert_eq!(
            early["carol"]
                .rungs
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2"]
        );
    }

    #[test]
    fn bot_posts_never_reach_ladder() {
        let (early, _) = scan(&[(
            "c1",
            vec![ScannedMessage {
                id: "1".to_owned(),
                at: "2026-03-01T00:00:00.000Z".to_owned(),
                author_id: Some("botly".to_owned()),
                author_is_bot: true,
            }],
        )]);
        assert!(early.is_empty());
    }

    #[test]
    fn log_channels_detected() {
        assert!(is_log_channel("audit-log", "Mods"));
        assert!(is_log_channel("general", "wick-logs"));
        assert!(is_log_channel("3-general", "Community"));
        assert!(!is_log_channel("general", "Community"));
        assert!(is_conversation_channel("general", "Community", 0));
        assert!(!is_conversation_channel("audit-log", "Community", 0));
        assert!(!is_conversation_channel("general", "Community", 2));
        assert!(is_conversation_channel("introductions", "Community", 0));
        assert!(is_conversation_channel("forum-posts", "Community", 15));
    }
}
