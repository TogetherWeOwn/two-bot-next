//! Pure packing for ordered discovery content, before Discord rendering.

use crate::message_safety::{
    neutralize_mentions, text_len, EMBED_DESCRIPTION_LIMIT, EMBED_FIELD_NAME_LIMIT,
    EMBED_FIELD_VALUE_LIMIT, EMBED_TOTAL_LIMIT,
};
use std::collections::VecDeque;

const SOURCE_BYTES: usize = 128 * 1024;
const SOURCE_UNITS: usize = 32_768;
const FIELDS_PER_PAGE: usize = 6;
pub const MAX_HELP_PAGES: usize = 128;

#[derive(Clone, Copy)]
pub enum Block<'a> {
    /// Adjacent description blocks are separated by a newline.
    Description(&'a str),
    /// An already-grouped fragment: heading (including hint), then value.
    Field(&'a str, &'a str),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub description: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PlanError;

/// Pack all blocks or fail. Reserve space for title, footer and navigation.
pub fn plan(blocks: &[Block<'_>], reserved: usize) -> Result<Vec<Page>, PlanError> {
    let desc_limit = EMBED_DESCRIPTION_LIMIT.saturating_sub(reserved);
    (desc_limit >= 2).then_some(()).ok_or(PlanError)?;
    let mut pages = vec![Page::default()];
    let (mut bytes, mut units, mut used) = (0usize, SOURCE_UNITS, 0);
    let mut run = String::new();
    for &block in blocks {
        let (name, value, is_field) = match block {
            Block::Description(value) => ("", value, false),
            Block::Field(name, value) => (name, value, true),
        };
        // Bound raw input before neutralizing; count fallback separators below.
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        if bytes > SOURCE_BYTES || value.is_empty() {
            return Err(PlanError);
        }
        let [name, value] = [name, value].map(neutralize_mentions);
        let [name_units, value_units] = [&name, &value].map(|s| text_len(s));
        let cost = name_units + value_units;
        let joins = !run.is_empty();
        let size = cost + usize::from(is_field) + usize::from(joins);
        units = units.checked_sub(size).ok_or(PlanError)?;
        if name_units <= EMBED_FIELD_NAME_LIMIT
            && value_units <= EMBED_FIELD_VALUE_LIMIT
            && !name.trim().is_empty()
            && !value.trim().is_empty()
        {
            if joins {
                used = flush(&mut pages, std::mem::take(&mut run), desc_limit)?;
            }
            let mut page = pages.last_mut().expect("page exists");
            if page.fields.len() == FIELDS_PER_PAGE || used + cost > EMBED_TOTAL_LIMIT - reserved {
                page = new_page(&mut pages)?;
                used = 0;
            }
            page.fields.push((name, value));
            used += cost;
        } else {
            if joins {
                run.push('\n');
            }
            if is_field {
                run.extend([name.as_str(), "\n"]);
            }
            run.push_str(&value);
        }
    }
    if !run.is_empty() {
        flush(&mut pages, run, desc_limit)?;
    }
    Ok(pages)
}

fn flush(pages: &mut Vec<Page>, text: String, limit: usize) -> Result<usize, PlanError> {
    let mut page = pages.last_mut().expect("page exists");
    if !page.fields.is_empty() {
        page = new_page(pages)?;
    }
    let width = text_len(&text);
    if width <= limit {
        (!text.trim().is_empty()).then_some(()).ok_or(PlanError)?;
        page.description = text;
        return Ok(width);
    }
    // Boundaries in reverse order: byte offset, suffix units, score, next cut.
    let mut suffix = vec![(text.len(), 0, 0, 0)];
    let (mut units, mut scanned) = (0, 0);
    let mut ends = VecDeque::<usize>::new();
    for (byte, ch) in text.char_indices().rev() {
        units += ch.len_utf16();
        // Both ends of the valid window move left. Keep the furthest cut on ties.
        if !ch.is_whitespace() {
            for end in (scanned..suffix.len()).filter(|&i| suffix[i].2 != usize::MAX) {
                let worse = |&old: &usize| suffix[old].2 > suffix[end].2;
                while ends.back().is_some_and(worse) {
                    ends.pop_back();
                }
                ends.push_back(end);
            }
            scanned = suffix.len();
        }
        let over_budget = |&end: &usize| units - suffix[end].1 > limit;
        while ends.front().is_some_and(over_budget) {
            ends.pop_front();
        }
        let (score, cut) = match ends.front() {
            Some(&end) => (suffix[end].2 + 1, end),
            None => (usize::MAX, usize::MAX),
        };
        suffix.push((byte, units, score, cut));
    }
    let mut start = suffix.len() - 1;
    loop {
        let (byte, units, _, end) = suffix[start];
        let end_byte = suffix.get(end).ok_or(PlanError)?.0;
        page.description.push_str(&text[byte..end_byte]);
        if end == 0 {
            return Ok(units);
        }
        page = new_page(pages)?;
        start = end;
    }
}

fn new_page(pages: &mut Vec<Page>) -> Result<&mut Page, PlanError> {
    let room = pages.len() < MAX_HELP_PAGES;
    room.then_some(()).ok_or(PlanError)?;
    pages.push(Page::default());
    pages.last_mut().ok_or(PlanError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Block::{Description, Field};

    #[test]
    fn preserves_order_across_notes_and_group_breaks() {
        let notes = ["Heading", "Note", "End"];
        let [a, b, c] = notes.map(Description);
        let blocks = [a, Field("Group", "/a"), b, Field("Next", "/b"), c];
        let pages = plan(&blocks, 80).unwrap();
        let actual: Vec<_> = pages.iter().map(|p| p.description.as_str()).collect();
        assert_eq!(actual, notes);
        assert_eq!(pages[0].fields, [("Group".to_owned(), "/a".to_owned())]);
        assert_eq!(pages[1].fields, [("Next".to_owned(), "/b".to_owned())]);
    }

    #[test]
    fn keeps_every_field_past_five_pages() {
        let tail = "x".repeat(998);
        let values: Vec<_> = (0..32).map(|i| format!("{i:02}{tail}")).collect();
        let fields: Vec<_> = values.iter().map(|v| Field("Group", v)).collect();
        let pages = plan(&fields, 80).unwrap();
        assert!(pages.len() > 5);
        assert_eq!(pages[0].fields.len(), 5); // Six would exceed the shared budget.
        let actual = pages.iter().flat_map(|p| &p.fields).map(|(_, v)| v);
        assert!(actual.eq(&values));
        assert_eq!(plan(&[Field("Group", "/a"); 7], 0).unwrap().len(), 2);
    }

    #[test]
    fn splits_descriptions_with_a_packable_suffix() {
        let full = "a".repeat(4016);
        let deep_suffix = format!("{}  b{}", "a".repeat(4015), " ".repeat(4015));
        for (input, lengths) in [
            (format!("{full}\n"), [4015, 2].as_slice()),
            (format!("{}🔥 \n", "a".repeat(4013)), &[4013, 4]),
            (format!("{full}{}b ", " ".repeat(4016)), &[4015, 4016, 3]),
            (format!("{full}{}🔥", " ".repeat(4015)), &[4015, 4016, 2]),
            (format!("{full}{}🔥", " ".repeat(4014)), &[4016, 4016]),
            (deep_suffix, &[4014, 3, 4016]),
        ] {
            assert_split(plan(&[Description(&input)], 80).unwrap(), &input, lengths);
        }
        let reserved = EMBED_DESCRIPTION_LIMIT - 4;
        for (tail, lengths) in [("d", &[4, 3][..]), ("d   ", &[2, 4, 4])] {
            let pages = plan(&["abc", " ", tail].map(Description), reserved).unwrap();
            assert_split(pages, &format!("abc\n \n{tail}"), lengths);
        }
        let value = format!("@here{}", "🔥".repeat(2200));
        let pages = plan(&[Description("Notes"), Field("Group", &value)], 80).unwrap();
        let expected = format!("Notes\nGroup\n{}", neutralize_mentions(&value));
        assert_split(pages, &expected, &[4016, text_len(&expected) - 4016]);
    }

    fn assert_split(pages: Vec<Page>, input: &str, lengths: &[usize]) {
        let actual: Vec<_> = pages.iter().map(|p| text_len(&p.description)).collect();
        assert_eq!(actual, lengths);
        for page in &pages {
            assert!(page.fields.is_empty() && !page.description.trim().is_empty());
        }
        let joined: String = pages.into_iter().map(|p| p.description).collect();
        assert_eq!(joined, input);
    }

    #[test]
    fn rejects_a_whole_plan_when_bounds_are_exceeded() {
        assert_eq!(plan(&[], 80), Ok(vec![Page::default()]));
        assert_eq!(plan(&[], EMBED_DESCRIPTION_LIMIT), Err(PlanError));
        for input in [
            String::new(),
            " ".into(),
            format!("a{}", " ".repeat(4016)),
            "a".repeat(SOURCE_UNITS + 1),
            "a".repeat(SOURCE_BYTES + 1),
        ] {
            assert_eq!(plan(&[Description(&input)], 80), Err(PlanError));
        }
        let too_many = [Field("Group", "/a"), Description("Note")].repeat(MAX_HELP_PAGES);
        assert_eq!(plan(&too_many, 80), Err(PlanError));
    }
}
