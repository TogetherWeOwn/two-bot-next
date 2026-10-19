//! Acceptance tests: `/rank` replies render hostile display names inert
//! (pure, offline, no Discord call).
//!
//! Failing-first contract for the display-name slot of
//! `two_bot_core::leveling::rank_reply` — the real path the runtime rank arm
//! forwards verbatim with the ephemeral flag and no mention suppression. The
//! slot gets the same vote-reason-grade treatment the goodbye notice already
//! ships (`goodbye_hostile_acceptance`): single-line, mention-neutralized,
//! markdown-escaped, length-capped. Normal names render byte-identical to
//! before, and every reply stays ephemeral.
//!
//! Contract bounds pinned here, each traceable to an existing public
//! contract rather than to the implementation under change:
//!
//! - Mention neutralization is the shared `message_safety` neutralizer:
//!   `@everyone` becomes `@` + U+200B + `everyone`, including zero-width
//!   obfuscations, and `message_safety::contains_mass_mention` reports the
//!   rendered reply clean. Mention pills (`<@`, `<#`, `<:`, `<a:`) are split
//!   with U+200B so they render literally.
//! - Inline markdown is backslash-escaped for the vote-reason set
//!   (`\`, backtick, `*`, `_`, `~`, `|`, `[`, `]`); a scanner below proves no
//!   unescaped markdown-active character survives in the slot, so any
//!   superset escaper also passes.
//! - Newlines fold to spaces: the name stays on the reply's first line, the
//!   reply keeps exactly its three template lines, and line-leading markup
//!   (headings, quotes, lists, fences) can never start from the slot.
//! - The rendered slot is capped at 512 characters — the vote-grade public
//!   bound (`VOTE_KICK_PUBLIC_REASON_LIMIT`, also `GOODBYE_USERNAME_LIMIT`)
//!   — applied to the escaped output so the bound holds on the wire, and the
//!   full reply always fits `message_safety::CONTENT_LIMIT`.
//!
//! Exact byte pins are used only where every grade-conforming treatment
//! agrees (normal names, the shared neutralizer's canonical output, simple
//! escapes, the ASCII cap). Everything else asserts the inertness
//! properties above. Link-splitting (`://`, bare domains) is deliberately
//! not pinned: it is outside the four-property grade, and no input below is
//! shaped so that a Builder who ports it would fail.
//!
//! Current `rank_text` interpolates the display name raw, so the hostile
//! tests FAIL today for exactly that reason — the raw-interpolation mutant
//! is the code as shipped. The normal-name and ephemeral pins pass today
//! and must keep passing byte-identical. Test files only; no runtime code
//! is touched.

use two_bot_core::leveling::{rank_reply, total_xp_for_level, LevelProfile, RankReply};
use two_bot_core::message_safety::{contains_mass_mention, text_len, CONTENT_LIMIT};

/// Vote-grade public bound on the rendered slot, in characters.
const SLOT_CAP: usize = 512;

/// The two fixed template lines for [`ranked_profile`].
const RANKED_TAIL: &str =
    "Level **1** · Rank **#3** of **50**\nXP **114** · 14/155 this level · **141** to level 2";
/// The two fixed template lines for [`unranked_profile`].
const UNRANKED_TAIL: &str =
    "Level **0** · Rank **Unranked** (no XP recorded)\nXP **0** · 0/100 this level · **100** to level 1";

fn ranked_profile() -> LevelProfile {
    LevelProfile {
        guild_id: "g".to_owned(),
        member_id: "100000000000000001".to_owned(),
        xp: 114,
        level: 1,
        message_xp: 114,
        voice_xp: 0,
        imported_xp: 0,
        rank: Some(3),
        member_count: 50,
        next_level_xp: total_xp_for_level(2),
    }
}

fn unranked_profile() -> LevelProfile {
    LevelProfile {
        guild_id: "g".to_owned(),
        member_id: "100000000000000001".to_owned(),
        xp: 0,
        level: 0,
        message_xp: 0,
        voice_xp: 0,
        imported_xp: 0,
        rank: None,
        member_count: 2,
        next_level_xp: total_xp_for_level(1),
    }
}

/// The ranked reply for `name`: the rendered body plus the ephemeral pin
/// every `/rank` answer must keep, hostile name or not.
fn ranked(name: &str) -> String {
    let reply: RankReply = rank_reply(&ranked_profile(), name);
    assert!(reply.ephemeral, "/rank answers ephemerally: {reply:?}");
    reply.content
}

/// The unranked reply for `name`, with the same ephemeral pin.
fn unranked(name: &str) -> String {
    let reply: RankReply = rank_reply(&unranked_profile(), name);
    assert!(reply.ephemeral, "/rank answers ephemerally: {reply:?}");
    reply.content
}

/// The rendered slot: the first line between the template's bold markers.
/// Taking only the first line means a hostile name that smuggles in extra
/// lines shifts the template and fails the tail assertion in
/// [`assert_inert`].
fn slot<'a>(out: &'a str, tail: &str) -> &'a str {
    assert!(out.ends_with(tail), "reply must keep its tail: {out:?}");
    let first = out.lines().next().unwrap_or("");
    assert!(first.starts_with("**"), "reply must open bold: {out:?}");
    assert!(first.ends_with("**"), "first line must close bold: {out:?}");
    assert!(first.len() >= 4, "slot boundary inside bold: {out:?}");
    &first[2..first.len() - 2]
}

/// No markdown-active character from the vote-reason escape set may survive
/// unescaped in the slot. A backslash escapes the next character.
fn assert_no_unescaped_markdown(rendered_slot: &str) {
    let mut escaped = false;
    for ch in rendered_slot.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        assert!(
            !matches!(ch, '`' | '*' | '_' | '~' | '|' | '[' | ']'),
            "unescaped markdown {ch:?} survives in slot {rendered_slot:?}"
        );
    }
}

/// The full inertness battery every hostile render must satisfy.
fn assert_inert(out: &str, tail: &str) {
    assert_eq!(
        out.lines().count(),
        3,
        "reply must keep its three template lines: {out:?}"
    );
    assert!(!out.contains('\r'), "reply carries a raw CR: {out:?}");
    assert!(
        !contains_mass_mention(out),
        "reply carries a live mass mention: {out:?}"
    );
    assert!(!out.contains("@everyone"), "raw @everyone: {out:?}");
    assert!(!out.contains("@here"), "raw @here: {out:?}");
    for pill in ["<@", "<#", "<:", "<a:"] {
        assert!(!out.contains(pill), "live mention pill {pill:?}: {out:?}");
    }
    assert!(
        text_len(out) <= CONTENT_LIMIT,
        "reply exceeds the Discord content bound: {} chars",
        text_len(out)
    );
    let rendered = slot(out, tail);
    assert!(
        rendered.chars().count() <= SLOT_CAP,
        "slot exceeds the {SLOT_CAP}-char cap: {} chars",
        rendered.chars().count()
    );
    assert_no_unescaped_markdown(rendered);
}

// --- Pins that pass today: normal names, ephemeral, byte-identical --------

#[test]
fn benign_ranked_reply_is_byte_identical_and_ephemeral() {
    let reply = rank_reply(&ranked_profile(), "Test");
    assert!(reply.ephemeral, "legacy answers /rank ephemerally");
    assert_eq!(reply.content, format!("**Test**\n{RANKED_TAIL}"));
}

#[test]
fn benign_unranked_reply_is_byte_identical_and_ephemeral() {
    let reply = rank_reply(&unranked_profile(), "New Member");
    assert!(reply.ephemeral, "legacy answers /rank ephemerally");
    assert_eq!(reply.content, format!("**New Member**\n{UNRANKED_TAIL}"));
}

#[test]
fn normal_unicode_and_punctuation_names_pass_through_unchanged() {
    // None of these carry a mass mention, pill, markdown-active character,
    // newline or overlong run, so the slot must be the name verbatim —
    // including multilingual joiners and a non-mention `@`, which the shared
    // safety contract explicitly preserves.
    for name in [
        "space cadet",
        "mary-jane",
        "o'brien",
        "user42",
        "界界",
        "👩\u{200d}💻",
        "می\u{200c}روم",
        "@user\u{200c}name",
    ] {
        assert_eq!(
            ranked(name),
            format!("**{name}**\n{RANKED_TAIL}"),
            "normal name must render byte-identical: {name:?}"
        );
    }
}

#[test]
fn empty_name_renders_an_empty_slot() {
    assert_eq!(ranked(""), format!("****\n{RANKED_TAIL}"));
}

// --- RED today: hostile names must render inert ---------------------------

#[test]
fn mass_mentions_are_neutralized_exactly() {
    assert_eq!(
        ranked("@everyone"),
        format!("**@\u{200b}everyone**\n{RANKED_TAIL}")
    );
    assert_eq!(ranked("@here"), format!("**@\u{200b}here**\n{RANKED_TAIL}"));
    assert_inert(&ranked("@everyone @here"), RANKED_TAIL);
}

#[test]
fn zero_width_obfuscated_mass_mentions_are_neutralized() {
    for separator in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}'] {
        let disguised = format!("@{separator}everyone");
        let out = ranked(&disguised);
        assert_eq!(
            out,
            format!("**@\u{200b}everyone**\n{RANKED_TAIL}"),
            "obfuscated mention must canonicalize inert: {disguised:?}"
        );
        assert_inert(&out, RANKED_TAIL);
    }
}

#[test]
fn mention_pills_are_split_exactly() {
    assert_eq!(
        ranked("<@44> <@&77>"),
        format!("**<\u{200b}@44> <\u{200b}@&77>**\n{RANKED_TAIL}")
    );
    assert_eq!(ranked("<#13>"), format!("**<\u{200b}#13>**\n{RANKED_TAIL}"));
    assert_eq!(
        ranked("<:wave:99>"),
        format!("**<\u{200b}:wave:99>**\n{RANKED_TAIL}")
    );
    assert_eq!(
        ranked("<a:wave:99>"),
        format!("**<\u{200b}a:wave:99>**\n{RANKED_TAIL}")
    );
}

#[test]
fn markdown_bold_and_code_are_escaped_exactly() {
    assert_eq!(
        ranked("**pwned**"),
        r"**\*\*pwned\*\***".to_string() + "\n" + RANKED_TAIL
    );
    assert_eq!(
        ranked("`code`"),
        r"**\`code\`**".to_string() + "\n" + RANKED_TAIL
    );
    assert_inert(&ranked("***x*** ```y```"), RANKED_TAIL);
}

#[test]
fn markdown_italic_spoiler_strike_and_link_chars_are_escaped() {
    assert_eq!(
        ranked("_under_"),
        r"**\_under\_**".to_string() + "\n" + RANKED_TAIL
    );
    assert_eq!(
        ranked("||secret||"),
        r"**\|\|secret\|\|**".to_string() + "\n" + RANKED_TAIL
    );
    assert_eq!(
        ranked("~~gone~~"),
        r"**\~\~gone\~\~**".to_string() + "\n" + RANKED_TAIL
    );
    assert_eq!(
        ranked("[click](target)"),
        r"**\[click\](target)**".to_string() + "\n" + RANKED_TAIL
    );
}

#[test]
fn backslashes_are_escaped_so_escapes_cannot_be_forged() {
    // A raw `\*` in the slot reads as an escape to Discord; the backslash
    // itself must be escaped so an attacker cannot pre-forge one.
    assert_eq!(ranked(r"a\b"), r"**a\\b**".to_string() + "\n" + RANKED_TAIL);
    assert_eq!(
        ranked(r"\*pwn"),
        r"**\\\*pwn**".to_string() + "\n" + RANKED_TAIL
    );
}

#[test]
fn newlines_fold_to_the_first_line_exactly() {
    assert_eq!(
        ranked("line1\nline2"),
        format!("**line1 line2**\n{RANKED_TAIL}")
    );
}

#[test]
fn crlf_and_cr_fold_to_the_first_line() {
    let out = ranked("a\r\nb\rc");
    assert_inert(&out, RANKED_TAIL);
    let rendered = slot(&out, RANKED_TAIL);
    assert!(rendered.contains('a'), "fold keeps the text: {out:?}");
    assert!(rendered.contains('b'), "fold keeps the text: {out:?}");
    assert!(rendered.contains('c'), "fold keeps the text: {out:?}");
}

#[test]
fn newline_heading_quote_list_and_fence_battery_stays_on_line_one() {
    let out = ranked("x\n# fake heading\n> quote\n- item\n```code```");
    assert_inert(&out, RANKED_TAIL);
    assert!(
        out.contains("fake heading"),
        "text stays readable, just inert: {out:?}"
    );
}

#[test]
fn hostile_composite_renders_inert() {
    let out = ranked("**pwned**\n# fake heading\n@everyone");
    assert_eq!(
        out,
        format!("**\\*\\*pwned\\*\\* # fake heading @\u{200b}everyone**\n{RANKED_TAIL}")
    );
    assert_inert(&out, RANKED_TAIL);
}

#[test]
fn hostile_name_keeps_level_rank_and_xp_lines() {
    let out = ranked("@everyone <@44> <@&77>");
    assert_inert(&out, RANKED_TAIL);
    assert!(
        out.ends_with(RANKED_TAIL),
        "level/rank/XP lines must survive sanitizing: {out:?}"
    );
}

#[test]
fn hostile_name_for_unranked_member_renders_inert() {
    // `/rank member:` renders another member's name through the same path.
    let out = unranked("@everyone\n**pwned** <@44>");
    assert_inert(&out, UNRANKED_TAIL);
    assert!(
        out.ends_with(UNRANKED_TAIL),
        "unranked lines must survive sanitizing: {out:?}"
    );
}

#[test]
fn overlong_name_is_capped_at_the_vote_grade_limit_exactly() {
    let expected_slot = "a".repeat(SLOT_CAP);
    assert_eq!(
        ranked(&"a".repeat(600)),
        format!("**{expected_slot}**\n{RANKED_TAIL}")
    );
    // The wire size does not grow with the input length.
    assert_eq!(
        ranked(&"a".repeat(10_000)),
        format!("**{expected_slot}**\n{RANKED_TAIL}")
    );
}

#[test]
fn overlong_markdown_name_stays_capped_after_escaping() {
    // The cap binds the escaped output, so 600 asterisks cannot expand to a
    // 1200-char slot of `\*` pairs.
    let out = ranked(&"*".repeat(600));
    assert_inert(&out, RANKED_TAIL);
    assert!(
        slot(&out, RANKED_TAIL).chars().count() <= SLOT_CAP,
        "escaped slot must respect the cap: {} chars",
        slot(&out, RANKED_TAIL).chars().count()
    );
}

#[test]
fn overlong_multibyte_name_is_capped_on_a_char_boundary() {
    for name in ["😀".repeat(600), "界".repeat(600)] {
        let out = ranked(&name);
        assert_inert(&out, RANKED_TAIL);
        let rendered = slot(&out, RANKED_TAIL);
        assert!(
            rendered.chars().count() <= SLOT_CAP,
            "multibyte slot must respect the cap: {} chars",
            rendered.chars().count()
        );
        let first = name.chars().next().unwrap_or('?');
        assert!(
            rendered.chars().all(|ch| ch == first),
            "cap must cut whole scalars, not mangle them: {rendered:?}"
        );
    }
}

#[test]
fn name_of_only_newlines_and_spaces_keeps_three_lines() {
    for name in ["\n", "\r\n\r\n", "  \n  ", "\n\n\n"] {
        let out = ranked(name);
        assert_inert(&out, RANKED_TAIL);
    }
}
