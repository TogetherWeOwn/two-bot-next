//! Guild time zone for the voice name time tokens (`@@hour@@`, `@@weekday@@`,
//! `@@month@@`, `@@daypart@@` and the date and time-of-day conditions).
//!
//! `settings.time_zone` accepts `UTC`/`GMT`, a fixed offset (`UTC+3`,
//! `GMT-5`, `+05:30`, `-0800`) or one of the IANA names listed in this
//! module's `ZONES` table. Named zones follow their current daylight-saving rule (US, EU, Australian or
//! New Zealand); there is no historical tz database, so instants before a
//! rule changed use today's rule. Anything else is `None`, and callers keep
//! UTC.

/// Daylight-saving rules for the named zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// No daylight saving.
    Fixed,
    /// Second Sunday of March 02:00 local to the first Sunday of November
    /// 02:00 local.
    UnitedStates,
    /// Last Sunday of March to the last Sunday of October, 01:00 UTC.
    Europe,
    /// First Sunday of October 02:00 standard to the first Sunday of April
    /// 03:00 daylight (southern hemisphere).
    Australia,
    /// Last Sunday of September 02:00 standard to the first Sunday of April
    /// 03:00 daylight (southern hemisphere).
    NewZealand,
}

/// Supported IANA names: standard offset in minutes east of UTC and rule.
const ZONES: &[(&str, i32, Rule)] = &[
    ("America/New_York", -300, Rule::UnitedStates),
    ("America/Detroit", -300, Rule::UnitedStates),
    ("America/Toronto", -300, Rule::UnitedStates),
    ("America/Chicago", -360, Rule::UnitedStates),
    ("America/Winnipeg", -360, Rule::UnitedStates),
    ("America/Denver", -420, Rule::UnitedStates),
    ("America/Edmonton", -420, Rule::UnitedStates),
    ("America/Phoenix", -420, Rule::Fixed),
    ("America/Los_Angeles", -480, Rule::UnitedStates),
    ("America/Vancouver", -480, Rule::UnitedStates),
    ("America/Anchorage", -540, Rule::UnitedStates),
    ("America/Halifax", -240, Rule::UnitedStates),
    ("America/Sao_Paulo", -180, Rule::Fixed),
    ("America/Mexico_City", -360, Rule::Fixed),
    ("Pacific/Honolulu", -600, Rule::Fixed),
    ("Europe/London", 0, Rule::Europe),
    ("Europe/Dublin", 0, Rule::Europe),
    ("Europe/Lisbon", 0, Rule::Europe),
    ("Europe/Paris", 60, Rule::Europe),
    ("Europe/Berlin", 60, Rule::Europe),
    ("Europe/Amsterdam", 60, Rule::Europe),
    ("Europe/Brussels", 60, Rule::Europe),
    ("Europe/Madrid", 60, Rule::Europe),
    ("Europe/Rome", 60, Rule::Europe),
    ("Europe/Vienna", 60, Rule::Europe),
    ("Europe/Zurich", 60, Rule::Europe),
    ("Europe/Stockholm", 60, Rule::Europe),
    ("Europe/Oslo", 60, Rule::Europe),
    ("Europe/Copenhagen", 60, Rule::Europe),
    ("Europe/Warsaw", 60, Rule::Europe),
    ("Europe/Prague", 60, Rule::Europe),
    ("Europe/Athens", 120, Rule::Europe),
    ("Europe/Helsinki", 120, Rule::Europe),
    ("Europe/Kyiv", 120, Rule::Europe),
    ("Europe/Bucharest", 120, Rule::Europe),
    ("Europe/Istanbul", 180, Rule::Fixed),
    ("Europe/Moscow", 180, Rule::Fixed),
    ("Asia/Dubai", 240, Rule::Fixed),
    ("Asia/Kolkata", 330, Rule::Fixed),
    ("Asia/Singapore", 480, Rule::Fixed),
    ("Asia/Shanghai", 480, Rule::Fixed),
    ("Asia/Hong_Kong", 480, Rule::Fixed),
    ("Asia/Manila", 480, Rule::Fixed),
    ("Asia/Tokyo", 540, Rule::Fixed),
    ("Asia/Seoul", 540, Rule::Fixed),
    ("Australia/Perth", 480, Rule::Fixed),
    ("Australia/Brisbane", 600, Rule::Fixed),
    ("Australia/Adelaide", 570, Rule::Australia),
    ("Australia/Sydney", 600, Rule::Australia),
    ("Australia/Melbourne", 600, Rule::Australia),
    ("Australia/Hobart", 600, Rule::Australia),
    ("Pacific/Auckland", 720, Rule::NewZealand),
];

/// Largest fixed offset accepted, in minutes (UTC-14 to UTC+14).
const MAX_OFFSET_MINUTES: i32 = 14 * 60;

/// The zone's offset from UTC, in minutes east, at `unix_seconds`. `None`
/// for a value this module does not understand.
#[must_use]
pub fn offset_minutes(zone: &str, unix_seconds: i64) -> Option<i32> {
    let zone = zone.trim();
    if let Some(offset) = fixed_offset(zone) {
        return Some(offset);
    }
    let (_, standard, rule) = ZONES
        .iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(zone))?;
    Some(
        standard
            + if in_daylight(*rule, *standard, unix_seconds) {
                60
            } else {
                0
            },
    )
}

/// Whether `zone` is accepted by [`offset_minutes`].
#[must_use]
pub fn is_known(zone: &str) -> bool {
    offset_minutes(zone, 0).is_some()
}

fn fixed_offset(zone: &str) -> Option<i32> {
    let upper = zone.to_ascii_uppercase();
    if matches!(upper.as_str(), "UTC" | "GMT" | "Z" | "ETC/UTC" | "ETC/GMT") {
        return Some(0);
    }
    let rest = upper
        .strip_prefix("UTC")
        .or_else(|| upper.strip_prefix("GMT"))
        .unwrap_or(&upper);
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if digits.len() > 2 => digits.split_at(digits.len() - 2),
        None => (digits, "0"),
    };
    if hours.is_empty()
        || hours.len() > 2
        || minutes.len() > 2
        || !hours
            .bytes()
            .chain(minutes.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    let total = hours * 60 + minutes;
    (minutes < 60 && total <= MAX_OFFSET_MINUTES).then_some(sign * total)
}

fn in_daylight(rule: Rule, standard: i32, unix_seconds: i64) -> bool {
    let year = civil_year(unix_seconds);
    let at = |date: i64, utc_minutes: i64| date * 86_400 + utc_minutes * 60;
    let standard = i64::from(standard);
    match rule {
        Rule::Fixed => false,
        Rule::UnitedStates => {
            let start = at(nth_sunday(year, 3, 2), 120 - standard);
            let end = at(nth_sunday(year, 11, 1), 120 - (standard + 60));
            (start..end).contains(&unix_seconds)
        }
        Rule::Europe => {
            let start = at(last_sunday(year, 3), 60);
            let end = at(last_sunday(year, 10), 60);
            (start..end).contains(&unix_seconds)
        }
        Rule::Australia => {
            let end = at(nth_sunday(year, 4, 1), 180 - (standard + 60));
            let start = at(nth_sunday(year, 10, 1), 120 - standard);
            unix_seconds < end || unix_seconds >= start
        }
        Rule::NewZealand => {
            let end = at(nth_sunday(year, 4, 1), 180 - (standard + 60));
            let start = at(last_sunday(year, 9), 120 - standard);
            unix_seconds < end || unix_seconds >= start
        }
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_year(unix_seconds: i64) -> i64 {
    let z = unix_seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(month <= 2)
}

/// Monday 0 .. Sunday 6 for a day count since the epoch (a Thursday).
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// Day count of the `n`th Sunday (1-based) of `month`.
fn nth_sunday(year: i64, month: i64, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (6 - weekday(first)).rem_euclid(7) + (n - 1) * 7
}

/// Day count of the last Sunday of `month`.
fn last_sunday(year: i64, month: i64) -> i64 {
    let next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    let last = next - 1;
    last - (weekday(last) - 6).rem_euclid(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix seconds for a UTC date and time.
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60
    }

    #[test]
    fn fixed_offsets_and_utc_parse() {
        for (zone, expected) in [
            ("UTC", Some(0)),
            ("gmt", Some(0)),
            ("Etc/UTC", Some(0)),
            ("UTC+3", Some(180)),
            ("UTC-5", Some(-300)),
            ("GMT+05:30", Some(330)),
            ("+05:30", Some(330)),
            ("-0800", Some(-480)),
            ("UTC+14", Some(840)),
            ("UTC+15", None),
            ("UTC+3:60", None),
            ("Mars/Olympus", None),
            ("", None),
        ] {
            assert_eq!(offset_minutes(zone, 0), expected, "{zone}");
        }
    }

    #[test]
    fn united_states_daylight_saving_switches_at_2am_local() {
        let ny = "America/New_York";
        // 2026: starts 8 March 07:00 UTC, ends 1 November 06:00 UTC.
        assert_eq!(offset_minutes(ny, utc(2026, 3, 8, 6, 59)), Some(-300));
        assert_eq!(offset_minutes(ny, utc(2026, 3, 8, 7, 0)), Some(-240));
        assert_eq!(offset_minutes(ny, utc(2026, 11, 1, 5, 59)), Some(-240));
        assert_eq!(offset_minutes(ny, utc(2026, 11, 1, 6, 0)), Some(-300));
        let la = "america/los_angeles";
        assert_eq!(offset_minutes(la, utc(2026, 7, 1, 0, 0)), Some(-420));
        assert_eq!(
            offset_minutes("America/Phoenix", utc(2026, 7, 1, 0, 0)),
            Some(-420)
        );
    }

    #[test]
    fn european_daylight_saving_switches_at_1am_utc() {
        let berlin = "Europe/Berlin";
        // 2026: last Sundays are 29 March and 25 October.
        assert_eq!(offset_minutes(berlin, utc(2026, 3, 29, 0, 59)), Some(60));
        assert_eq!(offset_minutes(berlin, utc(2026, 3, 29, 1, 0)), Some(120));
        assert_eq!(offset_minutes(berlin, utc(2026, 10, 25, 0, 59)), Some(120));
        assert_eq!(offset_minutes(berlin, utc(2026, 10, 25, 1, 0)), Some(60));
        assert_eq!(
            offset_minutes("Europe/London", utc(2026, 7, 1, 0, 0)),
            Some(60)
        );
    }

    #[test]
    fn southern_hemisphere_daylight_saving_wraps_the_new_year() {
        let sydney = "Australia/Sydney";
        // 2026: ends 5 April 03:00 AEDT (16:00 UTC on 4 April), starts
        // 4 October 02:00 AEST (16:00 UTC on 3 October).
        assert_eq!(offset_minutes(sydney, utc(2026, 1, 15, 0, 0)), Some(660));
        assert_eq!(offset_minutes(sydney, utc(2026, 4, 4, 15, 59)), Some(660));
        assert_eq!(offset_minutes(sydney, utc(2026, 4, 4, 16, 0)), Some(600));
        assert_eq!(offset_minutes(sydney, utc(2026, 10, 3, 15, 59)), Some(600));
        assert_eq!(offset_minutes(sydney, utc(2026, 10, 3, 16, 0)), Some(660));
        let auckland = "Pacific/Auckland";
        // 2026: starts 27 September 02:00 NZST (14:00 UTC on 26 September).
        assert_eq!(
            offset_minutes(auckland, utc(2026, 9, 26, 13, 59)),
            Some(720)
        );
        assert_eq!(offset_minutes(auckland, utc(2026, 9, 26, 14, 0)), Some(780));
    }

    #[test]
    fn every_named_zone_parses_and_unknown_zones_do_not() {
        for (name, _, _) in ZONES {
            assert!(is_known(name), "{name}");
        }
        assert!(!is_known("America/Atlantis"));
    }
}
