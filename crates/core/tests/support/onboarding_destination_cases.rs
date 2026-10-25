//! Shared, hand-derived write/reader fixtures. Protected acceptance test data.

use serde_json::{json, Value};

pub const SINGLES: [&str; 3] = [
    "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
    "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID",
    "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID",
];
pub const LISTS: [&str; 2] = ["DISCORD_LANDING_CHANNEL_IDS", "DISCORD_GOODBYE_CHANNEL_IDS"];
pub const ALL: [&str; 5] = [SINGLES[0], SINGLES[1], SINGLES[2], LISTS[0], LISTS[1]];

/// Each tuple is (stored JSON, literal env rendering, literal parsed IDs).
/// Booleans and flat arrays deliberately follow the existing renderer, not a
/// newly invented JSON schema. Empty/null CSV elements are omitted by readers.
pub fn valid(key: &str) -> Vec<(Value, &'static str, Vec<u64>)> {
    let mut cases = vec![
        (json!(42), "42", vec![42]),
        (json!(42.0), "42", vec![42]),
        (json!(" \t42\n"), " \t42\n", vec![42]),
        (json!("+42"), "+42", vec![42]),
        (json!("00042"), "00042", vec![42]),
        (
            json!(9_007_199_254_740_993_u64),
            "9007199254740993",
            vec![9_007_199_254_740_993],
        ),
        (json!(u64::MAX), "18446744073709551615", vec![u64::MAX]),
        (json!(true), "1", vec![1]),
        (json!([42]), "42", vec![42]),
        (json!(""), "", vec![]),
        (json!(" \t\n"), " \t\n", vec![]),
        (json!([]), "", vec![]),
        (json!([null]), "", vec![]),
    ];
    if LISTS.contains(&key) {
        cases.extend([
            (json!(" 42, ,43,, "), " 42, ,43,, ", vec![42, 43]),
            (json!([42, " 43 ", null, ""]), "42, 43 ,,", vec![42, 43]),
            (json!(", ,"), ", ,", vec![]),
            (json!("42,42"), "42,42", vec![42, 42]),
        ]);
    }
    cases
}

pub fn invalid(key: &str) -> Vec<Value> {
    let mut cases = vec![
        json!(0),
        json!(false),
        json!(-1),
        json!(1.5),
        json!("0"),
        json!("-1"),
        json!("18446744073709551616"),
        json!(18_446_744_073_709_551_616.0_f64),
        json!("1e3"),
        json!("42.0"),
        json!("4 2"),
        json!("private-destination-sentinel"),
        json!("https://example.invalid/private-destination-sentinel"),
        json!({"private-destination-sentinel": 42}),
        json!([[42]]),
        json!([{"id": 42}]),
        json!([false]),
        // A stored JSON null is ignored, not an explicit row deletion (None).
        Value::Null,
    ];
    if LISTS.contains(&key) {
        cases.extend([
            json!("42,0,43"),
            json!("42,private-destination-sentinel"),
            json!([42, -1]),
        ]);
    } else {
        cases.extend([
            json!("42,43"),
            json!("42,"),
            json!([42, 43]),
            json!([42, null]),
        ]);
    }
    cases
}
