//! Pure packing for ordered discovery content, before Discord rendering.

use crate::message_safety::{
    has_message_text, neutralize_mentions, text_len, EMBED_DESCRIPTION_LIMIT,
    EMBED_FIELD_NAME_LIMIT, EMBED_FIELD_VALUE_LIMIT, EMBED_TOTAL_LIMIT,
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

/// Pack all blocks or fail. Both reservations use the total; `inside` also uses the description.
/// Splittable content, including field fallbacks, must be plain text (cuts can break Markdown).
pub fn plan(blocks: &[Block<'_>], outside: usize, inside: usize) -> Result<Vec<Page>, PlanError> {
    let charge = |budget: usize, cost| budget.checked_sub(cost).ok_or(PlanError);
    let reserved = outside.checked_add(inside).ok_or(PlanError)?;
    let total = charge(EMBED_TOTAL_LIMIT, reserved)?;
    let desc_limit = charge(EMBED_DESCRIPTION_LIMIT, inside)?.min(total);
    let mut pages = vec![Page::default()];
    let (mut bytes, mut units, mut used) = (SOURCE_BYTES, SOURCE_UNITS, 0);
    let mut run = None::<String>;
    for &block in blocks {
        let parts = match block {
            Block::Field("", "") => return Err(PlanError),
            Block::Field(name, value) => [name, value],
            Block::Description(text) => ["", text],
        };
        bytes = charge(bytes, parts[0].len() + parts[1].len())?;
        let [name, value] = parts.map(neutralize_mentions);
        let [n, v] = [&name, &value].map(|s| text_len(s));
        let cost = n + v;
        units = charge(units, cost)?;
        let fits = n <= EMBED_FIELD_NAME_LIMIT && v <= EMBED_FIELD_VALUE_LIMIT;
        let nonempty = !name.trim().is_empty() && !value.trim().is_empty();
        if fits && cost <= total && nonempty {
            if let Some(text) = run.take() {
                used = flush(&mut pages, text, desc_limit)?;
            }
            let mut page = pages.last_mut().expect("page exists");
            if page.fields.len() == FIELDS_PER_PAGE || used + cost > total {
                page = new_page(&mut pages)?;
                used = 0;
            }
            page.fields.push((name, value));
            used += cost;
        } else {
            let name = (n != 0).then_some(name);
            let value = (v != 0 || matches!(block, Block::Description(_))).then_some(value);
            for part in [name, value].into_iter().flatten() {
                let sep = if run.is_some() { "\n" } else { "" };
                units = charge(units, sep.len())?;
                run.get_or_insert_default().extend([sep, part.as_str()]);
            }
        }
    }
    if let Some(text) = run {
        flush(&mut pages, text, desc_limit)?;
    }
    let visible = |p: &Page| {
        let field = |(n, v): &(String, String)| has_message_text(n) || has_message_text(v);
        has_message_text(&p.description) || p.fields.iter().any(field)
    };
    ((blocks.is_empty() && reserved != 0) || pages.iter().all(visible))
        .then_some(pages)
        .ok_or(PlanError)
}

fn flush(pages: &mut Vec<Page>, text: String, limit: usize) -> Result<usize, PlanError> {
    let mut page = pages.last_mut().expect("page exists");
    if !page.fields.is_empty() {
        page = new_page(pages)?;
    }
    let width = text_len(&text);
    if width <= limit {
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
        if has_message_text(&text[byte..byte + ch.len_utf8()]) {
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
        let cut = ends.front().copied().unwrap_or(usize::MAX);
        let score = suffix.get(cut).map_or(usize::MAX, |end| end.2 + 1);
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
        let (heading, z) = ("x".repeat(4050), "\u{200b}");
        let mut blocks = vec![Description(&heading), Field("Next", "/b")];
        blocks.extend([Description("Overview"), Field(z, z)]);
        blocks.extend([Description(z), Field("Group", "/a")]);
        blocks.extend(["Overview", "", "Commands"].map(Description));
        blocks.extend([Field(z, "/a"), Field("End", z), Field("End", "")]);
        let pages = plan(&blocks, 80, 0).unwrap();
        let notes = "Overview\n\nCommands";
        let expected = [heading.as_str(), "Overview", z, notes, "End"];
        assert!(pages.iter().map(|p| p.description.as_str()).eq(expected));
        assert!(pages.iter().map(|p| p.fields.len()).eq([1, 1, 1, 2, 0]));
        let pairs = pages.iter().flat_map(|p| &p.fields);
        let actual = pairs.map(|(n, v)| (n.as_str(), v.as_str()));
        let groups = [("Next", "/b"), (z, z), ("Group", "/a")].into_iter();
        assert!(actual.eq(groups.chain([(z, "/a"), ("End", z)])));
    }

    #[test]
    fn keeps_every_field_past_five_pages() {
        let (name, note) = ("x".repeat(256), "x".repeat(1024));
        let values: Vec<_> = (0..32).map(|i| format!("{i:0768}")).collect();
        let mut fields: Vec<_> = values.iter().map(|v| Field(&name, v)).collect();
        let pages = plan(&fields, 80, 0).unwrap(); // Exactly 32,768 source units.
        assert!(pages.len() > 5);
        assert_eq!(pages[0].fields.len(), 5); // Six would exceed the shared budget.
        let actual = pages.iter().flat_map(|p| &p.fields).map(|(_, v)| v);
        assert!(actual.eq(&values));
        fields[30] = Description(&note);
        let pages = plan(&fields, 80, 0).unwrap();
        assert_eq!(pages.last().unwrap().description, note);
        let pages = plan(&[Field("Group", "/a"); 7], 0, EMBED_DESCRIPTION_LIMIT).unwrap();
        assert!(pages.iter().map(|p| p.fields.len()).eq([6, 1]));
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
            (format!("{full}\u{200b}"), &[4015, 2]),
        ] {
            let pages = plan(&[Description(&input)], 0, 80).unwrap();
            assert_split(pages, &input, lengths);
        }
        let reserved = EMBED_DESCRIPTION_LIMIT - 4;
        for (tail, lengths) in [("d", &[4, 3][..]), ("d   ", &[2, 4, 4])] {
            let pages = plan(&["abc", " ", tail].map(Description), 0, reserved).unwrap();
            assert_split(pages, &format!("abc\n \n{tail}"), lengths);
        }
        let value = format!("@here{}", "🔥".repeat(2200));
        let expected = format!("Notes\nGroup\n{}", neutralize_mentions(&value));
        let pages = plan(&[Description("Notes"), Field("Group", &value)], 2200, 100).unwrap();
        assert_split(pages, &expected, &[3700, text_len(&expected) - 3700]);
    }

    fn assert_split(pages: Vec<Page>, input: &str, lengths: &[usize]) {
        let parts = pages.iter().map(|p| p.description.as_str());
        assert!(parts.clone().map(text_len).eq(lengths.iter().copied()));
        assert!(pages.iter().all(|p| p.fields.is_empty()) && parts.clone().all(has_message_text));
        assert_eq!(parts.collect::<String>(), input);
    }

    #[test]
    fn rejects_a_whole_plan_when_bounds_are_exceeded() {
        let limit = EMBED_DESCRIPTION_LIMIT;
        assert_split(plan(&[Description("A")], 0, limit - 1).unwrap(), "A", &[1]);
        let empty = |outside, inside| plan(&[], outside, inside);
        assert_eq!(empty(80, 0), Ok(vec![Page::default()]));
        assert_eq!(empty(EMBED_TOTAL_LIMIT - limit, limit), empty(80, 0));
        assert!(empty(0, 0).is_err() && empty(0, limit + 1).is_err());
        assert!(empty(EMBED_TOTAL_LIMIT, 1).is_err() && empty(usize::MAX, 1).is_err());
        let (z, group) = ("\u{200b}", Field("Group", "/a"));
        assert!(plan(&[Field(z, z)], 0, 0).is_err());
        assert!(plan(&[group, Description(z)], 0, 0).is_err());
        for input in [
            String::new(),
            " ".into(),
            "\u{200b}".into(),
            "\u{200b}".repeat(4017),
            format!("a{}", " ".repeat(4016)),
            "a".repeat(SOURCE_UNITS + 1),
            "a".repeat(SOURCE_BYTES + 1),
        ] {
            assert_eq!(plan(&[Description(&input)], 0, 80), Err(PlanError));
        }
        let too_many = [Field("Group", "/a"), Description("Note")].repeat(MAX_HELP_PAGES);
        assert_eq!(plan(&too_many, 80, 0), Err(PlanError));
    }
}
