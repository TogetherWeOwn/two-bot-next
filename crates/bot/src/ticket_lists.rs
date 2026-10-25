//! Pure rendering and option parsing for the private ticket-list commands.
//!
//! These views deliberately consume stored ticket state only. Only an `Open`
//! lifecycle row may render a valid stored channel identifier; rendering never
//! probes Discord, claims readiness, or schedules ticket recovery.

use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction,
};
use two_bot_core::{
    message_safety::{neutralize_mentions, text_len, truncate, CONTENT_LIMIT},
    tickets::{Ticket, TicketStatus},
};

const TICKET_QUEUE_PAGE_SIZE: usize = 5;
const DISPLAY_VALUE_LIMIT: usize = 128;

pub(crate) const TICKET_QUEUE_PAGE_INVALID: &str =
    "Invalid ticket queue page. Use /ticket-queue page:1 or a higher page.";
pub(crate) const TICKET_QUEUE_PAGE_OUT_OF_RANGE: &str =
    "That ticket queue page is unavailable. Start at page 1.";
pub(crate) const TICKET_LIST_STORE_FAILURE: &str =
    "Tickets are temporarily unavailable. Please try again.";
pub(crate) const TICKET_QUEUE_STAFF_ONLY: &str = "Only ticket staff can view the ticket queue.";
pub(crate) const TICKET_LIST_UNAVAILABLE: &str = "Tickets are not available in this build yet.";

/// Strict outcome for `/ticket-queue`'s optional 1-based page option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TicketQueuePage {
    Default,
    Page(i64),
    Invalid,
}

/// Decode the queue page without echoing untrusted wire data. A duplicate or
/// wrong-type `page` is a refusal rather than a fallback to page one.
pub(crate) fn ticket_queue_page_request(interaction: &Interaction) -> TicketQueuePage {
    let Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)) =
        &interaction.data
    else {
        return TicketQueuePage::Invalid;
    };
    let pages: Vec<&CommandOptionValue> = data
        .options
        .iter()
        .filter(|option| option.name == "page")
        .map(|option| &option.value)
        .collect();
    match pages.as_slice() {
        [] => TicketQueuePage::Default,
        [CommandOptionValue::Integer(page)] if page_is_representable(*page) => {
            TicketQueuePage::Page(*page)
        }
        _ => TicketQueuePage::Invalid,
    }
}

/// Render one member's active tickets. Storage applies the opener and guild
/// fences; this renderer filters terminal rows too so an unexpected caller can
/// never re-expose a completed ticket.
pub(crate) fn render_my_tickets(tickets: &[Ticket]) -> String {
    let active: Vec<&Ticket> = tickets
        .iter()
        .filter(|ticket| ticket.status != TicketStatus::Closed)
        .collect();
    if active.is_empty() {
        return "You have no open tickets.".to_owned();
    }

    let mut output = String::from("Your active tickets:\n");
    let mut shown = 0;
    for ticket in &active {
        let row = member_row(ticket);
        let omitted = active.len().saturating_sub(shown + 1);
        let suffix = if omitted == 0 {
            String::new()
        } else {
            format!(
                "Showing the first {} of {} active tickets.",
                shown + 1,
                active.len()
            )
        };
        if text_len(&output) + text_len(&row) + text_len(&suffix) > CONTENT_LIMIT {
            break;
        }
        output.push_str(&row);
        shown += 1;
    }
    if shown < active.len() {
        output.push_str(&format!(
            "Showing the first {shown} of {} active tickets.",
            active.len()
        ));
    }
    truncate(&output, CONTENT_LIMIT)
}

/// Render one bounded, 1-based staff queue page. The store orders active rows
/// by `(created_at, id)` before this function sees them.
pub(crate) fn render_ticket_queue(tickets: &[Ticket], page: i64) -> Result<String, &'static str> {
    let active: Vec<&Ticket> = tickets
        .iter()
        .filter(|ticket| ticket.status != TicketStatus::Closed)
        .collect();
    let total_pages = active.len().div_ceil(TICKET_QUEUE_PAGE_SIZE).max(1);
    let page_index = page
        .checked_sub(1)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|index| *index < total_pages)
        .ok_or(TICKET_QUEUE_PAGE_OUT_OF_RANGE)?;
    let start = page_index
        .checked_mul(TICKET_QUEUE_PAGE_SIZE)
        .ok_or(TICKET_QUEUE_PAGE_OUT_OF_RANGE)?;
    let end = start
        .saturating_add(TICKET_QUEUE_PAGE_SIZE)
        .min(active.len());

    let mut output = format!("Ticket queue\nPage {page} of {total_pages}\n");
    if active.is_empty() {
        output.push_str("No active tickets are waiting in the queue.");
    } else {
        for ticket in &active[start..end] {
            output.push_str(&queue_row(ticket));
        }
    }
    Ok(truncate(&output, CONTENT_LIMIT))
}

fn page_is_representable(page: i64) -> bool {
    page > 0
        && usize::try_from(page)
            .ok()
            .is_some_and(|page| page <= usize::MAX / TICKET_QUEUE_PAGE_SIZE)
}

fn member_row(ticket: &Ticket) -> String {
    let mut row = format!(
        "- Ticket {} | status: {} | claim: {}",
        display_value(&ticket.id),
        status_text(ticket.status),
        claim_text(ticket)
    );
    if let Some(channel) = usable_channel(ticket) {
        row.push_str(" | channel: <#");
        row.push_str(channel);
        row.push('>');
    }
    row.push('\n');
    row
}

fn queue_row(ticket: &Ticket) -> String {
    let mut row = format!(
        "- Ticket {} | opener: {} | status: {} | created: <t:{}:F> | claim: {}",
        display_value(&ticket.id),
        display_value(&ticket.opener_id),
        status_text(ticket.status),
        ticket.created_at.div_euclid(1_000).max(0),
        claim_text(ticket),
    );
    if let Some(channel) = usable_channel(ticket) {
        row.push_str(" | channel: <#");
        row.push_str(channel);
        row.push('>');
    }
    row.push('\n');
    row
}

fn claim_text(ticket: &Ticket) -> &'static str {
    if ticket.claimed_by.is_some() {
        "claimed"
    } else {
        "unclaimed"
    }
}

fn status_text(status: TicketStatus) -> &'static str {
    match status {
        TicketStatus::Creating => "creating",
        TicketStatus::Open => "open",
        TicketStatus::Closing => "closing",
        TicketStatus::CleanupPending => "cleanup pending",
        TicketStatus::Closed => "closed",
    }
}

/// A channel mention is a convenience link, never a readiness check. Stored
/// channel text must still be a nonzero decimal snowflake before it reaches
/// Discord's mention syntax.
fn usable_channel(ticket: &Ticket) -> Option<&str> {
    (ticket.status == TicketStatus::Open)
        .then_some(ticket.channel_id.as_deref())
        .flatten()
        .filter(|channel| {
            !channel.is_empty()
                && channel.bytes().all(|byte| byte.is_ascii_digit())
                && channel.parse::<u64>().is_ok_and(|id| id != 0)
        })
}

/// Make persisted display values single-line, mention-safe plain text. They
/// are not wrapped in markdown, and every markdown delimiter is escaped so a
/// corrupt legacy value cannot change the structure of an operator's view.
fn display_value(value: &str) -> String {
    let flattened: String = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    let normalized = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    let neutralized = neutralize_mentions(&normalized);
    let mut escaped = String::with_capacity(neutralized.len());
    for character in neutralized.chars() {
        match character {
            '<' => escaped.push('‹'),
            '>' => escaped.push('›'),
            '\\' | '`' | '*' | '_' | '~' | '|' | '#' | '[' | ']' | '(' | ')' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    let mut bounded = truncate(&escaped, DISPLAY_VALUE_LIMIT);
    while bounded.ends_with('\\') {
        bounded.pop();
    }
    if bounded.is_empty() {
        "(unknown)".to_owned()
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(
        id: &str,
        opener_id: &str,
        status: TicketStatus,
        channel_id: Option<&str>,
        created_at: i64,
    ) -> Ticket {
        Ticket {
            id: id.to_owned(),
            guild_id: "guild".to_owned(),
            channel_id: channel_id.map(str::to_owned),
            opener_id: opener_id.to_owned(),
            claimed_by: None,
            status,
            created_at,
            closing_started_at: None,
            closed_at: None,
        }
    }

    #[test]
    fn mine_only_links_stored_open_channels() {
        let open = ticket("open", "member", TicketStatus::Open, Some("601"), 1_000);
        let closing = ticket(
            "closing",
            "member",
            TicketStatus::Closing,
            Some("602"),
            2_000,
        );
        let malformed_open = ticket(
            "malformed",
            "member",
            TicketStatus::Open,
            Some("+603"),
            3_000,
        );

        let text = render_my_tickets(&[open, closing, malformed_open]);

        assert!(text.contains("open"));
        assert!(text.contains("closing"));
        assert!(text.contains("<#601>"));
        assert!(!text.contains("<#602>"));
        assert!(!text.contains("<#+603>"));
        assert!(text.contains("unclaimed"));
    }

    #[test]
    fn queue_pages_are_bounded_and_render_opener_inertly() {
        let mut tickets = vec![ticket(
            "hostile",
            "x\nINJECTED_LINE\n@everyone\u{202e}spoofed",
            TicketStatus::Creating,
            None,
            1_700_000_000_000,
        )];
        tickets.extend((0..20).map(|index| {
            ticket(
                &format!("t-{index}"),
                "member",
                TicketStatus::Open,
                Some("601"),
                1_700_000_001_000 + index,
            )
        }));

        let first = render_ticket_queue(&tickets, 1).expect("first page");
        let second = render_ticket_queue(&tickets, 2).expect("second page");

        assert!(first.contains("Page 1 of"));
        assert!(second.contains("Page 2 of"));
        assert!(first.contains("<t:1700000000:F>"));
        assert!(!first.contains("x\nINJECTED_LINE"));
        assert!(!first.contains("@everyone"));
        assert!(!first.contains('\u{202e}'));
        assert!(first.encode_utf16().count() <= 2000);
        assert!(second.encode_utf16().count() <= 2000);
    }

    #[test]
    fn queue_refuses_pages_outside_the_available_range() {
        let tickets = vec![ticket(
            "only",
            "member",
            TicketStatus::Open,
            Some("601"),
            1_000,
        )];

        assert_eq!(
            render_ticket_queue(&tickets, 2),
            Err(TICKET_QUEUE_PAGE_OUT_OF_RANGE)
        );
    }
}
