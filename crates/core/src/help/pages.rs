//! Pure packing for ordered discovery content, before Discord rendering.

use crate::message_safety::{
    neutralize_mentions, text_len, EMBED_DESCRIPTION_LIMIT, EMBED_FIELD_NAME_LIMIT,
    EMBED_FIELD_VALUE_LIMIT, EMBED_TOTAL_LIMIT,
};

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

/// Pack all blocks or fail without returning a partial plan. `reserved` covers
/// title, footer and any navigation text, even if it goes in the description.
pub fn plan(blocks: &[Block<'_>], reserved: usize) -> Result<Vec<Page>, PlanError> {
    if reserved > EMBED_DESCRIPTION_LIMIT - 2 {
        return Err(PlanError);
    }
    let total = EMBED_TOTAL_LIMIT - reserved;
    let desc_limit = EMBED_DESCRIPTION_LIMIT - reserved;
    let mut pages = vec![Page::default()];
    let (mut bytes, mut units, mut used) = (0usize, 0, 0);
    for &block in blocks {
        let (name, value, is_field) = match block {
            Block::Description(value) => ("", value, false),
            Block::Field(name, value) => (name, value, true),
        };
        // Bound work before allocating neutralized text, including separators
        // for a possible field fallback. The UTF-16 cap also bounds safe bytes.
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        if bytes > SOURCE_BYTES || value.is_empty() {
            return Err(PlanError);
        }
        let name = neutralize_mentions(name);
        let mut value = neutralize_mentions(value);
        let name_units = text_len(&name);
        let value_units = text_len(&value);
        let cost = name_units + value_units;
        let mut page = pages.last_mut().expect("page exists");
        let joins_description = page.fields.is_empty() && used != 0;
        units += cost + usize::from(is_field) + usize::from(joins_description);
        if units > SOURCE_UNITS {
            return Err(PlanError);
        }
        if is_field
            && name_units <= EMBED_FIELD_NAME_LIMIT
            && value_units <= EMBED_FIELD_VALUE_LIMIT
            && !name.trim().is_empty()
            && !value.trim().is_empty()
        {
            if page.fields.len() == FIELDS_PER_PAGE || used + cost > total {
                page = new_page(&mut pages)?;
                used = 0;
            }
            page.fields.push((name, value));
            used += cost;
            continue;
        }
        if is_field {
            value = format!("{name}\n{value}");
        }
        if joins_description {
            value.insert(0, '\n');
        }
        let mut rest = value.as_str();
        while !rest.is_empty() {
            // Descriptions cannot follow fields on the same page.
            let mut room = if page.fields.is_empty() {
                desc_limit - used
            } else {
                0
            };
            let mut end = 0;
            for ch in rest.chars() {
                if ch.len_utf16() > room {
                    break;
                }
                room -= ch.len_utf16();
                end += ch.len_utf8();
            }
            let (prefix, next) = rest.split_at(end);
            let visible = next.trim_start();
            let next_is_visible = visible.chars().next().is_some_and(|ch| {
                text_len(&next[..next.len() - visible.len()]) + ch.len_utf16() <= desc_limit
            });
            // Carry a visible scalar into a blank next page. A trailing suffix must all fit.
            if !next.is_empty() && !next_is_visible {
                end = prefix
                    .rfind(|ch: char| !ch.is_whitespace())
                    .filter(|&last| {
                        (!visible.is_empty() || text_len(&rest[last..]) <= desc_limit)
                            && (!prefix[..last].trim().is_empty()
                                || !page.description.trim().is_empty())
                    })
                    .unwrap_or(end);
            }
            page.description.push_str(&rest[..end]);
            used += text_len(&rest[..end]);
            rest = &rest[end..];
            if !rest.is_empty() {
                page = new_page(&mut pages)?;
                used = 0;
            }
        }
    }
    if pages
        .iter()
        .any(|p| !p.description.is_empty() && p.description.trim().is_empty())
    {
        return Err(PlanError);
    }
    Ok(pages)
}

fn new_page(pages: &mut Vec<Page>) -> Result<&mut Page, PlanError> {
    if pages.len() == MAX_HELP_PAGES {
        return Err(PlanError);
    }
    pages.push(Page::default());
    Ok(pages.last_mut().expect("page was just pushed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Block::{Description, Field};

    #[test]
    fn preserves_order_across_notes_and_group_breaks() {
        let blocks = [
            Description("Heading"),
            Field("Group", "/a"),
            Description("Note"),
            Field("Next", "/b"),
            Description("End"),
        ];
        let pages = plan(&blocks, 80).unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].description, "Heading");
        assert_eq!(pages[0].fields, [("Group".to_owned(), "/a".to_owned())]);
        assert_eq!(pages[1].description, "Note");
        assert_eq!(pages[1].fields, [("Next".to_owned(), "/b".to_owned())]);
        assert_eq!(pages[2].description, "End");
    }

    #[test]
    fn keeps_every_field_past_five_pages() {
        let values: Vec<_> = (0..32)
            .map(|i| format!("{i:02}{}", "x".repeat(998)))
            .collect();
        let fields: Vec<_> = values.iter().map(|v| Field("Group", v)).collect();
        let pages = plan(&fields, 80).unwrap();
        assert!(pages.len() > 5);
        assert_eq!(pages[0].fields.len(), 5); // Six would exceed the shared budget.
        let actual = pages.iter().flat_map(|p| &p.fields).map(|(_, v)| v);
        assert!(actual.eq(&values));
        assert_eq!(plan(&[Field("Group", "/a"); 7], 0).unwrap().len(), 2);
    }

    #[test]
    fn splits_fallback_after_neutralizing_mentions_without_splitting_scalars() {
        let value = format!("@here{}", "🔥".repeat(2200));
        let pages = plan(&[Field("Group", &value)], 80).unwrap();
        assert!(pages.len() > 1);
        assert!(pages
            .iter()
            .all(|p| p.fields.is_empty() && text_len(&p.description) <= 4016));
        let joined: String = pages.into_iter().map(|p| p.description).collect();
        assert_eq!(joined, format!("Group\n{}", neutralize_mentions(&value)));
    }

    #[test]
    fn keeps_whitespace_with_visible_text() {
        let full = "a".repeat(4016);
        for (input, lengths) in [
            (format!("{full}\n"), [4015, 2].as_slice()),
            (format!("{}🔥 \n", "a".repeat(4013)), &[4013, 4]),
            (format!("{full}{}b ", " ".repeat(4016)), &[4015, 4016, 3]),
            (format!("{full}{}🔥", " ".repeat(4015)), &[4015, 4016, 2]),
            (format!("{full}{}🔥", " ".repeat(4014)), &[4016, 4016]),
        ] {
            let pages = plan(&[Description(&input)], 80).unwrap();
            let actual: Vec<_> = pages.iter().map(|p| text_len(&p.description)).collect();
            assert_eq!(actual.as_slice(), lengths);
            assert!(pages.iter().all(|p| !p.description.trim().is_empty()));
            let joined: String = pages.into_iter().map(|p| p.description).collect();
            assert_eq!(joined, input);
        }
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
