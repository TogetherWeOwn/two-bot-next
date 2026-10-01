//! Shared bounds for user-controlled text rendered into Discord messages.

pub const CONTENT_LIMIT: usize = 2000;
pub const EMBED_TITLE_LIMIT: usize = 256;
pub const EMBED_DESCRIPTION_LIMIT: usize = 4096;
pub const EMBED_FIELD_NAME_LIMIT: usize = 256;
pub const EMBED_FIELD_VALUE_LIMIT: usize = 1024;
pub const EMBED_FOOTER_LIMIT: usize = 2048;
pub const EMBED_AUTHOR_LIMIT: usize = 256;
pub const EMBED_TOTAL_LIMIT: usize = 6000;
pub const EMBED_FIELD_LIMIT: usize = 25;
pub const EMBED_LIMIT: usize = 10;

/// Count UTF-16 code units, matching legacy JavaScript bounds. This is also
/// conservative for Discord's character limits and never splits a UTF-8 scalar.
pub fn text_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

pub fn truncate(text: &str, limit: usize) -> String {
    let mut remaining = limit;
    text.chars()
        .take_while(|ch| {
            let len = ch.len_utf16();
            if len > remaining {
                return false;
            }
            remaining -= len;
            true
        })
        .collect()
}

/// Neutralize mass mentions, normalizing invisible separators only inside a
/// matched mention. Preserve other Unicode (including meaningful non-joiners).
/// Role/user mentions are kept readable: the REST boundary disables their parsing.
pub fn neutralize_mentions(text: &str) -> String {
    let mut safe = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('@') {
        safe.push_str(&rest[..start]);
        safe.push('@');
        rest = &rest[start + 1..];
        for mention in ["everyone", "here"] {
            if let Some(end) = mention_end(rest, mention) {
                safe.push('\u{200b}');
                safe.push_str(mention);
                rest = &rest[end..];
                break;
            }
        }
    }
    safe.push_str(rest);
    safe
}

fn mention_end(text: &str, mention: &str) -> Option<usize> {
    let mut chars = text
        .char_indices()
        .filter(|(_, ch)| !invisible_separator(*ch));
    let mut end = 0;
    for expected in mention.chars() {
        let (index, ch) = chars.next()?;
        if ch != expected {
            return None;
        }
        end = index + ch.len_utf8();
    }
    Some(end)
}

fn invisible_separator(ch: char) -> bool {
    matches!(ch, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}')
}

/// A standalone invisible/whitespace-only body cannot carry a text message.
/// Test the effective, bounded rendering; do not remove joiners from real text.
pub fn has_message_text(text: &str) -> bool {
    text.chars()
        .any(|ch| !ch.is_whitespace() && !invisible_separator(ch))
}

pub fn content(text: &str) -> String {
    truncate(&neutralize_mentions(text), CONTENT_LIMIT)
}

/// Render custom-command placeholders, then sanitize the complete output.
/// Binding names omit braces (for example `user`, `username`, `server`,
/// `channel`). Replacement values are text, never a second template.
pub fn render_template(template: &str, bindings: &[(&str, &str)]) -> String {
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        rendered.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let name = &rest[1..end];
        match bindings.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => rendered.push_str(value),
            None => rendered.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    rendered.push_str(rest);
    content(&rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mass_mentions_are_neutralized_and_replay_is_idempotent() {
        let text = "@everyone @here <@&123> <@456> @\u{200b}everyone @\u{200c}here";
        let safe = content(text);
        assert!(!safe.contains("@everyone"));
        assert!(!safe.contains("@here"));
        assert!(safe.contains("<@&123>"));
        assert_eq!(content(&safe), safe);
    }

    #[test]
    fn nonmention_unicode_is_preserved_and_obfuscated_mentions_are_neutralized() {
        for text in [
            "می\u{200c}روم",
            "👩\u{200d}💻",
            "line\u{200b}break",
            "\u{feff}text",
            "@user\u{200c}name",
        ] {
            assert_eq!(content(text), text);
        }
        for separator in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'] {
            for mention in ["everyone", "here"] {
                let disguised = format!(
                    "@{separator}{}",
                    mention
                        .chars()
                        .map(|ch| format!("{ch}{separator}"))
                        .collect::<String>()
                );
                let expected = format!("@\u{200b}{mention}{separator}");
                assert_eq!(content(&disguised), expected);
                assert_eq!(content(&expected), expected);
            }
        }
    }

    #[test]
    fn bounds_preserve_multibyte_scalars_at_exact_limits() {
        for limit in [
            CONTENT_LIMIT,
            EMBED_TITLE_LIMIT,
            EMBED_DESCRIPTION_LIMIT,
            EMBED_FIELD_NAME_LIMIT,
            EMBED_FIELD_VALUE_LIMIT,
            EMBED_FOOTER_LIMIT,
            EMBED_AUTHOR_LIMIT,
            EMBED_TOTAL_LIMIT,
            300,
        ] {
            let text = "界".repeat(limit);
            assert_eq!(truncate(&text, limit), text);
            assert_eq!(truncate(&(text.clone() + "界"), limit), text);
            let text = "😀".repeat(limit / 2);
            assert_eq!(truncate(&text, limit), text);
            assert_eq!(truncate(&(text.clone() + "😀"), limit), text);
            assert_eq!(
                truncate(&("a".repeat(limit - 1) + "😀"), limit),
                "a".repeat(limit - 1)
            );
        }
        assert_eq!(truncate("😀", 0), "");
        assert_eq!(truncate("😀", 1), "");
        assert_eq!(truncate("😀", 2), "😀");
    }

    #[test]
    fn neutralization_happens_before_the_content_bound() {
        let safe = content(&("界".repeat(CONTENT_LIMIT - 9) + "@everyone"));
        assert_eq!(text_len(&safe), CONTENT_LIMIT);
        assert!(!safe.contains("@everyone"));
        assert_eq!(content(&safe), safe);
    }
}
