#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_core::commands::MAX_RESOURCE_ID_CHARS;
use two_bot_core::custom_commands::{AuditRecord, CommandError, MAX_COMMAND_NAME_CHARS};
use two_bot_core::lfg::{parse_role_spec, RoleSpecError, MAX_ROLE_SPEC_CHARS};
use two_bot_core::scheduled::{resolve_scheduled_id, IdResolution};

/// Mirrors the private `MAX_ROLE_KEY_ERROR_CHARS` in `lfg.rs`: a `BadKey`
/// carries at most this many chars of the raw key into the error reply.
const MAX_ROLE_KEY_ERROR_CHARS: usize = 128;

/// Fixed synthetic schedule-id inventory. Twelve hex chars, like the
/// production generator emits, plus two overlong entries that pin the
/// refusal-before-resolution guard: a 130-char hex id and a 65-emoji id
/// (65 chars but 130 UTF-16 units). No real id, token or secret appears here.
const LONG_HEX_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefab";
/// 65 emoji = 65 chars, 130 UTF-16 units (overlong); exact match must refuse.
const LONG_EMOJI_ID: &str = "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀";
/// 64 emoji = 64 chars, 128 UTF-16 units (at the bound, not overlong).
const BOUNDARY_EMOJI_PREFIX: &str = "😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀😀";
/// 129 ASCII chars = 129 UTF-16 units (overlong) and a prefix of LONG_HEX_ID.
const LONG_HEX_PREFIX_129: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefa";
const SCHEDULE_IDS: [&str; 6] = [
    "01f3a9c4d2e5",
    "01f3a9c4d2e6",
    "9b7e2a10cc44",
    "deadbeef0042",
    LONG_HEX_ID,
    LONG_EMOJI_ID,
];

fuzz_target!(|data: &[u8]| {
    // Slash options arrive as UTF-8 text. Invalid UTF-8 is discarded,
    // matching the other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };

    // Empty/overlong base cases, independent of the fuzzer input: the
    // #694 bounds refuse before any resolution, parsing or store access.
    assert!(parse_role_spec("").is_err());
    assert!(matches!(
        resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), ""),
        IdResolution::Missing
    ));
    let overlong_prefix = "x".repeat(MAX_RESOURCE_ID_CHARS + 1);
    assert!(matches!(
        resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), &overlong_prefix),
        IdResolution::Missing
    ));
    // Pin the overlong guard against a matching inventory id: without the
    // guard these would resolve Unique. Also pins the UTF-16 measure: the
    // emoji id is only 65 chars but 130 UTF-16 units, so a `.chars()` count
    // would wrongly accept it.
    assert!(matches!(
        resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), LONG_HEX_PREFIX_129),
        IdResolution::Missing
    ));
    assert!(matches!(
        resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), LONG_EMOJI_ID),
        IdResolution::Missing
    ));
    // Boundary pin: 128 UTF-16 units is allowed, so the 64-emoji prefix of
    // the long emoji id still resolves Unique.
    assert!(matches!(
        resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), BOUNDARY_EMOJI_PREFIX),
        IdResolution::Unique(id) if id == LONG_EMOJI_ID
    ));
    assert_eq!(
        AuditRecord::put("guild", "actor", "", true)
            .target_key
            .as_deref(),
        Some("")
    );

    // `/lfg roles` spec: refusal before parsing, bounded `BadKey` echo, and
    // no panic or hang on hostile shapes (mentions, links, markdown, NUL,
    // multi-thousand-char keys).
    let spec_too_long =
        raw.encode_utf16().take(MAX_ROLE_SPEC_CHARS + 1).count() > MAX_ROLE_SPEC_CHARS;
    match parse_role_spec(raw) {
        // Refusal-before-resolution pin: `TooLong` if and only if the raw
        // spec exceeds the published UTF-16 bound.
        Err(RoleSpecError::TooLong) => assert!(
            spec_too_long,
            "only an overlong spec refuses before parsing"
        ),
        Err(RoleSpecError::BadKey(key)) => {
            assert!(!spec_too_long);
            assert!(
                key.chars().count() <= MAX_ROLE_KEY_ERROR_CHARS,
                "BadKey echoes at most a bounded raw prefix"
            );
        }
        Err(_) => assert!(!spec_too_long),
        Ok(_) => assert!(!spec_too_long),
    }

    // `/command name` audit rows: every `target_key` carries at most the
    // 32-char command-name bound, exactly the char-prefix of the input.
    let expected_key: String = raw.chars().take(MAX_COMMAND_NAME_CHARS).collect();
    let put = AuditRecord::put("guild", "actor", raw, raw.len() % 2 == 0);
    assert_eq!(put.target_key.as_deref(), Some(expected_key.as_str()));
    let rejected = AuditRecord::put_rejected(
        "guild",
        "actor",
        raw,
        raw.len() % 2 == 1,
        &CommandError::InvalidName,
    );
    assert_eq!(rejected.target_key.as_deref(), Some(expected_key.as_str()));
    let deleted = AuditRecord::delete("guild", "actor", raw, raw.len() % 2 == 0);
    assert_eq!(deleted.target_key.as_deref(), Some(expected_key.as_str()));
    let run = AuditRecord::run("guild", "actor", raw, raw.len() % 2 == 0, None);
    assert_eq!(run.target_key.as_deref(), Some(expected_key.as_str()));

    // `/schedule-remove` (and `/lfg-close`) id prefixes: empty and overlong
    // prefixes refuse before resolution, and every other outcome agrees with
    // an independent exact-or-prefix-match oracle over the same inventory.
    // No database, Discord client, network or secret is touched here; only
    // these pure functions run.
    let prefix_refused = raw.is_empty()
        || raw.encode_utf16().take(MAX_RESOURCE_ID_CHARS + 1).count() > MAX_RESOURCE_ID_CHARS;
    let oracle: Vec<&str> = SCHEDULE_IDS
        .iter()
        .copied()
        .filter(|id| *id == raw || id.starts_with(raw))
        .collect();
    match resolve_scheduled_id(SCHEDULE_IDS.iter().copied(), raw) {
        IdResolution::Missing => assert!(
            prefix_refused || oracle.is_empty(),
            "missing means refused prefix or no match"
        ),
        IdResolution::Unique(id) => {
            assert!(!prefix_refused);
            assert_eq!(oracle.len(), 1, "unique means exactly one match");
            assert_eq!(id, oracle[0]);
        }
        IdResolution::Ambiguous => {
            assert!(!prefix_refused);
            assert!(oracle.len() >= 2, "ambiguous means at least two matches");
        }
    }
});
