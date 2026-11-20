//! Turn one confirmed discovery catalog into ordered help pages.

use super::help_catalog;
use super::pages::{plan, Block, Page, PlanError};
use crate::commands::{CommandDefinition, GUILD_COMMAND_LIMIT};
use crate::custom_commands::MAX_COMMAND_NAME_CHARS;
use crate::message_safety::{
    neutralize_mentions, text_len, EMBED_FIELD_VALUE_LIMIT, EMBED_TOTAL_LIMIT,
};

const COMMAND_SEPARATOR: &str = " · ";

/// Plan one confirmed catalog, rejecting budgets that would split command fields.
/// `reserved` covers the caller's embed chrome and navigation text.
pub fn plan_discovery(defs: &[CommandDefinition], reserved: usize) -> Result<Vec<Page>, PlanError> {
    let budget = EMBED_TOTAL_LIMIT.checked_sub(reserved).ok_or(PlanError)?;
    if defs.len() > GUILD_COMMAND_LIMIT
        || defs.iter().any(|def| {
            def.name.is_empty() || def.name.chars().nth(MAX_COMMAND_NAME_CHARS).is_some()
        })
    {
        return Err(PlanError);
    }
    let catalog = help_catalog(defs);
    let mut groups = Vec::new();
    for section in catalog.sections {
        let title = match section.requires {
            Some(perm) => format!("{} — needs {perm}", section.title),
            None => section.title.to_owned(),
        };
        groups.push((title, command_fields(&section.commands), section.notes));
    }
    let mut blocks = vec![Block::Description(&catalog.heading)];
    for (title, fields, notes) in &groups {
        for value in fields {
            if text_len(title) + text_len(value) > budget {
                return Err(PlanError);
            }
            blocks.push(Block::Field(title, value));
        }
        blocks.extend(notes.iter().copied().map(Block::Description));
    }
    blocks.push(Block::Description(catalog.closing_note));
    plan(&blocks, reserved, 0)
}

fn command_fields(names: &[&str]) -> Vec<String> {
    let (mut fields, mut value, mut used) = (Vec::new(), String::new(), 0);
    for name in names {
        let safe = neutralize_mentions(name);
        let units = 1 + text_len(&safe); // Include the slash.
        if used + text_len(COMMAND_SEPARATOR) + units > EMBED_FIELD_VALUE_LIMIT {
            fields.push(std::mem::take(&mut value));
            used = 0;
        } else if used != 0 {
            value.push_str(COMMAND_SEPARATOR);
            used += text_len(COMMAND_SEPARATOR);
        }
        value.push('/');
        value.push_str(&safe);
        used += units;
    }
    if !value.is_empty() {
        fields.push(value);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::{tests::maximum_publication, HELP_GROUPS};

    #[test]
    fn maximum_publication_reconstructs_every_group_and_note() {
        let (defs, _) = maximum_publication();
        assert_eq!(defs.len(), GUILD_COMMAND_LIMIT);
        let catalog = help_catalog(&defs);
        let pages = plan_discovery(&defs, 150).expect("bounded discovery");
        let mut actual = pages.iter().flat_map(|page| {
            page.description
                .lines()
                .map(|line| ("", line))
                .chain(page.fields.iter().flat_map(|(title, value)| {
                    value
                        .split(COMMAND_SEPARATOR)
                        .map(|command| (title.as_str(), command))
                }))
        });
        assert_eq!(actual.next(), Some(("", catalog.heading.as_str())));
        for group in &catalog.sections {
            let title = match group.requires {
                Some(perm) => format!("{} — needs {perm}", group.title),
                None => group.title.to_owned(),
            };
            for command in group.commands.iter().map(|name| format!("/{name}")) {
                assert_eq!(actual.next(), Some((title.as_str(), command.as_str())));
            }
            for note in group.notes {
                assert_eq!(actual.next(), Some(("", *note)));
            }
        }
        assert_eq!(actual.next(), Some(("", catalog.closing_note)));
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
        let field_count: usize = pages.iter().map(|page| page.fields.len()).sum();
        assert_eq!(field_count, HELP_GROUPS.len());
        let empty = plan_discovery(&[], 150).expect("empty registry");
        assert_eq!(empty.len(), 1);
        assert!(empty[0].fields.is_empty());
        let catalog = help_catalog(&[]);
        let expected = format!("{}\n{}", catalog.heading, catalog.closing_note);
        assert_eq!(empty[0].description, expected);
    }

    #[test]
    fn rejects_reservation_that_would_split_a_command() {
        let def = CommandDefinition::new(&"x".repeat(MAX_COMMAND_NAME_CHARS), "A test command");
        let reserved = EMBED_TOTAL_LIMIT - 20;
        assert_eq!(plan_discovery(&[def], reserved), Err(PlanError));
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
        let fields = command_fields(&[name.as_str(); GUILD_COMMAND_LIMIT]);
        assert!(fields.len() > 1);
        assert!(fields
            .iter()
            .all(|value| text_len(value) <= EMBED_FIELD_VALUE_LIMIT));
        let command = format!("/{}", neutralize_mentions(&name));
        assert_eq!(
            fields
                .iter()
                .flat_map(|value| value.split(COMMAND_SEPARATOR))
                .collect::<Vec<_>>(),
            vec![command.as_str(); GUILD_COMMAND_LIMIT]
        );
    }
}
