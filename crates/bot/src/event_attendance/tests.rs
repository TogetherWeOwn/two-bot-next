use super::*;
use serde_json::json;

const MIN: i64 = 60_000;
const SCOPE: AttendanceScope<'static> = AttendanceScope {
    guild_id: 42,
    event_key: "game-night",
};
const WINDOW: AttendanceWindowMs<'static> = AttendanceWindowMs {
    scope: SCOPE,
    starts_ms: 60 * MIN,
    ends_ms: 120 * MIN,
};
const CLOSE: i64 = 135 * MIN;

fn session<'a>(member_id: Snowflake, joined_ms: i64, left_ms: i64) -> SessionMs<'a> {
    SessionMs {
        scope: SCOPE,
        member_id,
        channel_id: 7,
        channel_name: "lobby",
        name_observed_ns: i128::from(left_ms) * 1_000_000,
        is_bot: false,
        afk: false,
        joined_ms,
        left_ms: Some(left_ms),
    }
}

fn project(rows: &[SessionMs<'_>]) -> Result<Value, AttendanceRefusal> {
    project_attendance(&WINDOW, rows, CLOSE)
}

#[test]
fn final_v1_contains_only_scoped_positive_list_fields() {
    let mut early = session(2, 40 * MIN, 45 * MIN + 30_000);
    early.channel_name = "renamed";
    early.name_observed_ns = i128::MAX - 1;
    let mut open = session(2, CLOSE - 30_000, CLOSE);
    open.left_ms = None;
    let mut afk = session(10, 68 * MIN, 80 * MIN);
    afk.channel_id = 9;
    afk.channel_name = "quiet room";
    afk.afk = true;
    let mut foreign_guild = session(99, 60 * MIN, 80 * MIN);
    foreign_guild.scope.guild_id = 43;
    let mut foreign_event = session(99, 60 * MIN, 80 * MIN);
    foreign_event.scope.event_key = "other-event";
    let mut bot = session(99, 60 * MIN, 80 * MIN);
    bot.is_bot = true;
    bot.channel_id = 11;
    let rows = [
        session(10, 60 * MIN, 70 * MIN),
        session(10, 62 * MIN, 68 * MIN),
        session(3, 65 * MIN, 66 * MIN),
        early,
        open,
        afk,
        foreign_guild,
        foreign_event,
        bot,
        session(99, CLOSE + 1, CLOSE + MIN),
        session(99, 60 * MIN, 59 * MIN),
    ];
    assert_eq!(
        project_attendance(&WINDOW, &rows, CLOSE - 1),
        Err(AttendanceRefusal::NotFinal)
    );
    let expected = json!({
        "v": 1,
        "event_key": "game-night",
        "window": {"starts_at": "1970-01-01T01:00:00.000Z", "ends_at": "1970-01-01T02:00:00.000Z"},
        "threshold_minutes": 10,
        "peak_concurrent": 2,
        "rooms": [
            {"channel_id": "7", "name_at_close": "renamed"},
            {"channel_id": "9", "name_at_close": "quiet room"},
        ],
        "members": [
            {"discord_id": "2", "minutes": 1, "sessions": 2, "first_joined_at": "1970-01-01T00:40:00.000Z"},
            {"discord_id": "3", "minutes": 1, "sessions": 1, "first_joined_at": "1970-01-01T01:05:00.000Z"},
            {"discord_id": "10", "minutes": 20, "sessions": 3, "first_joined_at": "1970-01-01T01:00:00.000Z"},
        ],
        "final": true,
    });
    assert_eq!(project(&rows), Ok(expected.clone()));
    assert_eq!(
        project_attendance(&WINDOW, &rows, CLOSE + MIN),
        Ok(expected)
    );
}

#[test]
fn afk_time_and_touching_members_do_not_raise_peak() {
    let start = 60 * MIN;
    let mut afk = session(u64::MAX, start + 3 * MIN, start + 18 * MIN);
    afk.channel_id = u64::MAX;
    afk.afk = true;
    let late = session(u64::MAX, start + 15 * MIN, start + 20 * MIN);
    let mut zero = session(2, CLOSE, CLOSE);
    zero.channel_name = "last at tied stamp";
    zero.name_observed_ns = late.name_observed_ns;
    let rows = [
        session(u64::MAX, start, start + 5 * MIN),
        afk,
        late,
        session(10, start + 10 * MIN, start + 15 * MIN),
        zero,
    ];
    let result = project(&rows).unwrap();
    assert_eq!(result["peak_concurrent"], 1);
    assert_eq!(result["members"][0]["discord_id"], "2");
    assert_eq!(result["members"][0]["minutes"], 0);
    assert_eq!(result["members"][1]["discord_id"], "10");
    assert_eq!(result["members"][2]["discord_id"], u64::MAX.to_string());
    assert_eq!(result["members"][2]["minutes"], 20);
    assert_eq!(result["rooms"][0]["name_at_close"], "last at tied stamp");
    assert_eq!(result["rooms"][1]["channel_id"], u64::MAX.to_string());
}

#[test]
fn no_eligible_observation_or_invalid_window_refuses() {
    let mut bot = session(1, 60 * MIN, 70 * MIN);
    bot.is_bot = true;
    let outside = session(2, 40 * MIN, 45 * MIN - 1);
    assert_eq!(project(&[]), Err(AttendanceRefusal::NoRecord));
    assert_eq!(project(&[bot, outside]), Err(AttendanceRefusal::NoRecord));
    for (starts_ms, ends_ms) in [(1, 0), (0, i64::MAX), (i64::MIN, 0)] {
        let window = AttendanceWindowMs {
            starts_ms,
            ends_ms,
            scope: SCOPE,
        };
        let result = project_attendance(&window, &[], i64::MAX);
        assert_eq!(result, Err(AttendanceRefusal::InvalidWindow));
    }
}

#[test]
fn input_bounds_refuse_whole_result_even_with_tiny_output() {
    let row = session(1, 60 * MIN, 70 * MIN);
    let mut rows = vec![row; MAX_INPUT_SESSIONS];
    let at_limit = project(&rows).unwrap();
    assert_eq!(at_limit["members"][0]["sessions"], MAX_INPUT_SESSIONS);
    rows.push(row);
    assert_eq!(project(&rows), Err(AttendanceRefusal::OverBudget));
    let name = "界".repeat(MAX_INPUT_NAME_BYTES / 3);
    let mut bot = row;
    bot.channel_name = &name;
    bot.is_bot = true;
    assert_eq!(project(&[row, bot]), Err(AttendanceRefusal::OverBudget));
}

#[test]
fn wire_budget_counts_escaped_utf8_and_reserves_envelope_bytes() {
    let mut row = session(1, 60 * MIN, 70 * MIN);
    row.channel_name = "";
    let base = project(&[row]).unwrap();
    let spare = MAX_RESULT_BYTES - serde_json::to_vec(&base).unwrap().len();
    let mut name = String::from("界");
    name.push_str(&"\"".repeat((spare - 3) / 2));
    name.push_str(&"x".repeat((spare - 3) % 2));
    row.channel_name = &name;
    let at_limit = project(&[row]).unwrap();
    let wire = serde_json::to_vec(&at_limit).unwrap();
    assert_eq!(wire.len(), MAX_RESULT_BYTES);
    assert_eq!(at_limit["rooms"][0]["name_at_close"], name);
    let over = name + "x";
    row.channel_name = &over;
    assert_eq!(project(&[row]), Err(AttendanceRefusal::OverBudget));
}
