//! Bounded chronological transcript capture from newest-first Discord history.
//! Pagination must continue after truncation: every message still needs validation
//! and contributes to the count. The caller sorts each page by numeric ID DESC
//! and checks strict cursor progress; no history-sized ID set is needed here.

use std::collections::VecDeque;

use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_core::{
    funnel::format_iso_millis,
    tickets::{TranscriptSnapshot, MAX_TRANSCRIPT_UTF16_UNITS},
};

const TRUNCATION_MARKER: &str = "\n[transcript truncated]";

#[derive(Default)]
pub(super) struct HistoryCapture {
    // Chunks are message lines, including their separator before a newer line.
    // Only the newest retained chunk can be a partial line. Box<str> releases
    // removed tails rather than retaining their former String allocations.
    lines: VecDeque<Line>,
    units: usize,
    previous: Option<(u64, i64)>,
    message_count: i32,
    truncated: bool,
}

struct Line {
    text: Box<str>,
    units: usize,
}

impl HistoryCapture {
    pub(super) fn push(&mut self, id: u64, message: &Value) -> Result<(), ()> {
        let timestamp = message.get("timestamp").and_then(Value::as_str).ok_or(())?;
        // RFC3339 parsing rejects impossible dates and malformed UTF-8-safe
        // offsets without the permissive legacy parser's unchecked arithmetic.
        let created_at = OffsetDateTime::parse(timestamp.trim(), &Rfc3339)
            .map_err(|_| ())?
            .unix_timestamp_nanos()
            .div_euclid(1_000_000);
        let created_at = i64::try_from(created_at).map_err(|_| ())?;
        if id == 0
            || self
                .previous
                .is_some_and(|(newer_id, newer_at)| id >= newer_id || created_at > newer_at)
        {
            return Err(());
        }

        let author = message.get("author").and_then(Value::as_object).ok_or(())?;
        let username = author.get("username").and_then(Value::as_str).ok_or(())?;
        let discriminator = match author.get("discriminator") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_str().ok_or(())?),
        };
        let content = message.get("content").and_then(Value::as_str).ok_or(())?;
        let attachments = message
            .get("attachments")
            .and_then(Value::as_array)
            .ok_or(())?;
        // Validate ALL URLs even if a previous component fills the line budget.
        for attachment in attachments {
            attachment.get("url").and_then(Value::as_str).ok_or(())?;
        }

        let mut line = BoundedLine::default();
        line.push("[");
        line.push(&format_iso_millis(created_at));
        line.push("] ");
        line.push(username);
        if let Some(discriminator) = discriminator.filter(|value| *value != "0") {
            line.push("#");
            line.push(discriminator);
        }
        line.push(":");

        // Match domain format!("[ISO] author: content urls").trim() without
        // first allocating a potentially enormous formatted line. Its opening
        // '[' prevents leading trim; trim trailing whitespace on the final
        // non-whitespace component, preserving all earlier spacing verbatim.
        let last_url = attachments.iter().rposition(|attachment| {
            !attachment["url"]
                .as_str()
                .expect("validated URL")
                .trim_end()
                .is_empty()
        });
        if let Some(last_url) = last_url {
            line.push(" ");
            line.push(content);
            for (index, attachment) in attachments.iter().take(last_url + 1).enumerate() {
                line.push(" ");
                let url = attachment["url"].as_str().expect("validated URL");
                line.push(if index == last_url {
                    url.trim_end()
                } else {
                    url
                });
            }
        } else if !content.trim_end().is_empty() {
            line.push(" ");
            line.push(content.trim_end());
        }
        if self.previous.is_some() {
            line.push("\n");
        }

        if line.overflow {
            // The prefix ends inside this older line (or at its separator).
            // Spare capacity left by a two-unit scalar must NOT be filled with
            // newer text: that would skip the scalar and no longer be a prefix.
            self.lines.clear();
            self.units = 0;
            self.truncated = true;
        } else {
            self.trim_newest_to(MAX_TRANSCRIPT_UTF16_UNITS - line.units);
        }
        self.units += line.units;
        self.lines.push_front(Line {
            text: line.text.into_boxed_str(),
            units: line.units,
        });
        self.previous = Some((id, created_at));
        self.message_count = self.message_count.saturating_add(1);
        Ok(())
    }

    fn trim_newest_to(&mut self, budget: usize) {
        while self.units > budget {
            let tail = self.lines.pop_back().expect("nonempty retained prefix");
            self.units -= tail.units;
            self.truncated = true;
            if self.units < budget {
                // Copy only the scalar-safe retained prefix. Dropping the old
                // Box releases the removed bytes, not just its logical length.
                let (bytes, units) = prefix(&tail.text, budget - self.units);
                if units != 0 {
                    self.lines.push_back(Line {
                        text: tail.text[..bytes].into(),
                        units,
                    });
                    self.units += units;
                }
            }
        }
    }

    pub(super) fn finish(self) -> TranscriptSnapshot {
        let bytes: usize = self.lines.iter().map(|line| line.text.len()).sum();
        let mut content = String::with_capacity(
            bytes
                + if self.truncated {
                    TRUNCATION_MARKER.len()
                } else {
                    0
                },
        );
        for line in self.lines {
            content.push_str(&line.text);
        }
        if self.truncated {
            content.push_str(TRUNCATION_MARKER);
        }
        TranscriptSnapshot {
            content,
            message_count: self.message_count,
        }
    }
}

/// Formats only a scalar-safe prefix, never a full unbounded message line.
#[derive(Default)]
struct BoundedLine {
    text: String,
    units: usize,
    overflow: bool,
}

impl BoundedLine {
    fn push(&mut self, part: &str) {
        if self.overflow {
            return;
        }
        let (bytes, units) = prefix(part, MAX_TRANSCRIPT_UTF16_UNITS - self.units);
        self.text.push_str(&part[..bytes]);
        self.units += units;
        self.overflow = bytes < part.len();
    }
}

fn prefix(text: &str, budget: usize) -> (usize, usize) {
    let mut bytes = 0;
    let mut units = 0;
    for ch in text.chars() {
        if units + ch.len_utf16() > budget {
            break;
        }
        bytes += ch.len_utf8();
        units += ch.len_utf16();
    }
    (bytes, units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use two_bot_core::tickets::{format_transcript, TranscriptMessage};

    const AT: &str = "2026-10-01T00:00:00.000Z";

    fn message(content: &str) -> Value {
        json!({
            "timestamp": AT,
            "author": {"username": "author", "discriminator": "0123"},
            "content": content,
            "attachments": [],
        })
    }

    fn domain(message: &Value) -> TranscriptMessage {
        let author = &message["author"];
        let username = author["username"].as_str().unwrap();
        let author_tag = match author["discriminator"].as_str() {
            Some(discriminator) if discriminator != "0" => format!("{username}#{discriminator}"),
            _ => username.to_owned(),
        };
        TranscriptMessage {
            created_at: two_bot_core::funnel::parse_iso_millis(
                message["timestamp"].as_str().unwrap(),
            )
            .unwrap(),
            author_tag,
            content: message["content"].as_str().unwrap().to_owned(),
            attachment_urls: message["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|attachment| attachment["url"].as_str().unwrap().to_owned())
                .collect(),
        }
    }

    fn assert_bounded(capture: &HistoryCapture) {
        let units: usize = capture.lines.iter().map(|line| line.units).sum();
        let bytes: usize = capture.lines.iter().map(|line| line.text.len()).sum();
        assert_eq!(capture.units, units);
        assert!(units <= MAX_TRANSCRIPT_UTF16_UNITS);
        // UTF-8 requires at most three bytes per UTF-16 unit; boxed strings
        // have no hidden retained capacity. Deque metadata is also cap-bounded.
        assert!(bytes <= 3 * MAX_TRANSCRIPT_UTF16_UNITS);
        assert!(capture.lines.capacity() <= 2 * MAX_TRANSCRIPT_UTF16_UNITS);
        let allocation = bytes + capture.lines.capacity() * std::mem::size_of::<Line>();
        assert!(allocation <= (3 + 2 * std::mem::size_of::<Line>()) * MAX_TRANSCRIPT_UTF16_UNITS);
        for line in &capture.lines {
            assert_eq!(line.units, line.text.encode_utf16().count());
        }
    }

    #[test]
    fn many_large_messages_keep_only_bounded_oldest_prefix_and_total_count() {
        let large = message(&"界".repeat(3 * MAX_TRANSCRIPT_UTF16_UNITS));
        let mut capture = HistoryCapture::default();
        for id in (1..=256).rev() {
            capture.push(id, &large).unwrap();
            assert_bounded(&capture);
            assert_eq!(capture.lines.len(), 1);
        }
        let snapshot = capture.finish();
        assert_eq!(snapshot.message_count, 256);
        assert!(snapshot.content.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            snapshot
                .content
                .strip_suffix(TRUNCATION_MARKER)
                .unwrap()
                .encode_utf16()
                .count(),
            MAX_TRANSCRIPT_UTF16_UNITS,
        );
        assert!(snapshot.content.len() <= 3 * MAX_TRANSCRIPT_UTF16_UNITS + TRUNCATION_MARKER.len());

        // Author/discriminator/URL fields must not cause unbounded per-line
        // formatting either, even when content is small.
        for field in ["username", "discriminator", "url"] {
            let mut oversized = message("");
            let large_field = Value::String("界".repeat(3 * MAX_TRANSCRIPT_UTF16_UNITS));
            if field == "url" {
                oversized["attachments"] = json!([{"url": large_field}]);
            } else {
                oversized["author"][field] = large_field;
            }
            let mut capture = HistoryCapture::default();
            capture.push(1, &oversized).unwrap();
            assert_bounded(&capture);
            assert!(capture.truncated);
        }
        let mut writer = BoundedLine::default();
        writer.push(&"界".repeat(3 * MAX_TRANSCRIPT_UTF16_UNITS));
        writer.push(&"🦀".repeat(MAX_TRANSCRIPT_UTF16_UNITS));
        assert_eq!(writer.units, MAX_TRANSCRIPT_UTF16_UNITS);
        assert!(writer.text.capacity() <= 6 * MAX_TRANSCRIPT_UTF16_UNITS);
    }

    #[test]
    fn many_small_messages_bound_deque_metadata_and_release_evicted_tails() {
        let mut capture = HistoryCapture::default();
        let small = message("");
        for id in (2..=20_001).rev() {
            capture.push(id, &small).unwrap();
        }
        assert_bounded(&capture);
        assert!(capture.truncated);
        assert!(capture.lines.len() < 20_000);
        // Replace thousands of small lines with one huge oldest line; their
        // data allocations disappear even though deque metadata may be reused.
        capture
            .push(1, &message(&"x".repeat(MAX_TRANSCRIPT_UTF16_UNITS)))
            .unwrap();
        assert_bounded(&capture);
        assert_eq!(capture.lines.len(), 1);
        let snapshot = capture.finish();
        assert_eq!(snapshot.message_count, 20_001);
    }

    #[test]
    fn exact_cap_unicode_and_attachment_format_match_domain_trim_semantics() {
        let mut wire = message("");
        wire["attachments"] = json!([
            {"url": ""},
            {"url": " https://cdn.example/🦀  "},
            {"url": "\u{2003}\t"},
        ]);
        let overhead = format_transcript(vec![domain(&wire)])
            .content
            .encode_utf16()
            .count();
        let budget = MAX_TRANSCRIPT_UTF16_UNITS - overhead;
        wire["content"] = Value::String(format!(
            "{}{}",
            "🦀".repeat(budget / 2),
            "é".repeat(budget % 2)
        ));
        let expected = format_transcript(vec![domain(&wire)]);
        assert_eq!(
            expected.content.encode_utf16().count(),
            MAX_TRANSCRIPT_UTF16_UNITS
        );
        let mut capture = HistoryCapture::default();
        capture.push(1, &wire).unwrap();
        assert_bounded(&capture);
        assert_eq!(capture.finish(), expected);

        for (content, urls, discriminator) in [
            ("  text \n ", json!([]), json!("0")),
            (
                " \t\u{2003}",
                json!([{"url": " "}, {"url": "\t"}]),
                Value::Null,
            ),
            (
                "  text \n ",
                json!([{"url": "url "}, {"url": " url2  "}]),
                json!("1234"),
            ),
        ] {
            let mut wire = message(content);
            wire["attachments"] = urls;
            wire["author"]["discriminator"] = discriminator;
            let mut capture = HistoryCapture::default();
            capture.push(1, &wire).unwrap();
            assert_eq!(capture.finish(), format_transcript(vec![domain(&wire)]));
        }
    }

    #[test]
    fn scalar_boundary_does_not_skip_emoji_to_fill_spare_unit_with_newer_text() {
        let header = format_transcript(vec![domain(&message(""))]).content;
        // Nonempty content adds one space after ':'. Leave one unit before
        // an emoji, which cannot fit; no newer separator/text may fill it.
        let content = format!(
            "{}🦀",
            "x".repeat(MAX_TRANSCRIPT_UTF16_UNITS - header.len() - 2)
        );
        let older = message(&content);
        let newer = message("newer");
        let mut capture = HistoryCapture::default();
        capture.push(2, &newer).unwrap();
        capture.push(1, &older).unwrap();
        assert_bounded(&capture);
        let snapshot = capture.finish();
        assert_eq!(
            snapshot,
            format_transcript(vec![domain(&older), domain(&newer)])
        );
        assert_eq!(
            snapshot
                .content
                .strip_suffix(TRUNCATION_MARKER)
                .unwrap()
                .encode_utf16()
                .count(),
            MAX_TRANSCRIPT_UTF16_UNITS - 1,
        );
    }

    #[test]
    fn equal_millisecond_ids_reverse_to_oldest_first_across_page_boundary() {
        let mut capture = HistoryCapture::default();
        let pages = [vec![105, 104, 103], vec![102, 101]];
        for page in pages {
            for id in page {
                capture
                    .push(id, &message(&format!("message {id}")))
                    .unwrap();
            }
        }
        let expected = format_transcript(
            (101..=105)
                .map(|id| domain(&message(&format!("message {id}"))))
                .collect(),
        );
        assert_eq!(capture.finish(), expected);

        let mut capture = HistoryCapture::default();
        for id in [3, 2, 1] {
            capture
                .push(id, &message(&format!("{id}{}", "x".repeat(100_000))))
                .unwrap();
        }
        assert_bounded(&capture);
        let snapshot = capture.finish();
        let expected = format_transcript(
            (1..=3)
                .map(|id| domain(&message(&format!("{id}{}", "x".repeat(100_000)))))
                .collect(),
        );
        assert_eq!(snapshot, expected);
        assert_eq!(snapshot.message_count, 3);
    }

    #[test]
    fn mixed_unicode_partial_tail_eviction_matches_domain_after_each_prepend() {
        let mut capture = HistoryCapture::default();
        let mut oldest_first = VecDeque::new();
        for id in (1..=80).rev() {
            let mut wire = message(&format!(" {id}\n{}  ", "界🦀é".repeat(1_000)));
            wire["timestamp"] = json!(format_iso_millis(id as i64));
            if id % 3 == 0 {
                wire["author"]["discriminator"] = json!("0");
            }
            if id % 2 == 0 {
                wire["attachments"] = json!([
                    {"url": "https://cdn.example/界🦀é "},
                    {"url": " https://cdn.example/second\t"},
                ]);
            }
            capture.push(id, &wire).unwrap();
            oldest_first.push_front(domain(&wire));
            assert_bounded(&capture);
            // This oracle materializes only the bounded test fixture, not live
            // history. Compare every cutoff, including partial UTF-16 scalars.
            let expected = format_transcript(oldest_first.iter().cloned().collect());
            let mut retained = String::new();
            for line in &capture.lines {
                retained.push_str(&line.text);
            }
            if capture.truncated {
                retained.push_str(TRUNCATION_MARKER);
            }
            assert_eq!(retained, expected.content, "after ID {id}");
            assert_eq!(capture.message_count, expected.message_count);
        }
    }

    #[test]
    fn malformed_evidence_is_rejected_even_after_cap_without_mutating_capture() {
        let cases = [
            ("timestamp", json!("garbage")),
            ("timestamp", json!("2026-02-30T00:00:00Z")),
            ("timestamp", json!("2026-10-01T00:00:00+é:00")),
            ("timestamp", json!("99999999999999999999-10-01T00:00:00Z")),
            ("timestamp", json!(123)),
            ("timestamp", Value::Null),
            ("content", json!(12)),
            ("content", Value::Null),
            ("author", json!("author")),
            ("author", json!({"username": 12})),
            (
                "author",
                json!({"username": "author", "discriminator": 1234}),
            ),
            ("attachments", Value::Null),
            ("attachments", json!({})),
            ("attachments", json!([{}])),
            ("attachments", json!([{"url": 123}])),
            ("attachments", json!([{"url": "valid"}, {"url": null}])),
        ];
        for (field, value) in cases {
            let mut capture = HistoryCapture::default();
            capture
                .push(3, &message(&"x".repeat(MAX_TRANSCRIPT_UTF16_UNITS)))
                .unwrap();
            let mut malformed = message("ignored?");
            malformed[field] = value;
            assert_eq!(capture.push(2, &malformed), Err(()), "{field}");
            assert_eq!(capture.message_count, 1);
            assert_eq!(capture.previous.unwrap().0, 3);
            assert_bounded(&capture);
            capture.push(2, &message("valid older message")).unwrap();
            assert_eq!(capture.finish().message_count, 2);
        }
        for field in ["timestamp", "author", "content", "attachments"] {
            let mut wire = message("text");
            wire.as_object_mut().unwrap().remove(field);
            assert_eq!(HistoryCapture::default().push(1, &wire), Err(()));
        }
    }

    #[test]
    fn inconsistent_timestamp_order_and_non_decreasing_ids_are_rejected() {
        let mut capture = HistoryCapture::default();
        let newer = message("newer");
        capture.push(3, &newer).unwrap();
        let mut inconsistent = message("older ID with newer timestamp");
        inconsistent["timestamp"] = json!("2026-10-01T00:00:00.001Z");
        assert_eq!(capture.push(2, &inconsistent), Err(()));
        assert_eq!(capture.push(3, &newer), Err(()));
        assert_eq!(capture.push(4, &newer), Err(()));
        let mut older = message("older");
        older["timestamp"] = json!("2026-09-30T23:59:59.999Z");
        capture.push(2, &older).unwrap();
        assert_eq!(
            capture.finish(),
            format_transcript(vec![domain(&older), domain(&newer)])
        );
    }

    #[test]
    fn empty_history_and_saturating_message_count() {
        assert_eq!(
            HistoryCapture::default().finish(),
            format_transcript(vec![])
        );
        let mut capture = HistoryCapture {
            message_count: i32::MAX - 1,
            ..HistoryCapture::default()
        };
        capture.push(2, &message("newer")).unwrap();
        capture.push(1, &message("older")).unwrap();
        assert_eq!(capture.finish().message_count, i32::MAX);
    }
}
