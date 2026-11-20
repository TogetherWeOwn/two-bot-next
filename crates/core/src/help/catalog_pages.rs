//! Turn one confirmed discovery catalog into ordered help pages.

use super::pages::{plan, Block, Page, PlanError};
use super::{help_catalog, HelpSection};
use crate::commands::{CommandDefinition, GUILD_COMMAND_LIMIT};
use crate::custom_commands::MAX_COMMAND_NAME_CHARS;
use crate::message_safety::{
    neutralize_mentions, text_len, EMBED_FIELD_NAME_LIMIT, EMBED_FIELD_VALUE_LIMIT,
};

const COMMAND_SEPARATOR: &str = " · ";

/// Plan discovery without clipping. `reserved` covers the caller's title and
/// longest footer. The repeated description is budgeted here. Pass definitions
/// from one confirmed snapshot.
pub fn plan_discovery(defs: &[CommandDefinition], reserved: usize) -> Result<Vec<Page>, PlanError> {
    // Bound formatting before allocating the catalog or joined commands.
    if defs.len() > GUILD_COMMAND_LIMIT
        || defs.iter().any(|def| {
            def.name.is_empty()
                || def.name.chars().take(MAX_COMMAND_NAME_CHARS + 1).count()
                    > MAX_COMMAND_NAME_CHARS
        })
    {
        return Err(PlanError);
    }
    let catalog = help_catalog(defs);
    let description = format!("{}\n{}", catalog.heading, catalog.closing_note);
    let groups: Vec<_> = catalog.sections.iter().flat_map(group_fragments).collect();
    let blocks: Vec<_> = groups.iter().map(Fragment::block).collect();
    let mut pages = plan(&blocks, reserved.saturating_add(text_len(&description) + 2))?;
    for page in &mut pages {
        page.description = if page.description.is_empty() {
            description.clone()
        } else {
            format!("{description}\n\n{}", page.description)
        };
    }
    Ok(pages)
}

enum Fragment {
    Field(String, String),
    Description(String),
}

impl Fragment {
    fn block(&self) -> Block<'_> {
        match self {
            Self::Field(title, value) => Block::Field(title, value),
            Self::Description(text) => Block::DescriptionFragment(text),
        }
    }
}

fn group_fragments(section: &HelpSection<'_>) -> Vec<Fragment> {
    let hint = section.requires.map(|perm| format!("Needs {perm}\n"));
    let hint = hint.as_deref().unwrap_or_default();
    let capacity = EMBED_FIELD_VALUE_LIMIT.saturating_sub(text_len(hint));
    let commands = section
        .commands
        .iter()
        .map(|name| (format!("/{name}"), COMMAND_SEPARATOR));
    let notes = section.notes.iter().map(|note| (note.to_string(), "\n"));
    let mut fragments = Vec::new();
    let mut body = String::new();
    let mut used = 0;
    for (entry, separator) in commands.chain(notes) {
        let entry = neutralize_mentions(&entry);
        let cost = text_len(&entry);
        if used != 0 && used + text_len(separator) + cost > capacity {
            fragments.push(std::mem::take(&mut body));
            used = 0;
        }
        if used != 0 {
            body.push_str(separator);
            used += text_len(separator);
        }
        body.push_str(&entry);
        used += cost;
    }
    if !body.is_empty() {
        fragments.push(body);
    }
    fragments
        .into_iter()
        .enumerate()
        .map(|(i, body)| {
            let title = if i == 0 {
                section.title.to_owned()
            } else {
                format!("{} (continued)", section.title)
            };
            if text_len(&title) <= EMBED_FIELD_NAME_LIMIT && text_len(&body) <= capacity {
                Fragment::Field(title, format!("{hint}{body}"))
            } else {
                let mut header = format!("**{title}**");
                if let Some(perm) = section.requires {
                    header.push_str(&format!(" — needs {perm}"));
                }
                Fragment::Description(format!("{header}\n{body}"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::{tests::maximum_publication, HELP_GROUPS};

    #[test]
    fn maximum_publication_reconstructs_every_group_and_note() {
        let defs = maximum_publication();
        assert_eq!(defs.len(), GUILD_COMMAND_LIMIT);
        let catalog = help_catalog(&defs);
        let pages = plan_discovery(&defs, 150).expect("bounded discovery");
        assert!(pages.len() > 1);
        let expected_description = format!("{}\n{}", catalog.heading, catalog.closing_note);
        assert!(pages
            .iter()
            .all(|page| page.description == expected_description));
        let mut actual = pages.iter().flat_map(|page| &page.fields).peekable();
        for group in &catalog.sections {
            let continued = format!("{} (continued)", group.title);
            let mut commands = Vec::new();
            let mut notes = Vec::new();
            let mut title = group.title;
            while let Some((_, value)) = actual.next_if(|(name, _)| name == title) {
                let mut lines = value.lines();
                if let Some(perm) = group.requires {
                    assert_eq!(lines.next(), Some(format!("Needs {perm}").as_str()));
                }
                for line in lines {
                    if line.starts_with('/') {
                        assert!(notes.is_empty(), "commands must precede notes");
                        commands.extend(line.split(COMMAND_SEPARATOR));
                    } else {
                        notes.push(line);
                    }
                }
                title = &continued;
            }
            let expected: Vec<_> = group
                .commands
                .iter()
                .map(|name| format!("/{name}"))
                .collect();
            assert_eq!(commands, expected);
            assert_eq!(notes, group.notes);
        }
        assert_eq!(actual.next(), None);
        assert!(catalog.sections.iter().any(|group| !group.notes.is_empty()));
    }

    #[test]
    fn all_groups_and_empty_publication_fit() {
        let defs: Vec<_> = HELP_GROUPS
            .iter()
            .flat_map(|group| group.commands)
            .map(|name| CommandDefinition::new(name, "A test command"))
            .collect();
        assert_eq!(help_catalog(&defs).sections.len(), HELP_GROUPS.len());
        let pages = plan_discovery(&defs, 150).expect("every group fits");
        assert_eq!(
            pages.iter().map(|page| page.fields.len()).sum::<usize>(),
            HELP_GROUPS.len()
        );
        let empty = plan_discovery(&[], 150).expect("empty registry");
        assert_eq!(empty.len(), 1);
        assert!(empty[0].fields.is_empty());
        let catalog = help_catalog(&[]);
        let expected = format!("{}\n{}", catalog.heading, catalog.closing_note);
        assert_eq!(empty[0].description, expected);
    }

    #[test]
    fn rejects_unbounded_input_and_splits_after_mention_neutralization() {
        let long = CommandDefinition::new(&"界".repeat(33), "A test command");
        let empty = CommandDefinition::new("", "A test command");
        assert_eq!(plan_discovery(&[empty], 150), Err(PlanError));
        assert_eq!(plan_discovery(&[long], 150), Err(PlanError));
        let name = "@here😀".repeat(5);
        let defs =
            vec![CommandDefinition::new(name.as_str(), "A test command"); GUILD_COMMAND_LIMIT + 1];
        assert_eq!(plan_discovery(&defs, 150), Err(PlanError));
        let fragments = group_fragments(&HelpSection {
            title: "Custom",
            requires: Some("Manage Server"),
            commands: vec![name.as_str(); GUILD_COMMAND_LIMIT],
            notes: &[],
        });
        assert!(fragments.len() > 1);
        let command = format!("/{}", neutralize_mentions(&name));
        let mut actual = Vec::new();
        for (index, fragment) in fragments.iter().enumerate() {
            let Fragment::Field(title, value) = fragment else {
                panic!("expected field");
            };
            assert_eq!(
                title,
                if index == 0 {
                    "Custom"
                } else {
                    "Custom (continued)"
                }
            );
            assert!(text_len(value) <= EMBED_FIELD_VALUE_LIMIT);
            let rest = value
                .strip_prefix("Needs Manage Server\n")
                .expect("repeated hint");
            actual.extend(rest.split(COMMAND_SEPARATOR));
        }
        assert_eq!(actual, vec![command.as_str(); GUILD_COMMAND_LIMIT]);
    }

    #[test]
    fn fallback_uses_legacy_heading_and_keeps_the_note_whole() {
        // Catalog notes are static. This fixture exercises a future note too
        // large to fit in one field but still safe in the description.
        let note = "🔥".repeat(600);
        let notes = [note.as_str(), "Last note."];
        let fragments = group_fragments(&HelpSection {
            title: "Custom",
            requires: Some("Manage Server"),
            commands: vec!["test"],
            notes: &notes,
        });
        let blocks: Vec<_> = fragments.iter().map(Fragment::block).collect();
        let pages = plan(&blocks, 150).expect("bounded fallback");
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[0].fields,
            [("Custom".into(), "Needs Manage Server\n/test".into())]
        );
        assert_eq!(
            pages[1].description,
            format!("**Custom (continued)** — needs Manage Server\n{note}")
        );
        assert_eq!(
            pages[1].fields,
            [(
                "Custom (continued)".into(),
                "Needs Manage Server\nLast note.".into()
            )]
        );
    }
}
