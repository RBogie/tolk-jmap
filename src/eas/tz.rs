//! Time handling for EAS calendars: the `TIME_ZONE_INFORMATION` blob,
//! compact UTC date-times and ISO 8601 durations.
//!
//! EAS describes a time zone the Windows way: a base offset plus one rule
//! each for the start of standard and daylight time ("last Sunday of
//! October, 03:00"). JSCalendar uses IANA names, so the rules are derived by
//! finding the zone's actual transitions in a year.

use crate::LockExt;
use base64::{Engine, engine::general_purpose::STANDARD};
use jiff::{
    SignedDuration, Timestamp,
    civil::{self, DateTime},
    tz::Offset,
};

/// An IANA time zone.
pub type Tz = jiff::tz::TimeZone;

/// A number of whole days as a duration.
pub fn days(n: i64) -> SignedDuration {
    SignedDuration::from_hours(n.saturating_mul(24))
}

/// EAS compact UTC date-time, e.g. `20261030T100000Z`.
pub fn compact(time: Timestamp) -> String {
    time.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// A JSCalendar LocalDateTime (`2026-10-30T10:00:00`, seconds optional).
pub fn parse_local(value: &str) -> Option<DateTime> {
    let value = value.split('.').next()?;
    DateTime::strptime("%Y-%m-%dT%H:%M:%S", value)
        .or_else(|_| DateTime::strptime("%Y-%m-%dT%H:%M", value))
        .ok()
}

/// A local time in `tz` (UTC when floating) as UTC. A time skipped by a
/// daylight saving jump uses the offset from before the jump (RFC 5545).
/// Times at the very ends of the supported range are clamped.
pub fn to_utc(local: DateTime, tz: Option<&Tz>) -> Timestamp {
    let utc = || Tz::UTC.to_timestamp(local);
    match tz {
        // "Compatible": a time in a gap moves forward by the gap, so it keeps
        // the offset from before the jump; a repeated time is the earlier.
        Some(tz) => tz
            .to_ambiguous_timestamp(local)
            .compatible()
            .or_else(|_| utc()),
        None => utc(),
    }
    .unwrap_or(if local.year() < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

/// The local time of `time` in `tz` (UTC when floating).
pub fn local_in(time: Timestamp, tz: Option<&Tz>) -> DateTime {
    tz.unwrap_or(&Tz::UTC).to_datetime(time)
}

/// An ISO 8601 duration as used by JSCalendar (`PT1H30M`, `P1D`, `-PT15M`).
pub fn parse_duration(value: &str) -> Option<SignedDuration> {
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = SignedDuration::ZERO;
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
            let part = SignedDuration::from_millis((n * f64::from(seconds) * 1000.0) as i64);
            total = total.checked_add(part)?;
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

fn offset_minutes(tz: &Tz, at: Timestamp) -> i32 {
    tz.to_offset(at).seconds() / 60
}

fn zone_rules(tz: &Tz, year: i32) -> Zone {
    // Years outside what a calendar uses are read as the nearest one.
    let year = i16::try_from(year.clamp(1, 9998)).unwrap_or(2026);
    let new_year = Tz::UTC
        .to_timestamp(civil::date(year, 1, 1).at(0, 0, 0, 0))
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let day = |d: i64| new_year + days(d);
    let mut transitions = Vec::new();
    for d in 0..366 {
        let (from, to) = (day(d), day(d + 1));
        if offset_minutes(tz, from) != offset_minutes(tz, to) {
            // Narrow down to the minute.
            let (mut low, mut high) = (from, to);
            while high.duration_since(low) > SignedDuration::from_mins(1) {
                let mid = low + high.duration_since(low) / 2;
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
    let rule_at = |(at, before, _): (Timestamp, i32, i32)| {
        // Windows rules use the local time just before the switch.
        let offset = Offset::from_seconds(before * 60).unwrap_or(Offset::UTC);
        let local = offset.to_datetime(at);
        let week = if local.day() + 7 > local.days_in_month() {
            5
        } else {
            (local.day() - 1) / 7 + 1
        };
        let word = |n: i8| u16::try_from(n).unwrap_or(0);
        Rule {
            month: word(local.month()),
            day_of_week: word(local.weekday().to_sunday_zero_offset()),
            week: word(week),
            hour: word(local.hour()),
            minute: word(local.minute()),
        }
    };
    let to_daylight = transitions.iter().find(|(_, before, after)| after > before);
    let to_standard = transitions.iter().find(|(_, before, after)| after < before);
    let daylight = match (to_daylight, to_standard) {
        (Some(on), Some(off)) => Some((on.2, rule_at(*on), rule_at(*off))),
        _ => None,
    };
    // Without a switch to daylight time and back, one offset: the one at the
    // end of the year, so a zone that changes for good during the year (as
    // when a region keeps daylight time) is described as it is from then on.
    let standard_minutes = if daylight.is_some() {
        standard_minutes
    } else {
        offset_minutes(tz, day(365))
    };
    Zone {
        standard_minutes,
        daylight,
    }
}

/// The base64 `TIME_ZONE_INFORMATION` for `tz` (UTC when `None`), using
/// the rules in effect in `year`.
pub fn eas_timezone(tz: Option<&Tz>, year: i32) -> String {
    let zone = tz.map_or(
        Zone {
            standard_minutes: 0,
            daylight: None,
        },
        |tz| zone_rules(tz, year),
    );
    let name = tz.and_then(Tz::iana_name).unwrap_or("UTC");
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
        return found.clone();
    }
    let found = match_timezone(blob);
    let mut cache = cache.locked();
    // Devices send a handful of distinct blobs; a flood of new ones must
    // not grow memory without bound.
    if cache.len() >= 1000 {
        cache.clear();
    }
    cache.insert(blob.to_owned(), found.clone());
    found
}

/// Whether two `TIME_ZONE_INFORMATION` blobs describe the same offsets and
/// transitions, whatever their names say (iOS sends a zone back without
/// names).
pub fn same_zone(a: &str, b: &str) -> bool {
    a.trim() == b.trim() || matches!((blob_zone(a), blob_zone(b)), (Some(a), Some(b)) if a == b)
}

/// A blob's offsets and transition rules.
fn blob_zone(blob: &str) -> Option<Zone> {
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
    let bias = |at: usize| Some(i32_at(at)).filter(|b| b.unsigned_abs() <= 24 * 60);
    let standard_minutes = -bias(0)?;
    let daylight_bias = bias(168)?;
    let (standard_start, daylight_start) = (rule_at(68), rule_at(152));
    Some(Zone {
        standard_minutes,
        daylight: (daylight_start.month != 0).then(|| {
            (
                standard_minutes - daylight_bias,
                daylight_start,
                standard_start,
            )
        }),
    })
}

/// The zone with this IANA name from the bundled time zone database.
pub fn zone_named(name: &str) -> Option<Tz> {
    if name.is_empty() {
        return None;
    }
    Tz::get(name).ok()
}

fn match_timezone(blob: &str) -> Option<Tz> {
    let bytes = STANDARD
        .decode(blob.trim())
        .ok()
        .filter(|b| b.len() >= 172)?;
    let word = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let name: String = char::decode_utf16((0..32).map(|i| word(4 + i * 2)).take_while(|u| *u != 0))
        .filter_map(Result::ok)
        .collect();
    if let Some(tz) = zone_named(&name) {
        return Some(tz);
    }
    // Biases beyond a day are garbage (and would overflow); no zone then.
    let target = blob_zone(blob)?;
    if target
        == (Zone {
            standard_minutes: 0,
            daylight: None,
        })
    {
        return None;
    }
    let year = i32::from(local_in(Timestamp::now(), None).year());
    let preferred = PREFERRED.iter().filter_map(|name| zone_named(name));
    let all = jiff::tz::db()
        .available()
        .filter(|name| name.as_str().contains('/'))
        .filter_map(|name| zone_named(name.as_str()));
    let found = preferred
        .chain(all)
        .find(|tz| zone_rules(tz, year) == target);
    if found.is_none() {
        tracing::warn!(blob, "no time zone matches the device's rules, using UTC");
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> Tz {
        zone_named(name).unwrap()
    }

    fn name(tz: Option<Tz>) -> Option<String> {
        tz.and_then(|tz| tz.iana_name().map(str::to_owned))
    }

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
        let instants: Vec<Timestamp> = (1..=12)
            .flat_map(|month| {
                [1, 15].map(|day| to_utc(civil::date(2026, month, day).at(12, 0, 0, 0), None))
            })
            .collect();
        let offsets = |tz: &Tz| -> Vec<i32> {
            instants
                .iter()
                .map(|t| tz.to_offset(*t).seconds())
                .collect()
        };
        let mut failures = Vec::new();
        let zones = jiff::tz::db().available().collect::<Vec<_>>();
        assert!(
            zones.len() > 400,
            "the bundled database has {} zones",
            zones.len()
        );
        for name in zones {
            let tz = zone(name.as_str());
            let expected = offsets(&tz);
            // Switching more than twice a year (e.g. a Ramadan pause), or
            // once for good, does not fit the format.
            let switches = expected.windows(2).filter(|w| w[0] != w[1]).count();
            if switches > 2 || expected.first() != expected.last() {
                continue;
            }
            let mut blob = STANDARD.decode(eas_timezone(Some(&tz), 2026)).unwrap();
            blob[4..68].fill(0);
            blob[88..152].fill(0);
            let found = parse_eas_timezone(&STANDARD.encode(&blob)).unwrap_or(Tz::UTC);
            if offsets(&found) != expected {
                failures.push(format!(
                    "{} -> {}",
                    name.as_str(),
                    found.iana_name().unwrap_or("?")
                ));
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
            name(parse_eas_timezone(GMAIL_AMSTERDAM)).as_deref(),
            Some("Europe/Amsterdam")
        );
        // Our own blobs map back to an equivalent zone.
        for tz in ["America/New_York", "Australia/Sydney", "Asia/Tokyo"] {
            let found = parse_eas_timezone(&eas_timezone(Some(&zone(tz)), 2026));
            assert_eq!(name(found).as_deref(), Some(tz));
        }
        // Named blobs and UTC.
        assert_eq!(
            name(parse_eas_timezone(&eas_timezone(
                Some(&zone("Europe/Lisbon")),
                2026
            )))
            .as_deref(),
            Some("Europe/Lisbon")
        );
        assert_eq!(
            name(parse_eas_timezone(&eas_timezone(None, 2026))).as_deref(),
            Some("UTC")
        );
        assert_eq!(parse_eas_timezone("garbage"), None);
        // Found by fuzzing: an absurd bias must not overflow.
        let mut absurd = vec![0u8; 172];
        absurd[168..172].copy_from_slice(&i32::MIN.to_le_bytes());
        absurd[152 + 2] = 3;
        assert_eq!(parse_eas_timezone(&STANDARD.encode(&absurd)), None);
        // A name is looked up in the bundled database only.
        assert_eq!(zone_named(""), None);
        assert_eq!(zone_named("W. Europe Standard Time"), None);
    }

    #[test]
    fn amsterdam_rules() {
        let blob = decode(&eas_timezone(Some(&zone("Europe/Amsterdam")), 2026));
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
        let blob = decode(&eas_timezone(Some(&zone("America/New_York")), 2026));
        assert_eq!(i32_at(&blob, 0), 300);
        assert_eq!(rule_at(&blob, 68), [11, 0, 1, 2, 0, 0]);
        assert_eq!(rule_at(&blob, 152), [3, 0, 2, 2, 0, 0]);

        let utc = decode(&eas_timezone(None, 2026));
        assert_eq!(i32_at(&utc, 0), 0);
        assert_eq!(rule_at(&utc, 68), [0; 6]);
        assert_eq!(i32_at(&utc, 168), 0);

        let tokyo = decode(&eas_timezone(Some(&zone("Asia/Tokyo")), 2026));
        assert_eq!(i32_at(&tokyo, 0), -540);
        assert_eq!(rule_at(&tokyo, 152), [0; 6]);
    }

    /// Regions that keep daylight time from 2026 on (tzdb 2026f) are
    /// described with the offset they have from then on.
    #[test]
    fn a_change_for_good_uses_the_new_offset() {
        let blob = decode(&eas_timezone(Some(&zone("America/Edmonton")), 2026));
        assert_eq!(i32_at(&blob, 0), 360);
        assert_eq!(rule_at(&blob, 152), [0; 6]);
        assert_eq!(i32_at(&blob, 168), 0);
        // A year before the change still has both switches.
        let blob = decode(&eas_timezone(Some(&zone("America/Edmonton")), 2025));
        assert_eq!(i32_at(&blob, 0), 420);
        assert_eq!(rule_at(&blob, 152), [3, 0, 2, 2, 0, 0]);
    }

    #[test]
    fn southern_hemisphere_rules() {
        let blob = decode(&eas_timezone(Some(&zone("Australia/Sydney")), 2026));
        assert_eq!(i32_at(&blob, 0), -600);
        assert_eq!(rule_at(&blob, 68), [4, 0, 1, 3, 0, 0]);
        assert_eq!(rule_at(&blob, 152), [10, 0, 1, 2, 0, 0]);
    }

    #[test]
    fn compact_and_local_formats() {
        let local = parse_local("2026-10-30T10:00").unwrap();
        assert_eq!(local, civil::date(2026, 10, 30).at(10, 0, 0, 0));
        assert_eq!(
            parse_local("2026-10-30T10:00:05.250"),
            Some(civil::date(2026, 10, 30).at(10, 0, 5, 0))
        );
        assert_eq!(parse_local("2026-10-30"), None);
        assert_eq!(parse_local("2026-10-30T10:00:00Z"), None);
        assert_eq!(compact(to_utc(local, None)), "20261030T100000Z");
        assert_eq!(
            local_in(to_utc(local, None), Some(&zone("Asia/Tokyo"))),
            civil::date(2026, 10, 30).at(19, 0, 0, 0)
        );
        // The first of two repeated hours (end of daylight time).
        let fold = parse_local("2026-10-25T02:30").unwrap();
        assert_eq!(
            compact(to_utc(fold, Some(&zone("Europe/Amsterdam")))),
            "20261025T003000Z"
        );
        // The ends of the range are clamped instead of failing.
        assert_eq!(
            to_utc(DateTime::MAX, Some(&zone("Asia/Tokyo"))),
            to_utc(DateTime::MAX, None)
        );
    }

    #[test]
    fn converts_local_times() {
        let local = parse_local("2026-10-30T10:00:00").unwrap();
        assert_eq!(
            compact(to_utc(local, Some(&zone("Europe/Amsterdam")))),
            "20261030T090000Z"
        );
        assert_eq!(compact(to_utc(local, None)), "20261030T100000Z");
        // 02:30 does not exist on 29 March 2026 in Amsterdam.
        let gap = parse_local("2026-03-29T02:30").unwrap();
        assert_eq!(
            compact(to_utc(gap, Some(&zone("Europe/Amsterdam")))),
            "20260329T013000Z"
        );
    }

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_duration("PT1H30M"),
            Some(SignedDuration::from_mins(90))
        );
        assert_eq!(parse_duration("P1D"), Some(days(1)));
        assert_eq!(parse_duration("P1W"), Some(days(7)));
        assert_eq!(
            parse_duration("-PT15M"),
            Some(SignedDuration::from_mins(-15))
        );
        assert_eq!(
            parse_duration("P1DT2H"),
            Some(SignedDuration::from_hours(26))
        );
        assert_eq!(parse_duration("PT0S"), Some(SignedDuration::ZERO));
        assert_eq!(
            parse_duration("PT1.5S"),
            Some(SignedDuration::from_millis(1500))
        );
        assert_eq!(parse_duration("1H"), None);
        // Absurd values do not overflow.
        assert!(
            parse_duration(&format!(
                "P{}W{}DT{}H",
                "9".repeat(30),
                "9".repeat(30),
                "9".repeat(30)
            ))
            .is_some()
        );
    }
}
