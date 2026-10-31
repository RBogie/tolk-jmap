//! Time handling for EAS calendars: the `TIME_ZONE_INFORMATION` blob,
//! compact UTC date-times and ISO 8601 durations.
//!
//! EAS describes a time zone the Windows way: a base offset plus one rule
//! each for the start of standard and daylight time ("last Sunday of
//! October, 03:00"). JSCalendar uses IANA names, so the rules are derived by
//! finding the zone's actual transitions in a year.

use crate::LockExt;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;

/// EAS compact UTC date-time, e.g. `20261030T100000Z`.
pub fn compact(time: DateTime<Utc>) -> String {
    time.format("%Y%m%dT%H%M%SZ").to_string()
}

/// A JSCalendar LocalDateTime (`2026-10-30T10:00:00`, seconds optional).
pub fn parse_local(value: &str) -> Option<NaiveDateTime> {
    let value = value.split('.').next()?;
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .ok()
}

/// A local time in `tz` (UTC when floating) as UTC. A time skipped by a
/// daylight saving jump uses the offset from before the jump (RFC 5545).
pub fn to_utc(local: NaiveDateTime, tz: Option<Tz>) -> DateTime<Utc> {
    match tz {
        Some(tz) => match tz.from_local_datetime(&local).earliest() {
            Some(time) => time.with_timezone(&Utc),
            None => {
                // Gaps last a few hours at most, so half a day earlier is before it.
                let before = local - Duration::hours(12);
                let offset = tz.offset_from_utc_datetime(&before).fix().local_minus_utc();
                Utc.from_utc_datetime(&(local - Duration::seconds(i64::from(offset))))
            }
        },
        None => Utc.from_utc_datetime(&local),
    }
}

/// An ISO 8601 duration as used by JSCalendar (`PT1H30M`, `P1D`, `-PT15M`).
pub fn parse_duration(value: &str) -> Option<Duration> {
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = Duration::zero();
    for (part, units) in [
        (date, [('W', 7 * 86_400), ('D', 86_400), ('_', 0)]),
        (time, [('H', 3600), ('M', 60), ('S', 1)]),
    ] {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
                continue;
            }
            let seconds = units.iter().find(|(u, _)| *u == c)?.1;
            let n: f64 = number.parse().ok()?;
            total += Duration::milliseconds((n * f64::from(seconds) * 1000.0) as i64);
            number.clear();
        }
        if !number.is_empty() {
            return None;
        }
    }
    Some(if negative { -total } else { total })
}

/// A Windows SYSTEMTIME used as a yearly rule: month, weekday and week of
/// the month (5 = last), at a local time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rule {
    month: u16,
    day_of_week: u16,
    week: u16,
    hour: u16,
    minute: u16,
}

/// The zone's base offset and, if it observes daylight saving time, the
/// daylight offset and both transition rules, as seen in `year`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Zone {
    standard_minutes: i32,
    daylight: Option<(i32, Rule, Rule)>,
}

fn offset_minutes(tz: Tz, at: DateTime<Utc>) -> i32 {
    tz.offset_from_utc_datetime(&at.naive_utc())
        .fix()
        .local_minus_utc()
        / 60
}

fn zone_rules(tz: Tz, year: i32) -> Zone {
    let day = |d: i64| {
        Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(year, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        ) + Duration::days(d)
    };
    let mut transitions = Vec::new();
    for d in 0..366 {
        let (from, to) = (day(d), day(d + 1));
        if offset_minutes(tz, from) != offset_minutes(tz, to) {
            // Narrow down to the minute.
            let (mut low, mut high) = (from, to);
            while high - low > Duration::minutes(1) {
                let mid = low + (high - low) / 2;
                if offset_minutes(tz, mid) == offset_minutes(tz, low) {
                    low = mid
                } else {
                    high = mid
                }
            }
            transitions.push((high, offset_minutes(tz, low), offset_minutes(tz, high)));
        }
    }
    let standard_minutes = (0..12)
        .map(|m| offset_minutes(tz, day(m * 31)))
        .min()
        .unwrap_or(0);
    let rule_at = |(at, before, _): (DateTime<Utc>, i32, i32)| {
        // Windows rules use the local time just before the switch.
        let local = at.naive_utc() + Duration::minutes(i64::from(before));
        let days_in_month = NaiveDate::from_ymd_opt(local.year(), local.month() % 12 + 1, 1)
            .unwrap_or(NaiveDate::from_ymd_opt(local.year() + 1, 1, 1).unwrap())
            .pred_opt()
            .map_or(31, |d| if local.month() == 12 { 31 } else { d.day() });
        let week = if local.day() + 7 > days_in_month {
            5
        } else {
            (local.day() - 1) / 7 + 1
        };
        Rule {
            month: local.month() as u16,
            day_of_week: local.weekday().num_days_from_sunday() as u16,
            week: week as u16,
            hour: local.hour() as u16,
            minute: local.minute() as u16,
        }
    };
    let to_daylight = transitions.iter().find(|(_, before, after)| after > before);
    let to_standard = transitions.iter().find(|(_, before, after)| after < before);
    let daylight = match (to_daylight, to_standard) {
        (Some(on), Some(off)) => Some((on.2, rule_at(*on), rule_at(*off))),
        _ => None,
    };
    let standard_minutes = if daylight.is_some() {
        standard_minutes
    } else {
        offset_minutes(tz, day(0))
    };
    Zone {
        standard_minutes,
        daylight,
    }
}

/// The base64 `TIME_ZONE_INFORMATION` for `tz` (UTC when `None`), using
/// the rules in effect in `year`.
pub fn eas_timezone(tz: Option<Tz>, year: i32) -> String {
    let zone = tz.map_or(
        Zone {
            standard_minutes: 0,
            daylight: None,
        },
        |tz| zone_rules(tz, year),
    );
    let name = tz.map_or("UTC", |tz| tz.name());
    let mut out = Vec::with_capacity(172);
    let push_name = |out: &mut Vec<u8>, name: &str| {
        let mut units: Vec<u16> = name.encode_utf16().take(31).collect();
        units.resize(32, 0);
        out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
    };
    let push_rule = |out: &mut Vec<u8>, rule: Rule| {
        // wYear, wMonth, wDayOfWeek, wDay, wHour, wMinute, wSecond, wMilliseconds
        for value in [
            0,
            rule.month,
            rule.day_of_week,
            rule.week,
            rule.hour,
            rule.minute,
            0,
            0,
        ] {
            out.extend(value.to_le_bytes());
        }
    };
    out.extend((-zone.standard_minutes).to_le_bytes());
    push_name(&mut out, name);
    let (daylight_bias, daylight_start, standard_start) = match zone.daylight {
        Some((daylight_minutes, on, off)) => (-(daylight_minutes - zone.standard_minutes), on, off),
        None => (0, Rule::default(), Rule::default()),
    };
    push_rule(&mut out, standard_start);
    out.extend(0i32.to_le_bytes());
    push_name(&mut out, name);
    push_rule(&mut out, daylight_start);
    out.extend(daylight_bias.to_le_bytes());
    STANDARD.encode(out)
}

/// Zones tried first when matching a device's rules, so the common name
/// wins over the many zones that share its rules.
const PREFERRED: &[&str] = &[
    "Europe/Amsterdam",
    "Europe/London",
    "Europe/Paris",
    "Europe/Helsinki",
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "America/Sao_Paulo",
    "Asia/Tokyo",
    "Asia/Shanghai",
    "Asia/Kolkata",
    "Australia/Sydney",
    "Pacific/Auckland",
    "UTC",
];

/// The IANA zone for a device's `TIME_ZONE_INFORMATION`: the standard name
/// if it is an IANA name, otherwise the first zone whose current rules match.
/// `None` means UTC.
pub fn parse_eas_timezone(blob: &str) -> Option<Tz> {
    type Cache = std::sync::Mutex<std::collections::HashMap<String, Option<Tz>>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.locked().get(blob) {
        return *found;
    }
    let found = match_timezone(blob);
    let mut cache = cache.locked();
    // Devices send a handful of distinct blobs; a flood of new ones must
    // not grow memory without bound.
    if cache.len() >= 1000 {
        cache.clear();
    }
    cache.insert(blob.to_owned(), found);
    found
}

fn match_timezone(blob: &str) -> Option<Tz> {
    let bytes = STANDARD
        .decode(blob.trim())
        .ok()
        .filter(|b| b.len() >= 172)?;
    let i32_at = |at: usize| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let rule_at = |at: usize| Rule {
        month: word(at + 2),
        day_of_week: word(at + 4),
        week: word(at + 6),
        hour: word(at + 8),
        minute: word(at + 10),
    };
    let name: String = char::decode_utf16((0..32).map(|i| word(4 + i * 2)).take_while(|u| *u != 0))
        .filter_map(Result::ok)
        .collect();
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(tz);
    }
    // Biases beyond a day are garbage (and would overflow below).
    let bias = |at: usize| Some(i32_at(at)).filter(|b| b.unsigned_abs() <= 24 * 60);
    let standard_minutes = -bias(0)?;
    let daylight_bias = bias(168)?;
    let (standard_start, daylight_start) = (rule_at(68), rule_at(152));
    let target = Zone {
        standard_minutes,
        daylight: (daylight_start.month != 0).then(|| {
            (
                standard_minutes - daylight_bias,
                daylight_start,
                standard_start,
            )
        }),
    };
    if target
        == (Zone {
            standard_minutes: 0,
            daylight: None,
        })
    {
        return None;
    }
    let year = Utc::now().year();
    let preferred = PREFERRED.iter().filter_map(|name| name.parse::<Tz>().ok());
    let all = chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .filter(|tz| tz.name().contains('/'));
    let found = preferred
        .chain(all)
        .find(|tz| zone_rules(*tz, year) == target);
    if found.is_none() {
        tracing::warn!(blob, "no time zone matches the device's rules, using UTC");
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(blob: &str) -> Vec<u8> {
        STANDARD.decode(blob).unwrap()
    }

    fn i32_at(bytes: &[u8], at: usize) -> i32 {
        i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn rule_at(bytes: &[u8], at: usize) -> [u16; 6] {
        let word = |i: usize| u16::from_le_bytes([bytes[at + i * 2], bytes[at + i * 2 + 1]]);
        [word(1), word(2), word(3), word(4), word(5), word(0)]
    }

    /// What Gmail on Android sends for a phone set to Amsterdam time.
    const GMAIL_AMSTERDAM: &str = "xP///wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAoAAAAFAAMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMAAAAFAAIAAAAAAAAAxP///w==";

    /// Every zone whose 2026 rules a `TIME_ZONE_INFORMATION` can express
    /// (one offset, or one yearly switch to daylight time and back) maps
    /// back to a zone with the same UTC offsets all year, even when the
    /// device sends no usable zone name.
    #[test]
    fn every_zone_survives_a_nameless_round_trip() {
        use chrono::{Offset, TimeZone};
        let instants: Vec<DateTime<Utc>> = (1..=12)
            .flat_map(|month| {
                [1, 15].map(|day| Utc.with_ymd_and_hms(2026, month, day, 12, 0, 0).unwrap())
            })
            .collect();
        let offsets = |tz: Tz| -> Vec<i32> {
            instants
                .iter()
                .map(|t| {
                    tz.offset_from_utc_datetime(&t.naive_utc())
                        .fix()
                        .local_minus_utc()
                })
                .collect()
        };
        let mut failures = Vec::new();
        for tz in chrono_tz::TZ_VARIANTS {
            let expected = offsets(tz);
            // More than two distinct offsets, or switching more than twice
            // a year (e.g. a Ramadan pause), does not fit the format.
            let switches = expected.windows(2).filter(|w| w[0] != w[1]).count();
            if switches > 2 {
                continue;
            }
            let mut blob = STANDARD.decode(eas_timezone(Some(tz), 2026)).unwrap();
            blob[4..68].fill(0);
            blob[88..152].fill(0);
            let found = parse_eas_timezone(&STANDARD.encode(&blob)).unwrap_or(chrono_tz::UTC);
            if offsets(found) != expected {
                failures.push(format!("{} -> {}", tz.name(), found.name()));
            }
        }
        assert!(
            failures.is_empty(),
            "{} zones: {failures:?}",
            failures.len()
        );
    }

    #[test]
    fn finds_zones_from_device_rules() {
        assert_eq!(
            parse_eas_timezone(GMAIL_AMSTERDAM),
            Some(chrono_tz::Europe::Amsterdam)
        );
        // Our own blobs map back to an equivalent zone.
        for tz in [
            chrono_tz::America::New_York,
            chrono_tz::Australia::Sydney,
            chrono_tz::Asia::Tokyo,
        ] {
            let found = parse_eas_timezone(&eas_timezone(Some(tz), 2026));
            assert_eq!(found.map(|f| f.name()), Some(tz.name()));
        }
        // Named blobs and UTC.
        assert_eq!(
            parse_eas_timezone(&eas_timezone(Some(chrono_tz::Europe::Lisbon), 2026)),
            Some(chrono_tz::Europe::Lisbon)
        );
        assert_eq!(
            parse_eas_timezone(&eas_timezone(None, 2026)),
            Some(chrono_tz::UTC)
        );
        assert_eq!(parse_eas_timezone("garbage"), None);
        // Found by fuzzing: an absurd bias must not overflow.
        let mut absurd = vec![0u8; 172];
        absurd[168..172].copy_from_slice(&i32::MIN.to_le_bytes());
        absurd[152 + 2] = 3;
        assert_eq!(parse_eas_timezone(&STANDARD.encode(&absurd)), None);
    }

    #[test]
    fn amsterdam_rules() {
        let blob = decode(&eas_timezone(Some(chrono_tz::Europe::Amsterdam), 2026));
        assert_eq!(blob.len(), 172);
        assert_eq!(i32_at(&blob, 0), -60);
        // Standard time starts on the last Sunday of October at 03:00.
        assert_eq!(rule_at(&blob, 68), [10, 0, 5, 3, 0, 0]);
        assert_eq!(i32_at(&blob, 84), 0);
        // Daylight time starts on the last Sunday of March at 02:00.
        assert_eq!(rule_at(&blob, 152), [3, 0, 5, 2, 0, 0]);
        assert_eq!(i32_at(&blob, 168), -60);
    }

    #[test]
    fn new_york_and_utc() {
        let blob = decode(&eas_timezone(Some(chrono_tz::America::New_York), 2026));
        assert_eq!(i32_at(&blob, 0), 300);
        assert_eq!(rule_at(&blob, 68), [11, 0, 1, 2, 0, 0]);
        assert_eq!(rule_at(&blob, 152), [3, 0, 2, 2, 0, 0]);

        let utc = decode(&eas_timezone(None, 2026));
        assert_eq!(i32_at(&utc, 0), 0);
        assert_eq!(rule_at(&utc, 68), [0; 6]);
        assert_eq!(i32_at(&utc, 168), 0);

        let tokyo = decode(&eas_timezone(Some(chrono_tz::Asia::Tokyo), 2026));
        assert_eq!(i32_at(&tokyo, 0), -540);
        assert_eq!(rule_at(&tokyo, 152), [0; 6]);
    }

    #[test]
    fn southern_hemisphere_rules() {
        let blob = decode(&eas_timezone(Some(chrono_tz::Australia::Sydney), 2026));
        assert_eq!(i32_at(&blob, 0), -600);
        assert_eq!(rule_at(&blob, 68), [4, 0, 1, 3, 0, 0]);
        assert_eq!(rule_at(&blob, 152), [10, 0, 1, 2, 0, 0]);
    }

    #[test]
    fn converts_local_times() {
        let local = parse_local("2026-10-30T10:00:00").unwrap();
        assert_eq!(
            compact(to_utc(local, Some(chrono_tz::Europe::Amsterdam))),
            "20261030T090000Z"
        );
        assert_eq!(compact(to_utc(local, None)), "20261030T100000Z");
        // 02:30 does not exist on 29 March 2026 in Amsterdam.
        let gap = parse_local("2026-03-29T02:30").unwrap();
        assert_eq!(
            compact(to_utc(gap, Some(chrono_tz::Europe::Amsterdam))),
            "20260329T013000Z"
        );
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_duration("P1W"), Some(Duration::days(7)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("P1DT2H"), Some(Duration::hours(26)));
        assert_eq!(parse_duration("PT0S"), Some(Duration::zero()));
        assert_eq!(parse_duration("1H"), None);
    }
}
