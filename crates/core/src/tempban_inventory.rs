//! Read-only saved-expiry inventory contract. No claims or Discord effects.

use crate::commands::{CommandDefinition, CommandOption, CommandOptionType, PERM_BAN_MEMBERS};

pub const PAGE_SIZE: usize = 10;
pub const PAGE_MAX: i64 = 10_000;
pub const SELECTOR_CORRECTION: &str =
    "Use /tempban-list page:1 with at most one integer page from 1 to 10000; no other options.";
pub const UNAVAILABLE: &str =
    "Saved expiry inventory is unavailable. Try /tempban-list page:1 again shortly; if it persists, ask an operator.";

/// A validated selector; the offset cannot exceed 99,990 rows.
#[derive(Clone, Copy)]
pub struct ExpiryPage(i64);

impl ExpiryPage {
    #[must_use]
    pub fn new(number: i64) -> Option<Self> {
        (1..=PAGE_MAX).contains(&number).then_some(Self(number))
    }

    #[must_use]
    pub fn number(self) -> i64 {
        self.0
    }

    #[must_use]
    pub fn offset(self) -> i64 {
        (self.0 - 1) * PAGE_SIZE as i64
    }
}

#[derive(Clone, Copy)]
pub enum ExpiryState {
    Pending,
    Staged,
    Running,
    Quarantined,
}

/// Minimal public projection: deliberately excludes reasons, attribution and
/// ownership capabilities. Due is observed at the database read, not dispatch.
pub struct SavedExpiry {
    pub request_id: String,
    pub user_id: String,
    pub execute_at: i64,
    pub state: ExpiryState,
    pub dispatch_uncertain: bool,
    pub due: bool,
}

#[must_use]
pub fn command_definition() -> CommandDefinition {
    CommandDefinition::new("tempban-list", "Inspect saved temporary-ban expiry work")
        .permissions(PERM_BAN_MEMBERS)
        .options(vec![CommandOption::new(
            "page",
            "Saved expiry page (10 rows, default 1; due time then work ID order)",
            CommandOptionType::Integer,
        )
        .int_range(1, PAGE_MAX)])
}

/// Percent-encode unsafe characters, including percent itself, so the complete
/// identity remains recoverable without mentions, markdown or control text.
/// Ordinary Unicode is retained; format controls are encoded as UTF-8 bytes.
fn identity(value: &str) -> String {
    let mut safe = String::new();
    for ch in value.chars() {
        if ch.is_alphanumeric()
            || matches!(ch, '-' | '.' | ':')
            || (!ch.is_ascii() && !unsafe_unicode(ch))
        {
            safe.push(ch);
        } else {
            for byte in ch.encode_utf8(&mut [0; 4]).bytes() {
                use std::fmt::Write;
                let _ = write!(safe, "%{byte:02X}");
            }
        }
    }
    safe
}

fn unsafe_unicode(ch: char) -> bool {
    ch.is_control()
        || ch.is_whitespace()
        || matches!(ch, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

/// Render a store-bounded page plus optional lookahead. Never clip an identity
/// or silently lose rows: an oversized complete page fails closed instead.
#[must_use]
pub fn render(page: ExpiryPage, rows: &[SavedExpiry]) -> String {
    if rows.is_empty() {
        return if page.number() == 1 {
            "No saved active temporary-ban expiries. This is saved work, not proof of a Discord unban. Refresh with /tempban-list page:1.".to_owned()
        } else {
            format!("No saved expiry rows on page {}. Return to /tempban-list page:1; pages may shift as saved work changes.", page.number())
        };
    }
    let mut output = format!(
        "Saved temporary-ban expiry work — page {}\nDue time, then work ID order; terminal history excluded.\n",
        page.number()
    );
    for row in rows.iter().take(PAGE_SIZE) {
        let state = match row.state {
            ExpiryState::Pending => "pending",
            ExpiryState::Staged => "staged/prepared",
            ExpiryState::Running => "running",
            ExpiryState::Quarantined => "quarantined",
        };
        let uncertainty = if row.dispatch_uncertain {
            ", dispatch uncertain"
        } else {
            ""
        };
        let timing = if row.due { "due" } else { "future" };
        output.push_str(&format!(
            "Work `{}` | member `{}` | <t:{}:f> | {state}, {timing}{uncertainty}\n",
            identity(&row.request_id),
            identity(&row.user_id),
            row.execute_at
        ));
        if output.encode_utf16().count() > 2000 {
            return OVERSIZE.to_owned();
        }
    }
    output
        .push_str("Saved/due is not confirmation of a Discord unban; running is not completion.\n");
    if rows.len() > PAGE_SIZE && page.number() < PAGE_MAX {
        output.push_str(&format!("Next: /tempban-list page:{}", page.number() + 1));
    } else if rows.len() > PAGE_SIZE {
        output.push_str(
            "Browse ceiling reached (page 10000); ask an operator for bounded inspection.",
        );
    } else {
        output.push_str("End of this saved page set. Refresh: /tempban-list page:1");
    }
    if output.encode_utf16().count() > 2000 {
        OVERSIZE.to_owned()
    } else {
        output
    }
}

const OVERSIZE: &str = "This saved expiry page cannot fit complete safely encoded IDs in a private reply. No rows are shown. Ask an operator for bounded inspection; /tempban-list page:1 refreshes the saved inventory.";
