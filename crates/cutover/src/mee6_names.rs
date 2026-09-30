//! MEE6 custom-command name cleaning + deterministic suffixed names.
//!
//! Ports `src/automations/mee6.ts`. A cleaned name is the stable import key,
//! so a changed export updates the same row instead of minting a suffix. The
//! first cleaned name keeps both slash name and text trigger; later rows
//! collapsing to the same name are retained as slash-only commands under
//! deterministic numeric suffixes (`faq-2`, …). The conflict list tells
//! admins which source names no longer own a text trigger.

/// MEE6 placeholder → ours. Anything not in this map is stripped of its
/// braces so text renders rather than leaking MEE6 syntax at members.
fn map_placeholder(key: &str) -> Option<&'static str> {
    match key.to_ascii_lowercase().as_str() {
        "user" => Some("user"),
        "username" => Some("username"),
        "server" | "guild" => Some("server"),
        "channel" => Some("channel"),
        _ => None,
    }
}

/// Clean an MEE6 command name into an invokable one: lowercased, invalid
/// characters dropped, truncated to 32 (legacy `cleanMee6Name`).
#[must_use]
pub fn clean_mee6_name(raw: &str) -> String {
    raw.to_lowercase()
        .bytes()
        .filter(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
        .take(32)
        .map(char::from)
        .collect()
}

/// Translate one MEE6 response into a template (legacy
/// `translateMee6Template`).
#[must_use]
pub fn translate_mee6_template(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = raw[i + 1..].find('}') {
                let key = &raw[i + 1..i + 1 + end];
                // A nested brace means this is not a placeholder span.
                if !key.contains('{') && !key.contains('}') {
                    // Only bare-word keys are looked up; anything else —
                    // dotted, dashed, or simply unmapped — drops so MEE6
                    // syntax never leaks into a member-facing reply.
                    let valid = !key.is_empty()
                        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                    if valid {
                        if let Some(mapped) = map_placeholder(key) {
                            out.push('{');
                            out.push_str(mapped);
                            out.push('}');
                        }
                    }
                    i += end + 2;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// One parsed export row (legacy `Mee6CommandInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mee6CommandInput {
    pub command: String,
    pub description: String,
    pub response: String,
}

/// One translated definition (legacy `TranslatedCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedCommand {
    pub name: String,
    pub description: String,
    pub template: String,
    pub text_trigger: Option<String>,
}

/// Translation outcome (legacy `TranslatedExport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedExport {
    pub commands: Vec<TranslatedCommand>,
    pub conflicts: Vec<String>,
}

/// Translate parsed rows into stable definitions. The first row per cleaned
/// name keeps the text trigger; collisions become slash-only suffixed names.
/// Deterministic in file order (legacy `translateExport`).
#[must_use]
pub fn translate_export(parsed: &[Mee6CommandInput]) -> TranslatedExport {
    use std::collections::{HashMap, HashSet};

    let mut commands = Vec::with_capacity(parsed.len());
    let mut conflicts = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();

    for item in parsed {
        let base = clean_mee6_name(&item.command);
        // A name that cleans to nothing cannot be invoked.
        if base.is_empty() {
            continue;
        }
        let occurrence = counts.get(&base).copied().unwrap_or(0) + 1;
        counts.insert(base.clone(), occurrence);

        let mut name = base.clone();
        if occurrence > 1 || used.contains(&name) {
            conflicts.push(base.clone());
            let mut suffix = occurrence;
            loop {
                let suffix_text = format!("-{suffix}");
                let keep = 32usize.saturating_sub(suffix_text.len());
                name = format!(
                    "{}{suffix_text}",
                    base.chars().take(keep).collect::<String>()
                );
                suffix += 1;
                if !used.contains(&name) {
                    break;
                }
            }
        }
        used.insert(name.clone());

        let description = if item.description.is_empty() {
            "Imported from MEE6".to_owned()
        } else {
            item.description.chars().take(100).collect()
        };
        commands.push(TranslatedCommand {
            name,
            description,
            template: translate_mee6_template(&item.response)
                .chars()
                .take(2000)
                .collect(),
            text_trigger: if occurrence == 1 {
                Some(format!("!{base}"))
            } else {
                None
            },
        });
    }
    TranslatedExport {
        commands,
        conflicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_truncated() {
        assert_eq!(clean_mee6_name("Rules & FAQ!"), "rulesfaq");
        assert_eq!(clean_mee6_name(&"a".repeat(50)).len(), 32);
        assert_eq!(clean_mee6_name("!!!"), "");
    }

    #[test]
    fn placeholders_map_and_unsupported_drop() {
        assert_eq!(
            translate_mee6_template(
                "{user} welcome to {server}! #{member_count} {user.id} {random-choice}"
            ),
            "{user} welcome to {server}! #  "
        );
    }

    #[test]
    fn first_trigger_preserved_later_collisions_suffixed() {
        let long = "a".repeat(32);
        let out = translate_export(&[
            Mee6CommandInput {
                command: "faq".to_owned(),
                description: String::new(),
                response: "first".to_owned(),
            },
            Mee6CommandInput {
                command: "FAQ!".to_owned(),
                description: String::new(),
                response: "second".to_owned(),
            },
            Mee6CommandInput {
                command: "welcome".to_owned(),
                description: String::new(),
                response: "third".to_owned(),
            },
            Mee6CommandInput {
                command: format!("{long}!"),
                description: String::new(),
                response: "long first".to_owned(),
            },
            Mee6CommandInput {
                command: long.clone(),
                description: String::new(),
                response: "long second".to_owned(),
            },
        ]);
        assert_eq!(out.conflicts, vec!["faq".to_owned(), long.clone()]);
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "faq",
                "faq-2",
                "welcome",
                long.as_str(),
                &format!("{}-2", "a".repeat(30))
            ]
        );
        assert_eq!(
            out.commands
                .iter()
                .map(|c| c.text_trigger.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("!faq".to_owned()),
                None,
                Some("!welcome".to_owned()),
                Some(format!("!{long}")),
                None,
            ]
        );
    }
}
