//! Producer-side checks for the site decoder's row constraints.

use std::{collections::HashSet, hash::Hash};

use super::{Channel, Event, Member};

pub(super) fn distinct<T: Eq + Hash>(mut keys: impl Iterator<Item = T>) -> bool {
    let mut seen = HashSet::new();
    keys.all(|key| seen.insert(key))
}

// The site measures string length in JavaScript UTF-16 code units.
fn short_text(text: &str) -> bool {
    !text.is_empty() && text.encode_utf16().take(101).count() <= 100
}

pub(super) fn channel(row: &Channel) -> bool {
    short_text(&row.name)
        && row.overwrites.len() <= 100
        && distinct(row.overwrites.iter().map(|entry| (entry.kind, entry.id)))
}

pub(super) fn member(row: &Member) -> bool {
    row.nick.as_deref().is_none_or(short_text)
        && row.role_ids.len() <= 250
        && distinct(row.role_ids.iter().copied())
}

pub(super) fn event(row: &Event) -> bool {
    let description = row.description.as_deref().unwrap_or_default();
    // The site's Date.parse compares milliseconds, not nanoseconds.
    let millis = |t: super::WireTime| t.0.unix_timestamp_nanos().div_euclid(1_000_000);
    event_text_is_valid(&row.title, description, row.location.as_deref())
        && row
            .ends_at
            .is_none_or(|end| millis(end) > millis(row.starts_at))
}

/// Check the site's event text bounds before a source row is projected.
pub fn event_text_is_valid(title: &str, description: &str, location: Option<&str>) -> bool {
    short_text(title)
        && location.is_none_or(short_text)
        && description.encode_utf16().take(1001).count() <= 1000
}
