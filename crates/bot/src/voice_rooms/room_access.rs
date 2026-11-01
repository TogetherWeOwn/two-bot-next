//! `/room-access-inspect`: the owner's read-only view of who is blocked,
//! granted and waiting in their current room.
//!
//! The lists come straight from the worker's live privacy state, which loads
//! the durable block record and join grants. Nothing is reconstructed from
//! Discord overwrites, and nothing is written: pending requests are transient
//! and a restart legitimately empties them. The owner gate is the one every
//! owner command uses ([`GuildRoomWorker::owned_room`]), so only the caller's
//! own current room can be read.
//!
//! A reply is plain ephemeral content within Discord's 2000-unit bound. A long
//! list pages by entry across the three sections; every page carries all three
//! headings and each member appears on exactly one page, never clipped.

use super::*;

const COMMAND: &str = "room-access-inspect";
const REPLY_BOUND: usize = 2000;
/// Worst-case line for one member: `<@` + 20 digits + `>` + newline.
const ENTRY_UNITS: usize = 24;

type Members<'a> = [(&'a str, &'a [u64]); 3];

/// One page of the reply, or the real page count when `page` is past the end.
fn render_room_access(private: bool, lists: &Members<'_>, page: usize) -> Result<String, usize> {
    let total: usize = lists.iter().map(|(_, ids)| ids.len()).sum();
    if text_len(&compose(private, lists, 0, total, None)) <= REPLY_BOUND {
        return if page == 1 {
            Ok(compose(private, lists, 0, total, None))
        } else {
            Err(1)
        };
    }
    let wide = "9".repeat(total.to_string().len());
    let chrome = text_len(&compose(
        private,
        lists,
        0,
        0,
        Some((&wide, &wide, Some(&wide))),
    ));
    let per_page = (REPLY_BOUND.saturating_sub(chrome) / ENTRY_UNITS).max(1);
    let pages = total.div_ceil(per_page);
    if page > pages {
        return Err(pages);
    }
    let start = (page - 1) * per_page;
    let (here, last) = (page.to_string(), pages.to_string());
    let next = (page < pages).then(|| (page + 1).to_string());
    Ok(compose(
        private,
        lists,
        start,
        (start + per_page).min(total),
        Some((&here, &last, next.as_deref())),
    ))
}

/// The reply showing entries `start..end` of the three lists taken in order.
fn compose(
    private: bool,
    lists: &Members<'_>,
    start: usize,
    end: usize,
    footer: Option<(&str, &str, Option<&str>)>,
) -> String {
    let mut lines = vec![format!(
        "**Room access** — this room is {}.",
        if private { "private" } else { "public" }
    )];
    let mut offset = 0;
    for (title, ids) in lists {
        lines.push(format!("**{title} ({})**", ids.len()));
        let from = start.saturating_sub(offset).min(ids.len());
        let to = end.saturating_sub(offset).min(ids.len());
        if ids.is_empty() {
            lines.push("None.".to_owned());
        } else if from == to {
            lines.push("None on this page.".to_owned());
        } else {
            lines.extend(ids[from..to].iter().map(|id| format!("<@{id}>")));
        }
        offset += ids.len();
    }
    lines.push(
        "Note: join requests are held in memory and are cleared when the bot restarts.".to_owned(),
    );
    if let Some((page, pages, next)) = footer {
        lines.push(format!("Page {page} of {pages}"));
        if let Some(next) = next {
            lines.push(format!("Next page: /{COMMAND} page:{next}"));
        }
    }
    lines.join("\n")
}

impl<S: RoomPersistence, H: RoomWrites> GuildRoomWorker<S, H> {
    /// Answer `/room-access-inspect` from the live privacy state. Pure read:
    /// no state is created, refreshed or persisted, and no Discord write is
    /// queued. A room with no privacy record has three empty lists and reads
    /// as public unless Discord already denies @everyone Connect.
    pub(super) fn apply_room_access(
        &self,
        actor_id: Snowflake,
        is_admin: bool,
        page: PageArg,
    ) -> String {
        let (channel, _, _) = match self.owned_room(actor_id, is_admin, COMMAND) {
            Ok(found) => found,
            Err(refusal) => return refusal,
        };
        let Some(page) = page.number() else {
            return format!("Unknown page. Use /{COMMAND} page:1 to see the first page.");
        };
        let state = self.privacy.get(&channel);
        let blocked: Vec<u64> =
            state.map_or_else(Vec::new, |s| s.blocked.iter().map(|m| m.0).collect());
        let granted: Vec<u64> =
            state.map_or_else(Vec::new, |s| s.granted.iter().map(|m| m.0).collect());
        let pending: Vec<u64> =
            state.map_or_else(Vec::new, |s| s.pending.keys().map(|m| m.0).collect());
        let lists: Members<'_> = [
            ("Blocked", &blocked),
            ("Granted", &granted),
            ("Pending join requests", &pending),
        ];
        // A room an `/alwaysprivate` creator made denies Connect to @everyone
        // before any privacy record exists, so the flag alone cannot say the
        // room is open: an observed deny also reads as private (as `/public`).
        let private =
            state.is_some_and(|s| s.private) || self.live_everyone_denies_connect(channel);
        render_room_access(private, &lists, page).unwrap_or_else(|pages| {
            let noun = if pages == 1 { "page" } else { "pages" };
            format!(
                "Unknown page. /{COMMAND} has {pages} {noun}; use /{COMMAND} page:1 to start over."
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(count: u64, base: u64) -> Vec<u64> {
        (0..count).map(|n| base - n).collect()
    }

    fn lists<'a>(b: &'a [u64], g: &'a [u64], p: &'a [u64]) -> Members<'a> {
        [("Blocked", b), ("Granted", g), ("Pending join requests", p)]
    }

    #[test]
    fn empty_lists_say_so_on_one_page() {
        let text = render_room_access(false, &lists(&[], &[], &[]), 1).unwrap();
        assert_eq!(text.matches("None.").count(), 3);
        assert!(text.contains("**Blocked (0)**") && text.contains("public"));
        assert_eq!(render_room_access(false, &lists(&[], &[], &[]), 2), Err(1));
    }

    #[test]
    fn long_lists_page_without_loss_or_duplication() {
        let (b, g, p) = (
            ids(100, u64::MAX),
            ids(100, u64::MAX - 500),
            ids(100, u64::MAX - 900),
        );
        let l = lists(&b, &g, &p);
        let first = render_room_access(true, &l, 1).unwrap();
        let pages = first
            .rsplit_once("Page 1 of ")
            .and_then(|(_, tail)| tail.lines().next()?.parse::<usize>().ok())
            .expect("multi-page footer");
        for page in 1..=pages {
            let text = render_room_access(true, &l, page).unwrap();
            assert!(text_len(&text) <= REPLY_BOUND, "page {page} over bound");
            assert_eq!(
                text.matches("**").count(),
                8,
                "title and all three headings"
            );
            assert_eq!(text.contains("Next page:"), page < pages);
        }
        let shown: usize = (1..=pages)
            .map(|page| {
                render_room_access(true, &l, page)
                    .unwrap()
                    .matches("<@")
                    .count()
            })
            .sum();
        assert_eq!(shown, 300);
        assert_eq!(render_room_access(true, &l, pages + 1), Err(pages));
    }
}
