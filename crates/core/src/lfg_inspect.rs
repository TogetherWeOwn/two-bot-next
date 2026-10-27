//! Private, read-only inspection of one saved LFG post: deterministic whole-row
//! pages over every configured role and saved signup, without the public
//! post's 2000-unit truncation ever hiding a later role.
//!
//! Pure projection only. The Discord adapter authorizes guild, invoking
//! channel and Manage Events before any roster read, then hands the saved
//! rows here. A stored message ID is never rendered as delivery evidence.

use crate::{
    lfg::{LfgPost, LfgRole, LfgSignup},
    lfg_list::{inert_title, quoted_id},
    message_safety,
};

/// Largest accepted `page` option (1-based); role selection is bounded by
/// [`crate::lfg::MAX_LFG_ROLES`].
pub const MAX_PAGE: usize = 100;

/// Saved post exists in this guild but not for this channel, or not at all:
/// one indistinguishable private miss for unknown, other-guild and
/// other-channel IDs.
pub const MISS: &str =
    "No saved LFG post with that ID was found in this channel. Check the ID, then run /lfg-inspect again.";
/// The store could not be read; never dressed up as empty or missing.
pub const UNAVAILABLE: &str =
    "Unable to read the saved LFG post right now. Try /lfg-inspect again in a moment.";
/// Malformed, duplicated or unbounded options; refused before storage.
pub const INVALID: &str =
    "Invalid inspection. Use /lfg-inspect id:<saved ID> [role:<1-20>] [page:<1-100>].";
/// The selected 1-based role is beyond the roles saved for this post.
pub const ROLE_OUT_OF_RANGE: &str = "That role is out of range for this saved post. Run /lfg-inspect id:<saved ID> to see its saved roles.";
/// The selected page is beyond the last page of whole rows.
pub const PAGE_OUT_OF_RANGE: &str =
    "That page is out of range — no more saved rows. Run /lfg-inspect id:<saved ID> page:1 to start again.";

/// One validated inspection request (strict decode lives in the adapter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectSelection {
    /// 1-based role index in position/key order; `None` shows role summaries.
    pub role: Option<usize>,
    /// 1-based page, already bounded to 1..=100 by the adapter.
    pub page: usize,
}

fn status_word(post: &LfgPost) -> &'static str {
    match post.status {
        crate::lfg::LfgStatus::Open => "open",
        crate::lfg::LfgStatus::Closed => "closed",
    }
}

/// Saved-state header: stable ID, inert saved title, saved start and state.
/// Never a jump link: a stored message ID does not prove live delivery.
fn header(post: &LfgPost) -> String {
    use time::format_description::well_known::Rfc3339;
    let start = time::OffsetDateTime::parse(&post.starts_at, &Rfc3339)
        .map(|dt| format!("<t:{}:f>", dt.unix_timestamp()))
        .unwrap_or_else(|_| "unknown".to_owned());
    format!(
        "Saved LFG {} — {}\nStarts: {start} — saved state: {}",
        quoted_id(&post.id),
        inert_title(&post.title),
        status_word(post),
    )
}

/// Roles in the contract order: saved position, then role key. The store
/// already orders by position; sorting here keeps the projection honest
/// even if a caller hands over unordered rows.
fn ordered_roles(roles: &[LfgRole]) -> Vec<&LfgRole> {
    let mut ordered: Vec<&LfgRole> = roles.iter().collect();
    ordered.sort_by(|a, b| (a.position, &a.role_key).cmp(&(b.position, &b.role_key)));
    ordered
}

fn filled(role: &LfgRole, signups: &[LfgSignup]) -> usize {
    signups
        .iter()
        .filter(|signup| signup.role_key == role.role_key)
        .count()
}

fn footer(post: &LfgPost, selection: &InspectSelection, page: usize, total: usize) -> String {
    let mut text = format!("Page {page} of {total}.");
    if page < total {
        text.push_str(&format!(" Continue: /lfg-inspect id:{}", post.id));
        if let Some(role) = selection.role {
            text.push_str(&format!(" role:{role}"));
        }
        text.push_str(&format!(" page:{}", page + 1));
    }
    text
}

/// Fold whole rows into bounded pages under the saved-state header. Rows are
/// never cut: a row that cannot fit with header and footer fails explicitly
/// as [`UNAVAILABLE`] instead of silently truncating saved text.
fn render_lines(
    post: &LfgPost,
    selection: &InspectSelection,
    intro: Option<&str>,
    rows: &[String],
) -> Result<String, &'static str> {
    let header = message_safety::neutralize_mentions(&header(post));
    let intro = intro.map(message_safety::neutralize_mentions);
    let rows: Vec<String> = rows
        .iter()
        .map(|row| message_safety::neutralize_mentions(row))
        .collect();
    // Reserve the widest footer so chunking never depends on the total it is
    // still computing: the widest continuation footer (page 99 of 100,
    // continuing to page 100) or the widest final footer (page 100 of 100).
    let reserve = [
        footer(post, selection, 99, 100),
        footer(post, selection, 100, 100),
    ]
    .iter()
    .map(|text| message_safety::text_len(&message_safety::neutralize_mentions(text)))
    .max()
    .unwrap_or(0)
        + 1;
    let fixed = message_safety::text_len(&header)
        + intro
            .as_ref()
            .map_or(0, |line| message_safety::text_len(line) + 1)
        + 1
        + reserve;
    if fixed > message_safety::CONTENT_LIMIT {
        return Err(UNAVAILABLE);
    }
    let budget = message_safety::CONTENT_LIMIT - fixed;
    let mut pages: Vec<Vec<&String>> = Vec::new();
    let mut current: Vec<&String> = Vec::new();
    let mut used = 0_usize;
    for row in &rows {
        let len = message_safety::text_len(row) + 1;
        if len > budget {
            return Err(UNAVAILABLE);
        }
        if used + len > budget {
            pages.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(row);
        used += len;
    }
    if !current.is_empty() || pages.is_empty() {
        pages.push(current);
    }
    let total = pages.len();
    if selection.page == 0 || selection.page > total {
        return Ok(PAGE_OUT_OF_RANGE.to_owned());
    }
    let mut content = header;
    if let Some(intro) = &intro {
        content.push('\n');
        content.push_str(intro);
    }
    for row in &pages[selection.page - 1] {
        content.push('\n');
        content.push_str(row);
    }
    content.push('\n');
    content.push_str(&message_safety::neutralize_mentions(&footer(
        post,
        selection,
        selection.page,
        total,
    )));
    Ok(content)
}

/// Role-summary view: every configured role exactly once across pages, each
/// whole row carrying filled/total and remaining seats. No member IDs.
pub fn render_summaries(
    post: &LfgPost,
    roles: &[LfgRole],
    signups: &[LfgSignup],
    page: usize,
) -> Result<String, &'static str> {
    let selection = InspectSelection { role: None, page };
    let ordered = ordered_roles(roles);
    if ordered.is_empty() {
        if page > 1 {
            return Ok(PAGE_OUT_OF_RANGE.to_owned());
        }
        return render_lines(
            post,
            &selection,
            Some("No roles are saved for this post."),
            &[],
        );
    }
    let rows: Vec<String> = ordered
        .iter()
        .enumerate()
        .map(|(index, role)| {
            let filled = filled(role, signups);
            let remaining = usize::from(role.slots).saturating_sub(filled);
            format!(
                "- Role {}: {} — {filled}/{} — {remaining} remaining",
                index + 1,
                inert_title(&role.label),
                role.slots
            )
        })
        .collect();
    render_lines(post, &selection, None, &rows)
}

/// Selected-role roster: every saved signup for that one role exactly once,
/// in joined_at then user-ID order (the store's read order, re-sorted here
/// so the projection does not depend on caller ordering).
pub fn render_roster(
    post: &LfgPost,
    roles: &[LfgRole],
    signups: &[LfgSignup],
    role_index: usize,
    page: usize,
) -> Result<String, &'static str> {
    let selection = InspectSelection {
        role: Some(role_index),
        page,
    };
    let ordered = ordered_roles(roles);
    let Some(role) = role_index
        .checked_sub(1)
        .and_then(|index| ordered.get(index))
    else {
        return Ok(ROLE_OUT_OF_RANGE.to_owned());
    };
    let intro = format!("Role {}: {}", role_index, inert_title(&role.label));
    let mut members: Vec<&LfgSignup> = signups
        .iter()
        .filter(|signup| signup.role_key == role.role_key)
        .collect();
    members.sort_by(|a, b| (&a.joined_at, &a.user_id).cmp(&(&b.joined_at, &b.user_id)));
    if members.is_empty() {
        if page > 1 {
            return Ok(PAGE_OUT_OF_RANGE.to_owned());
        }
        return render_lines(
            post,
            &selection,
            Some(&format!("{intro} — No saved signups for this role yet.")),
            &[],
        );
    }
    let rows: Vec<String> = members
        .iter()
        .map(|signup| format!("- {}", inert_title(&signup.user_id)))
        .collect();
    render_lines(post, &selection, Some(&intro), &rows)
}

/// Full projection dispatch once authorization and roster reads are done.
pub fn render(
    post: &LfgPost,
    roles: &[LfgRole],
    signups: &[LfgSignup],
    selection: &InspectSelection,
) -> String {
    let rendered = match selection.role {
        Some(role) => render_roster(post, roles, signups, role, selection.page),
        None => render_summaries(post, roles, signups, selection.page),
    };
    rendered.unwrap_or_else(|unavailable| unavailable.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lfg::LfgStatus;

    fn post() -> LfgPost {
        LfgPost {
            id: "post-1".into(),
            guild_id: "2".into(),
            channel_id: "4".into(),
            message_id: Some("8000".into()),
            title: "Raid night".into(),
            starts_at: "2099-01-02T03:04:05.000Z".into(),
            status: LfgStatus::Open,
            created_by: "9".into(),
            created_at: "2020-01-01T00:00:00.000Z".into(),
            closed_at: None,
        }
    }

    fn role(key: &str, position: usize) -> LfgRole {
        LfgRole {
            lfg_id: "post-1".into(),
            role_key: key.into(),
            label: format!("Label {key}"),
            slots: 3,
            position,
        }
    }

    fn signup(user: &str, key: &str, joined: &str) -> LfgSignup {
        LfgSignup {
            lfg_id: "post-1".into(),
            user_id: user.into(),
            role_key: key.into(),
            joined_at: joined.into(),
        }
    }

    #[test]
    fn summaries_order_by_position_then_key_with_counts() {
        let roles = [role("b", 0), role("a", 0), role("c", 1)];
        let signups = [signup("111", "a", "2020-01-01T00:00:00.000Z")];
        let text = render_summaries(&post(), &roles, &signups, 1).expect("render");
        let labels: Vec<&str> = text.lines().filter(|line| line.starts_with("- ")).collect();
        assert_eq!(
            labels,
            [
                "- Role 1: Label a — 1/3 — 2 remaining",
                "- Role 2: Label b — 0/3 — 3 remaining",
                "- Role 3: Label c — 0/3 — 3 remaining"
            ]
        );
        assert!(text.contains("<t:4071006245:f>"));
        assert!(!text.contains("111"), "summaries disclose no members");
        assert!(!text.contains("discord.com/channels"));
    }

    #[test]
    fn duplicate_labels_keep_global_selectors_across_summary_pages() {
        let mut roles: Vec<LfgRole> = (0..20)
            .rev()
            .map(|position| role(&format!("r{position:02}"), position))
            .collect();
        for role in &mut roles {
            role.label = "😀".repeat(80);
        }
        let signups = [signup("111", "r19", "2020-01-01T00:00:00.000Z")];
        let mut selectors = Vec::new();
        let mut pages = 0;
        for page in 1..=MAX_PAGE {
            let text = render_summaries(&post(), &roles, &signups, page).expect("render");
            if text == PAGE_OUT_OF_RANGE {
                break;
            }
            pages += 1;
            assert!(message_safety::text_len(&text) <= message_safety::CONTENT_LIMIT);
            for row in text.lines().filter(|line| line.starts_with("- ")) {
                let selector = row
                    .strip_prefix("- Role ")
                    .and_then(|row| row.split_once(':'))
                    .and_then(|(number, _)| number.parse::<usize>().ok())
                    .expect("each summary exposes its 1-based role selector");
                selectors.push(selector);
            }
        }
        assert!(
            pages > 1,
            "exercise selector numbering across a page boundary"
        );
        assert_eq!(selectors, (1..=20).collect::<Vec<_>>());
        let roster = render_roster(&post(), &roles, &signups, 20, 1).expect("roster");
        assert!(roster.contains("Role 20:"));
        assert!(roster.lines().any(|line| line == "- 111"));
        let empty = render_roster(&post(), &roles, &signups, 1, 1).expect("empty role");
        assert!(empty.contains("No saved signups"));
    }

    #[test]
    fn roster_pages_members_once_in_join_order() {
        let roles = [role("tank", 0)];
        let signups = [
            signup("222", "tank", "2020-01-01T00:00:00.000Z"),
            signup("111", "tank", "2020-01-01T00:00:00.000Z"),
            signup("333", "tank", "2019-01-01T00:00:00.000Z"),
        ];
        let text = render_roster(&post(), &roles, &signups, 1, 1).expect("render");
        let members: Vec<&str> = text.lines().filter(|line| line.starts_with("- ")).collect();
        assert_eq!(members, ["- 333", "- 111", "- 222"]);
        let end = render_roster(&post(), &roles, &signups, 1, 2).expect("render");
        assert_eq!(end, PAGE_OUT_OF_RANGE);
        let absent = render_roster(&post(), &roles, &signups, 2, 1).expect("render");
        assert_eq!(absent, ROLE_OUT_OF_RANGE);
    }

    #[test]
    fn every_page_stays_bounded_and_covers_each_member_once() {
        let roles = [role("tank", 0)];
        let signups: Vec<LfgSignup> = (1..=99_usize)
            .map(|n| {
                signup(
                    &format!("70000000000000{n:03}"),
                    "tank",
                    "2020-01-01T00:00:00.000Z",
                )
            })
            .collect();
        let mut seen = Vec::new();
        for page in 1..=100 {
            let text = render_roster(&post(), &roles, &signups, 1, page).expect("render");
            assert!(
                message_safety::text_len(&text) <= message_safety::CONTENT_LIMIT,
                "page {page} within the UTF-16 ceiling"
            );
            let rows: Vec<&str> = text.lines().filter(|line| line.starts_with("- ")).collect();
            if rows.is_empty() {
                assert_eq!(text, PAGE_OUT_OF_RANGE);
                break;
            }
            seen.extend(rows.into_iter().map(str::to_owned));
        }
        assert_eq!(seen.len(), 99, "each saved signup exactly once");
    }

    #[test]
    fn empty_role_is_honest_and_bounded() {
        let roles = [role("tank", 0)];
        let text = render_roster(&post(), &roles, &[], 1, 1).expect("render");
        assert!(text.contains("No saved signups"));
        assert!(message_safety::text_len(&text) <= message_safety::CONTENT_LIMIT);
        let later = render_roster(&post(), &roles, &[], 1, 2).expect("render");
        assert_eq!(later, PAGE_OUT_OF_RANGE);
    }
}
