//! JSCalendar events (RFC 8984, as JMAP `CalendarEvent`) to EAS Calendar
//! `ApplicationData` (MS-ASCAL) and back.
//!
//! Recurrence follows JSCalendar 2.0 (what Stalwart implements): a single
//! `recurrenceRule` object. RFC 8984's `recurrenceRules` array is still read
//! as a fallback, and cleared on write because a 2.0 server keeps it only as
//! an opaque property that never reaches CalDAV clients.
//!
//! Times go to the device in UTC with the event's time zone alongside, so
//! the device can expand recurrences across daylight saving changes.
//! Recurrence rules EAS cannot express (hourly, several rules, BYSETPOS)
//! are dropped and the event shows as a single occurrence.

use jiff::{SignedDuration as Duration, Timestamp, civil::DateTime};
use serde_json::{Map, Value, json};

use super::{
    device_body,
    email::{self, BodyPreference, body_type},
    tz::{self, Tz, local_in},
};
use crate::wbxml::{
    Element, Tag,
    cp::{airsync, airsyncbase, calendar as cal},
};

mod busy {
    pub const FREE: &str = "0";
    pub const TENTATIVE: &str = "1";
    pub const BUSY: &str = "2";
}

mod meeting {
    pub const APPOINTMENT: &str = "0";
    pub const ORGANIZER: &str = "1";
    pub const RECEIVED: &str = "3";
    /// Cancelled, as the user organized it or received it (MS-ASCAL).
    pub const CANCELLED_ORGANIZER: &str = "5";
    pub const CANCELLED_RECEIVED: &str = "7";
}

/// The parts of an event's timing that exceptions are computed from.
struct Timing {
    tz: Option<Tz>,
    all_day: bool,
    duration: Duration,
}

impl Timing {
    fn of(event: &Value) -> Self {
        let all_day = event["showWithoutTime"] == true;
        Self {
            tz: event["timeZone"].as_str().and_then(tz::zone_named),
            all_day,
            duration: event["duration"]
                .as_str()
                .and_then(tz::parse_duration)
                .unwrap_or(if all_day { tz::days(1) } else { Duration::ZERO }),
        }
    }

    /// Start and end in UTC for a local start; all-day events span whole
    /// days from local midnight.
    fn span(&self, start: DateTime, duration: Duration) -> (Timestamp, Timestamp) {
        let tz = self.tz.as_ref();
        if self.all_day {
            let day = start.date().at(0, 0, 0, 0);
            let days = (duration.as_secs() / 86_400).max(1);
            let last = day.checked_add(tz::days(days)).unwrap_or(day);
            (tz::to_utc(day, tz), tz::to_utc(last, tz))
        } else {
            let begin = tz::to_utc(start, tz);
            (begin, begin.checked_add(duration).unwrap_or(begin))
        }
    }
}

/// The event's `ApplicationData`. `own_email` tells meetings the user
/// organizes from invitations they received.
pub fn to_eas(event: &Value, body: &BodyPreference, own_email: &str) -> Element {
    let mut data = Element::new(airsync::ApplicationData);
    let timing = Timing::of(event);
    let start = event["start"].as_str().and_then(tz::parse_local);
    let year = event_year(start);

    data.push(Element::with_text(
        cal::TimeZone,
        tz::eas_timezone(timing.tz.as_ref(), year),
    ));
    data.push(Element::with_text(
        cal::AllDayEvent,
        if timing.all_day { "1" } else { "0" },
    ));
    if let Some(element) = description_body(event, body) {
        data.push(element);
    }
    data.push(Element::with_text(
        cal::BusyStatus,
        match (event["freeBusyStatus"].as_str(), event["status"].as_str()) {
            (Some("free"), _) => busy::FREE,
            (_, Some("tentative")) => busy::TENTATIVE,
            _ => busy::BUSY,
        },
    ));

    let participants: Vec<&Value> = event["participants"]
        .as_object()
        .map(|p| p.values().collect())
        .unwrap_or_default();
    let organizer = participants
        .iter()
        .find(|p| p["roles"]["owner"] == true)
        .copied()
        .or_else(|| {
            // JSCalendar 2.0 may name the organizer only on the event.
            let address = event["organizerCalendarAddress"].as_str()?;
            participants
                .iter()
                .find(|p| p["calendarAddress"] == address)
                .copied()
        });
    if let Some(organizer) = organizer {
        if let Some(name) = organizer["name"].as_str().filter(|n| !n.is_empty()) {
            data.push(Element::with_text(cal::OrganizerName, name));
        }
        if let Some(email) = participant_email(organizer) {
            data.push(Element::with_text(cal::OrganizerEmail, email));
        }
    }
    if let Some(stamp) = event["updated"]
        .as_str()
        .or(event["created"].as_str())
        .and_then(utc_timestamp)
    {
        data.push(Element::with_text(cal::DtStamp, tz::compact(stamp)));
    }
    if let Some(start) = start {
        let (begin, end) = timing.span(start, timing.duration);
        data.push(Element::with_text(cal::EndTime, tz::compact(end)));
        if let Some(location) = location(event) {
            data.push(Element::with_text(cal::Location, location));
        }
        if let Some(minutes) = reminder_minutes(event) {
            data.push(Element::with_text(cal::Reminder, minutes.to_string()));
        }
        data.push(Element::with_text(cal::Sensitivity, sensitivity(event)));
        data.push(Element::with_text(
            cal::Subject,
            event["title"].as_str().unwrap_or_default(),
        ));
        data.push(Element::with_text(cal::StartTime, tz::compact(begin)));
    }
    if let Some(uid) = event["uid"].as_str() {
        data.push(Element::with_text(cal::UID, uid));
    }

    let organizer_email = organizer.and_then(participant_email);
    let attendees: Vec<&&Value> = participants
        .iter()
        .filter(|p| {
            participant_email(p)
                .is_some_and(|e| organizer_email.is_none_or(|o| !o.eq_ignore_ascii_case(e)))
        })
        .collect();
    let is_meeting = organizer.is_some() || !attendees.is_empty();
    let organized_by_me = organizer_email.is_none_or(|e| is_own(own_email, e));
    data.push(Element::with_text(
        cal::MeetingStatus,
        match (is_meeting, organized_by_me, event["status"] == "cancelled") {
            (false, _, _) => meeting::APPOINTMENT,
            (true, true, false) => meeting::ORGANIZER,
            (true, false, false) => meeting::RECEIVED,
            (true, true, true) => meeting::CANCELLED_ORGANIZER,
            (true, false, true) => meeting::CANCELLED_RECEIVED,
        },
    ));
    if !attendees.is_empty() {
        data.push(attendees_element(&attendees));
    }
    if is_meeting && !organized_by_me {
        let own = participants
            .iter()
            .find(|p| participant_email(p).is_some_and(|e| is_own(own_email, e)));
        data.push(Element::with_text(
            cal::ResponseType,
            own.map_or("5", |p| response_type(p)),
        ));
    }

    let categories: Vec<&String> = event["keywords"]
        .as_object()
        .map(|k| {
            k.iter()
                .filter(|(_, v)| **v == true)
                .map(|(name, _)| name)
                .collect()
        })
        .unwrap_or_default();
    if !categories.is_empty() {
        data.push(
            categories
                .into_iter()
                .fold(Element::new(cal::Categories), |list, name| {
                    list.child(Element::with_text(cal::Category, name.as_str()))
                }),
        );
    }

    if let Some(start) = start
        && let Some(recurrence) = recurrence(event, start, &timing)
    {
        data.push(recurrence);
        if let Some(exceptions) = exceptions(event, &timing, body, organizer_email) {
            data.push(exceptions);
        }
    }
    data
}

/// An EAS `<Attendees>` list of these participants.
fn attendees_element(attendees: &[&&Value]) -> Element {
    attendees
        .iter()
        .fold(Element::new(cal::Attendees), |list, p| {
            let mut attendee = Element::new(cal::Attendee)
                .child(Element::with_text(
                    cal::Email,
                    participant_email(p).unwrap_or_default(),
                ))
                .child(Element::with_text(
                    cal::Name,
                    p["name"]
                        .as_str()
                        .filter(|n| !n.is_empty())
                        .or(participant_email(p))
                        .unwrap_or_default(),
                ));
            attendee.push(Element::with_text(cal::AttendeeStatus, attendee_status(p)));
            attendee.push(Element::with_text(
                cal::AttendeeType,
                if p["roles"]["optional"] == true {
                    "2"
                } else {
                    "1"
                },
            ));
            list.child(attendee)
        })
}

/// The `description` of an event or occurrence as a `<Body>` of the type
/// the device asked for, if it has one.
fn description_body(event: &Value, pref: &BodyPreference) -> Option<Element> {
    let description = event["description"].as_str().filter(|d| !d.is_empty())?;
    Some(body_element(description, is_html(event), pref))
}

/// Whether the event's or occurrence's `description` is HTML
/// (`descriptionContentType`, RFC 8984 4.2.3).
fn is_html(event: &Value) -> bool {
    event["descriptionContentType"]
        .as_str()
        .and_then(|t| t.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/html"))
}

fn body_element(description: &str, html: bool, pref: &BodyPreference) -> Element {
    let (kind, mut data) = match (pref.body_type, html) {
        (body_type::HTML, true) => (body_type::HTML, description.to_owned()),
        (body_type::HTML, false) => (body_type::HTML, email::text_to_html(description)),
        (_, true) => (body_type::PLAIN, email::html_to_text(description)),
        (_, false) => (body_type::PLAIN, description.to_owned()),
    };
    let size = data.len();
    let mut truncated = false;
    if let Some(limit) = pref.truncation_size.map(|l| l as usize)
        && data.len() > limit
    {
        let cut = (0..=limit)
            .rev()
            .find(|&i| data.is_char_boundary(i))
            .unwrap_or(0);
        data.truncate(cut);
        truncated = true;
    }
    let mut body = Element::new(airsyncbase::Body)
        .child(Element::with_text(airsyncbase::Type, kind.to_string()))
        .child(Element::with_text(
            airsyncbase::EstimatedDataSize,
            size.to_string(),
        ));
    if truncated {
        body.push(Element::with_text(airsyncbase::Truncated, "1"));
    }
    body.child(Element::with_text(airsyncbase::Data, data))
}

/// The year whose time zone rules describe an event: its start's, or this
/// year for an event that started earlier.
fn event_year(start: Option<DateTime>) -> i32 {
    let now = i32::from(local_in(Timestamp::now(), None).year());
    start.map_or(now, |s| i32::from(s.year()).max(now))
}

fn utc_timestamp(value: &str) -> Option<Timestamp> {
    value.parse().ok()
}

fn location(event: &Value) -> Option<&str> {
    let mut locations: Vec<(&String, &Value)> = event["locations"].as_object()?.iter().collect();
    locations.sort_by(|a, b| a.0.cmp(b.0));
    locations
        .into_iter()
        .find_map(|(_, l)| l["name"].as_str().filter(|n| !n.is_empty()))
}

fn sensitivity(event: &Value) -> &'static str {
    match event["privacy"].as_str() {
        Some("private" | "secret") => "2",
        _ => "0",
    }
}

/// Minutes before the start of the first alert that is relative to it.
fn reminder_minutes(event: &Value) -> Option<i64> {
    event["alerts"].as_object()?.values().find_map(|alert| {
        let trigger = &alert["trigger"];
        if trigger["relativeTo"].as_str().is_some_and(|r| r != "start") {
            return None;
        }
        let offset = tz::parse_duration(trigger["offset"].as_str()?)?;
        (offset <= Duration::ZERO).then(|| -offset.as_mins())
    })
}

/// A participant's email address: JSCalendar 2.0 `calendarAddress`, or
/// RFC 8984 `email` / `sendTo`.
fn participant_email(p: &Value) -> Option<&str> {
    fn mailto(uri: &Value) -> Option<&str> {
        uri.as_str()?
            .strip_prefix("mailto:")
            .filter(|e| !e.is_empty())
    }
    mailto(&p["calendarAddress"])
        .or_else(|| p["email"].as_str().filter(|e| !e.is_empty()))
        .or_else(|| p["sendTo"].as_object()?.values().find_map(mailto))
}

/// EAS AttendeeStatus: 0 unknown, 2 tentative, 3 accepted, 4 declined, 5 not responded.
fn attendee_status(p: &Value) -> &'static str {
    match p["participationStatus"].as_str() {
        Some("accepted") => "3",
        Some("tentative") => "2",
        Some("declined") => "4",
        Some("needs-action") | None => "5",
        _ => "0",
    }
}

/// EAS ResponseType for the user's own invitation.
fn response_type(p: &Value) -> &'static str {
    match p["participationStatus"].as_str() {
        Some("accepted") => "3",
        Some("tentative") => "2",
        Some("declined") => "4",
        _ => "5",
    }
}

/// The DayOfWeek sums that, with a WeekOfMonth, name the nth of a set of
/// days (MS-ASCAL 2.2.2.37.1): any day, a weekday, a weekend day.
const SET_MASKS: [u32; 3] = [127, 62, 65];

const WEEKDAYS: [(&str, u32); 7] = [
    ("su", 1),
    ("mo", 2),
    ("tu", 4),
    ("we", 8),
    ("th", 16),
    ("fr", 32),
    ("sa", 64),
];

fn weekday_bit(day: &str) -> Option<u32> {
    WEEKDAYS
        .iter()
        .find(|(d, _)| *d == day)
        .map(|(_, bit)| *bit)
}

/// The single recurrence rule as an EAS `<Recurrence>`, if EAS can say it.
fn recurrence(event: &Value, start: DateTime, timing: &Timing) -> Option<Element> {
    let rule = match (&event["recurrenceRule"], &event["recurrenceRules"]) {
        (rule @ Value::Object(_), _) => rule,
        (_, Value::Array(rules)) if rules.len() == 1 => &rules[0],
        _ => return None,
    };
    if rule.get("byHour").is_some() {
        return None;
    }
    let by_day: Vec<(u32, Option<i64>)> = rule["byDay"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| Some((weekday_bit(d["day"].as_str()?)?, d["nthOfPeriod"].as_i64())))
        .collect::<Option<_>>()?;
    let day_mask = by_day.iter().fold(0, |mask, (bit, _)| mask | bit);
    // The nth day of the month (WeekOfMonth 1 to 4, or 5 for the last), as
    // EAS can say it: one weekday (`nthOfPeriod`), every listed day with the
    // same n, or the nth of all days, weekdays or weekend days
    // (`bySetPosition`, MS-ASCAL 2.2.2.37.1).
    let week = |n: i64| match n {
        1..=4 => Some(n),
        -1 => Some(5),
        _ => None,
    };
    let positions = rule["bySetPosition"].as_array();
    let nth: Option<(u32, i64)> = match positions {
        Some(positions) => match positions.as_slice() {
            [position]
                if by_day.iter().all(|(_, n)| n.is_none()) && SET_MASKS.contains(&day_mask) =>
            {
                Some((day_mask, week(position.as_i64()?)?))
            }
            _ => return None,
        },
        None if by_day.iter().any(|(_, n)| n.is_some()) => {
            let n = by_day[0].1?;
            if by_day.iter().any(|(_, other)| *other != Some(n)) {
                return None;
            }
            Some((day_mask, week(n)?))
        }
        None => None,
    };
    let month_days = rule["byMonthDay"].as_array();
    let month_day = match month_days.map(Vec::as_slice) {
        None => None,
        Some([day]) => Some(day.as_i64().filter(|d| (1..=31).contains(d))?),
        Some(_) => return None,
    };
    let month = rule["byMonth"]
        .as_array()
        .and_then(|m| m.first())
        .and_then(|m| m.as_str()?.trim_end_matches('L').parse::<u32>().ok())
        .unwrap_or(u32::from(start.month().unsigned_abs()));
    let start_bit = 1 << start.weekday().to_sunday_zero_offset();

    let mut fields: Vec<(crate::wbxml::Tag, String)> = Vec::new();
    let kind = match rule["frequency"].as_str()? {
        // Every Monday of the month, or an nth day of a week: EAS has no
        // such pattern.
        "monthly" | "yearly" if day_mask != 0 && nth.is_none() => return None,
        "daily" | "weekly" if nth.is_some() => return None,
        "daily" if day_mask != 0 => {
            fields.push((cal::DayOfWeek, day_mask.to_string()));
            "1"
        }
        "daily" => "0",
        "weekly" => {
            fields.push((
                cal::DayOfWeek,
                (if day_mask == 0 { start_bit } else { day_mask }).to_string(),
            ));
            "1"
        }
        "monthly" => match nth {
            Some((bit, week)) => {
                fields.push((cal::DayOfWeek, bit.to_string()));
                fields.push((cal::WeekOfMonth, week.to_string()));
                "3"
            }
            None => {
                fields.push((
                    cal::DayOfMonth,
                    month_day.unwrap_or(i64::from(start.day())).to_string(),
                ));
                "2"
            }
        },
        "yearly" => {
            let kind = match nth {
                Some((bit, week)) => {
                    fields.push((cal::DayOfWeek, bit.to_string()));
                    fields.push((cal::WeekOfMonth, week.to_string()));
                    "6"
                }
                None => {
                    fields.push((
                        cal::DayOfMonth,
                        month_day.unwrap_or(i64::from(start.day())).to_string(),
                    ));
                    "5"
                }
            };
            fields.push((cal::MonthOfYear, month.to_string()));
            kind
        }
        _ => return None,
    };

    let mut recurrence = Element::new(cal::Recurrence)
        .child(Element::with_text(cal::Type, kind))
        .child(Element::with_text(
            cal::Interval,
            rule["interval"].as_u64().unwrap_or(1).to_string(),
        ));
    if let Some(count) = rule["count"].as_u64() {
        recurrence.push(Element::with_text(cal::Occurrences, count.to_string()));
    } else if let Some(until) = rule["until"].as_str().and_then(tz::parse_local) {
        recurrence.push(Element::with_text(
            cal::Until,
            tz::compact(tz::to_utc(until, timing.tz.as_ref())),
        ));
    }
    for (tag, value) in fields {
        recurrence.push(Element::with_text(tag, value));
    }
    Some(recurrence)
}

/// `recurrenceOverrides` as EAS exceptions: removed occurrences, and the
/// fields an occurrence has of its own (MS-ASCAL 2.2.2.21). A field the
/// override names only by a path into it (`participants/p1/...`) is not
/// shown.
fn exceptions(
    event: &Value,
    timing: &Timing,
    body: &BodyPreference,
    organizer_email: Option<&str>,
) -> Option<Element> {
    let overrides = event["recurrenceOverrides"].as_object()?;
    let mut list = Element::new(cal::Exceptions);
    let mut keys: Vec<&String> = overrides.keys().collect();
    keys.sort();
    for key in keys {
        let Some(original) = tz::parse_local(key) else {
            continue;
        };
        let patch = &overrides[key];
        let (original_start, _) = timing.span(original, timing.duration);
        let mut exception = Element::new(cal::Exception).child(Element::with_text(
            cal::ExceptionStartTime,
            tz::compact(original_start),
        ));
        if patch["excluded"] == true {
            exception.push(Element::with_text(cal::Deleted, "1"));
        } else {
            // The occurrence as the series with its patch applied: a server
            // may store a field as a path into the series' value
            // (`alerts/a1/trigger`). Fields that differ from the series are
            // shown; the rest the device takes from the series.
            let occurrence = apply_patch(event, patch);
            // Unless moved, an occurrence starts at its recurrence id.
            let start = patch["start"]
                .as_str()
                .and_then(tz::parse_local)
                .unwrap_or(original);
            let duration = occurrence["duration"]
                .as_str()
                .and_then(tz::parse_duration)
                .unwrap_or(timing.duration);
            let (begin, end) = timing.span(start, duration);
            let differs = |key: &str| occurrence.get(key) != event.get(key);
            if (patch.get("title").is_some() || differs("title"))
                && let Some(title) = occurrence["title"].as_str()
            {
                exception.push(Element::with_text(cal::Subject, title));
            }
            exception.push(Element::with_text(cal::StartTime, tz::compact(begin)));
            exception.push(Element::with_text(cal::EndTime, tz::compact(end)));
            if (patch.get("locations").is_some() || differs("locations"))
                && let Some(location) = location(&occurrence)
            {
                exception.push(Element::with_text(cal::Location, location));
            }
            if (differs("description") || is_html(&occurrence) != is_html(event))
                && let Some(element) = description_body(&occurrence, body)
            {
                exception.push(element);
            }
            if differs("alerts")
                && let Some(minutes) = reminder_minutes(&occurrence)
            {
                exception.push(Element::with_text(cal::Reminder, minutes.to_string()));
            }
            if differs("freeBusyStatus") {
                let status = if occurrence["freeBusyStatus"] == "free" {
                    busy::FREE
                } else {
                    busy::BUSY
                };
                exception.push(Element::with_text(cal::BusyStatus, status));
            }
            if differs("keywords") {
                let categories = occurrence["keywords"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| **v == true)
                    .fold(Element::new(cal::Categories), |list, (name, _)| {
                        list.child(Element::with_text(cal::Category, name.as_str()))
                    });
                exception.push(categories);
            }
            if differs("participants")
                && let Some(participants) = occurrence["participants"].as_object()
            {
                let attendees: Vec<&Value> = participants
                    .values()
                    .filter(|p| {
                        participant_email(p).is_some_and(|e| {
                            organizer_email.is_none_or(|o| !o.eq_ignore_ascii_case(e))
                        })
                    })
                    .collect();
                let attendees: Vec<&&Value> = attendees.iter().collect();
                if !attendees.is_empty() {
                    exception.push(attendees_element(&attendees));
                }
            }
        }
        list.push(exception);
    }
    (!list.children.is_empty()).then_some(list)
}

/// `event` with a `recurrenceOverrides` patch applied (RFC 8984 1.4.9): each
/// key is a path (`/`-separated, `~1` for `/` and `~0` for `~`) and a `null`
/// value removes what it names.
fn apply_patch(event: &Value, patch: &Value) -> Value {
    let mut out = event.clone();
    if let Value::Object(out) = &mut out {
        out.remove("recurrenceOverrides");
    }
    for (path, value) in patch.as_object().into_iter().flatten() {
        let segments: Vec<String> = path
            .split('/')
            .map(|s| s.replace("~1", "/").replace("~0", "~"))
            .collect();
        let Some((last, parents)) = segments.split_last() else {
            continue;
        };
        let mut target = &mut out;
        for segment in parents {
            if !target[segment.as_str()].is_object() {
                target[segment.as_str()] = Value::Object(Map::new());
            }
            target = &mut target[segment.as_str()];
        }
        if let Value::Object(target) = target {
            if value.is_null() {
                target.remove(last);
            } else {
                target.insert(last.clone(), value.clone());
            }
        }
    }
    out
}

/// An occurrence's override with `new` fields written over `old`: a path
/// below a written field (`alerts/a1/trigger` under `alerts`) goes, since a
/// patch may not hold both (RFC 8984 1.4.9).
pub fn merge_override(old: &Map<String, Value>, new: Map<String, Value>) -> Map<String, Value> {
    let mut merged = old.clone();
    merged.remove("excluded");
    for key in new.keys() {
        let below = format!("{key}/");
        merged.retain(|k, _| !k.starts_with(&below));
    }
    merged.extend(new);
    merged
}

/// The `<email:MeetingRequest>` of an invitation email, so the device shows
/// it as a meeting request it can answer (MS-ASEMAIL 2.2.2.48).
pub fn meeting_request(event: &Value) -> Element {
    use crate::wbxml::cp::{email as e, email2 as e2};
    let timing = Timing::of(event);
    let start = event["start"].as_str().and_then(tz::parse_local);
    let year = event_year(start);
    let mut request = Element::new(e::MeetingRequest).child(Element::with_text(
        e::AllDayEvent,
        if timing.all_day { "1" } else { "0" },
    ));
    if let Some(start) = start {
        let (begin, end) = timing.span(start, timing.duration);
        request.push(Element::with_text(e::StartTime, extended(begin)));
        if let Some(stamp) = event["updated"]
            .as_str()
            .or(event["created"].as_str())
            .and_then(utc_timestamp)
        {
            request.push(Element::with_text(e::DtStamp, extended(stamp)));
        }
        request.push(Element::with_text(e::EndTime, extended(end)));
    }
    let recurrence = start.and_then(|s| recurrence(event, s, &timing));
    request.push(Element::with_text(
        e::InstanceType,
        if recurrence.is_some() { "1" } else { "0" },
    ));
    if let Some(location) = location(event) {
        request.push(Element::with_text(e::Location, location));
    }
    let participants: Vec<&Value> = event["participants"]
        .as_object()
        .map(|p| p.values().collect())
        .unwrap_or_default();
    let organizer = participants.iter().find(|p| {
        p["roles"]["owner"] == true
            || (p["calendarAddress"] == event["organizerCalendarAddress"]
                && !p["calendarAddress"].is_null())
    });
    if let Some(organizer) = organizer
        && let Some(email) = participant_email(organizer)
    {
        let organizer = match organizer["name"].as_str().filter(|n| !n.is_empty()) {
            Some(name) => format!("\"{name}\" <{email}>"),
            None => email.to_owned(),
        };
        request.push(Element::with_text(e::Organizer, organizer));
    }
    if let Some(minutes) = reminder_minutes(event) {
        request.push(Element::with_text(e::Reminder, minutes.to_string()));
    }
    request.push(Element::with_text(e::ResponseRequested, "1"));
    if let Some(recurrence) = recurrence {
        request.push(Element::new(e::Recurrences).child(email_recurrence(&recurrence)));
    }
    request.push(Element::with_text(e::Sensitivity, sensitivity(event)));
    request.push(Element::with_text(
        e::BusyStatus,
        if event["freeBusyStatus"] == "free" {
            busy::FREE
        } else {
            busy::BUSY
        },
    ));
    request.push(Element::with_text(
        e::TimeZone,
        tz::eas_timezone(timing.tz.as_ref(), year),
    ));
    if let Some(uid) = event["uid"].as_str() {
        request.push(Element::with_text(e::GlobalObjId, global_obj_id(uid)));
    }
    request.push(Element::with_text(e::DisallowNewTimeProposal, "1"));
    request.push(Element::with_text(e2::MeetingMessageType, "1"));
    request
}

/// EAS email dates: `2026-11-05T13:00:00.000Z`.
fn extended(time: Timestamp) -> String {
    time.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A calendar `<Recurrence>` with the email page's tags, as a meeting
/// request carries it.
fn email_recurrence(recurrence: &Element) -> Element {
    use crate::wbxml::cp::email as e;
    let mut out = Element::new(e::Recurrence);
    for child in recurrence.elements() {
        let tag = match child.tag {
            cal::Type => e::Type,
            cal::Interval => e::Interval,
            cal::Until => e::Until,
            cal::Occurrences => e::Occurrences,
            cal::DayOfWeek => e::DayOfWeek,
            cal::DayOfMonth => e::DayOfMonth,
            cal::WeekOfMonth => e::WeekOfMonth,
            cal::MonthOfYear => e::MonthOfYear,
            _ => continue,
        };
        let value = child.text().unwrap_or_default();
        let value = match (tag, parse_eas_time(value)) {
            (e::Until, Some(time)) => extended(time),
            _ => value.to_owned(),
        };
        out.push(Element::with_text(tag, value));
    }
    out
}

/// Exchange's GlobalObjId for an iCalendar UID (the "vCal-Uid" form, which
/// devices use to find the matching calendar event).
pub fn global_obj_id(uid: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    const CLASS_ID: [u8; 16] = [
        0x04, 0x00, 0x00, 0x00, 0x82, 0x00, 0xE0, 0x00, 0x74, 0xC5, 0xB7, 0x10, 0x1A, 0x82, 0xE0,
        0x08,
    ];
    let mut data = b"vCal-Uid".to_vec();
    data.extend([1, 0, 0, 0]);
    data.extend(uid.as_bytes());
    data.push(0);
    let mut out = CLASS_ID.to_vec();
    out.extend([0u8; 4]); // instance date: the whole series
    out.extend([0u8; 8]); // creation time
    out.extend([0u8; 8]); // reserved
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    STANDARD.encode(out)
}

/// Whether `email` is one of the user's addresses. `own_email` throughout
/// this module is the user's addresses, comma separated (aliases included).
pub fn is_own(own_email: &str, email: &str) -> bool {
    own_email
        .split(',')
        .any(|own| own.trim().eq_ignore_ascii_case(email.trim()))
}

/// The user's participant key in an event, matched on email address.
pub fn own_participant_key<'a>(event: &'a Value, own_email: &str) -> Option<&'a str> {
    event["participants"]
        .as_object()?
        .iter()
        .find(|(_, p)| participant_email(p).is_some_and(|e| is_own(own_email, e)))
        .map(|(key, _)| key.as_str())
}

/// Event properties this mapping owns; a device change replaces exactly
/// these (participants are handled separately, see [`attendees_from_eas`]).
pub const MANAGED: &[&str] = &[
    "title",
    "description",
    "descriptionContentType",
    "locations",
    "start",
    "duration",
    "timeZone",
    "showWithoutTime",
    "alerts",
    "freeBusyStatus",
    "privacy",
    "keywords",
    "recurrenceRule",
    "recurrenceOverrides",
    // Only ever cleared: see the module documentation.
    "recurrenceRules",
];

/// An EAS date-time: compact (`20261101T140000Z`) or extended
/// (`2026-11-01T14:00:00.000Z`).
fn parse_eas_time(value: &str) -> Option<Timestamp> {
    let value = value.trim();
    DateTime::strptime("%Y%m%dT%H%M%SZ", value)
        .ok()
        .map(|t| tz::to_utc(t, None))
        .or_else(|| utc_timestamp(value))
}

fn local_string(time: DateTime) -> String {
    time.strftime("%Y-%m-%dT%H:%M:%S").to_string()
}

/// An ISO 8601 duration such as `PT1H30M` or `P2D`.
fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs().max(0);
    let (days, rest) = (total / 86_400, total % 86_400);
    let (hours, minutes, seconds) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let mut out = String::from("P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if rest > 0 || days == 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
        }
        if minutes > 0 {
            out.push_str(&format!("{minutes}M"));
        }
        if seconds > 0 || rest == 0 {
            out.push_str(&format!("{seconds}S"));
        }
    }
    out
}

/// Start, duration and all-day flag in the event's zone, from UTC times.
fn timing_from_eas(
    start: Timestamp,
    end: Timestamp,
    tz: Option<&Tz>,
    all_day: bool,
) -> (DateTime, Duration) {
    let (local_start, local_end) = (local_in(start, tz), local_in(end, tz));
    if all_day {
        let first = local_start.date();
        // A start just before local midnight is a device rounding issue.
        let first = if local_start.hour() >= 12 {
            first.tomorrow().unwrap_or(first)
        } else {
            first
        };
        let days = (local_end.date().duration_since(first).as_secs() / 86_400).max(1);
        (first.at(0, 0, 0, 0), tz::days(days))
    } else {
        (local_start, end.duration_since(start))
    }
}

/// The managed event properties from a device's `ApplicationData`; every key
/// of [`MANAGED`] is present, `null` when the device has nothing for it.
pub fn from_eas(data: &Element) -> Map<String, Value> {
    let text = |tag| {
        data.child_text(tag)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let mut event = Map::new();
    for key in MANAGED {
        event.insert((*key).to_owned(), Value::Null);
    }
    let mut tz = text(cal::TimeZone).and_then(tz::parse_eas_timezone);
    let all_day = text(cal::AllDayEvent) == Some("1");
    // A start in the hour that repeats when daylight saving time ends has
    // two meanings in local time; keep such an event in UTC to stay exact.
    if !all_day
        && let Some(start) = text(cal::StartTime).and_then(parse_eas_time)
        && tz::to_utc(local_in(start, tz.as_ref()), tz.as_ref()) != start
    {
        tz = None;
    }
    event.insert(
        "timeZone".to_owned(),
        tz.as_ref()
            .and_then(Tz::iana_name)
            .unwrap_or("Etc/UTC")
            .into(),
    );
    event.insert("showWithoutTime".to_owned(), all_day.into());
    event.insert(
        "title".to_owned(),
        text(cal::Subject).unwrap_or_default().into(),
    );

    if let (Some(start), Some(end)) = (
        text(cal::StartTime).and_then(parse_eas_time),
        text(cal::EndTime).and_then(parse_eas_time),
    ) {
        let (local_start, duration) = timing_from_eas(start, end, tz.as_ref(), all_day);
        event.insert("start".to_owned(), local_string(local_start).into());
        event.insert("duration".to_owned(), format_duration(duration).into());
        if let Some(recurrence) = data.find(cal::Recurrence) {
            event.insert(
                "recurrenceRule".to_owned(),
                recurrence_from_eas(recurrence, tz.as_ref()).unwrap_or(Value::Null),
            );
        }
        if let Some(exceptions) = data.find(cal::Exceptions) {
            let overrides = overrides_from_eas(exceptions, tz.as_ref(), all_day);
            if !overrides.is_empty() {
                event.insert("recurrenceOverrides".to_owned(), Value::Object(overrides));
            }
        }
    }
    if let Some((description, content_type)) = description_from_eas(data) {
        event.insert("description".to_owned(), description.into());
        if content_type == HTML_TYPE {
            event.insert("descriptionContentType".to_owned(), content_type.into());
        }
    }
    if let Some(location) = text(cal::Location) {
        event.insert(
            "locations".to_owned(),
            json!({ "l1": { "@type": "Location", "name": location } }),
        );
    }
    event.insert("alerts".to_owned(), alerts_from_eas(text(cal::Reminder)));
    event.insert(
        "freeBusyStatus".to_owned(),
        free_busy_from_eas(text(cal::BusyStatus)),
    );
    event.insert(
        "privacy".to_owned(),
        privacy_from_eas(text(cal::Sensitivity)),
    );
    if let Some(categories) = data.find(cal::Categories) {
        event.insert("keywords".to_owned(), keywords_from_eas(categories));
    }
    event
}

/// The JSCalendar `alerts` for an EAS Reminder (minutes before the start).
fn alerts_from_eas(reminder: Option<&str>) -> Value {
    match reminder.and_then(|m| m.trim().parse::<i64>().ok()) {
        Some(minutes) => json!({ "a1": { "@type": "Alert", "action": "display",
            "trigger": { "@type": "OffsetTrigger",
                "offset": format!("-{}", format_duration(Duration::from_mins(minutes))) } } }),
        None => Value::Null,
    }
}

fn free_busy_from_eas(status: Option<&str>) -> Value {
    (if status.map(str::trim) == Some(busy::FREE) {
        "free"
    } else {
        "busy"
    })
    .into()
}

fn privacy_from_eas(sensitivity: Option<&str>) -> Value {
    (if matches!(sensitivity.map(str::trim), Some("2" | "3")) {
        "private"
    } else {
        "public"
    })
    .into()
}

/// JSCalendar `keywords` for EAS `Categories`; `null` when there are none.
fn keywords_from_eas(categories: &Element) -> Value {
    let keywords: Map<String, Value> = categories
        .find_all(cal::Category)
        .filter_map(|c| c.text().filter(|t| !t.is_empty()))
        .map(|name| (name.to_owned(), Value::Bool(true)))
        .collect();
    if keywords.is_empty() {
        Value::Null
    } else {
        Value::Object(keywords)
    }
}

/// The EAS fields each [`MANAGED`] property is made from.
const SOURCES: &[(&str, &[Tag])] = &[
    ("title", &[cal::Subject]),
    ("description", &[airsyncbase::Body]),
    ("descriptionContentType", &[airsyncbase::Body]),
    ("locations", &[cal::Location]),
    ("start", TIMING),
    ("duration", TIMING),
    ("timeZone", TIMING),
    ("showWithoutTime", &[cal::AllDayEvent]),
    ("alerts", &[cal::Reminder]),
    ("freeBusyStatus", &[cal::BusyStatus]),
    ("privacy", &[cal::Sensitivity]),
    ("keywords", &[cal::Categories]),
    ("recurrenceRule", &[cal::Recurrence]),
    ("recurrenceRules", &[cal::Recurrence]),
];

const TIMING: &[Tag] = &[
    cal::StartTime,
    cal::EndTime,
    cal::AllDayEvent,
    cal::TimeZone,
];

/// The patch for a device's Change of the event `current`, following
/// MS-ASCMD 2.2.3.24 on what a field the device left out means:
///
/// - `Body` and `Exceptions` left out stay unchanged; so does a single
///   occurrence left out of `Exceptions`.
/// - EAS 16: every calendar field left out stays unchanged.
/// - EAS 14: a field left out is cleared, unless the device sent
///   `<Supported>` without it (ghosting).
///
/// Unchanged fields are left out of the patch, and so are fields the device
/// sent back as it was shown them (`shown_body` is its body preference):
/// EAS can show one reminder, one location and no rules it cannot express,
/// and writing that back would drop the rest. The ones that are written are
/// computed with the event's current values filled in for the fields the
/// device left out, so a new end time keeps the event's time zone.
pub fn change_from_eas(
    current: &Value,
    data: &Element,
    version: u16,
    supported: Option<&[String]>,
    own_email: &str,
    shown_body: &BodyPreference,
) -> Map<String, Value> {
    let sent = |tag: Tag| data.find(tag).is_some();
    let managed = |tag: Tag| {
        sent(tag)
            || (version < 160
                && tag != airsyncbase::Body
                && supported.is_none_or(|s| s.contains(&tag.to_string())))
    };
    let whole = BodyPreference::whole();
    let mut merged = data.clone();
    for element in to_eas(current, &whole, own_email).elements() {
        if !managed(element.tag) && element.tag != cal::Exceptions {
            merged.push(element.clone());
        }
    }
    let mut event = from_eas(&merged);
    keep_zone_name(&mut event, current);
    let series = event.clone();
    // What the device was shown, read the same way: a field that reads the
    // same was not changed on the device.
    let shown = to_eas(current, shown_body, own_email);
    let shown_event = from_eas(&shown);
    let timing_unchanged = same_timing(&merged, &shown);
    let unchanged = |key: &str| match key {
        "start" | "duration" | "timeZone" | "showWithoutTime" => timing_unchanged,
        // A new description is written with its type.
        "descriptionContentType" => {
            event.get(key) == shown_event.get(key)
                && event.get("description") == shown_event.get("description")
        }
        _ => event.get(key) == shown_event.get(key),
    };
    let changed: Vec<&str> = SOURCES
        .iter()
        .filter(|(_, tags)| tags.iter().any(|t| managed(*t)))
        .map(|(key, _)| *key)
        .filter(|key| *key != "recurrenceRules" && !unchanged(key))
        .collect();
    let mut patch: Map<String, Value> = changed
        .into_iter()
        .filter_map(|key| Some((key.to_owned(), event.remove(key)?)))
        .collect();
    // RFC 8984's array is cleared whenever the rule is written (module doc).
    if patch.contains_key("recurrenceRule") {
        patch.insert("recurrenceRules".to_owned(), Value::Null);
    }
    // A series that became a single event keeps no occurrences.
    if patch.get("recurrenceRule").is_some_and(Value::is_null)
        && current["recurrenceOverrides"].is_object()
    {
        patch.insert("recurrenceOverrides".to_owned(), Value::Null);
        return patch;
    }
    // An occurrence's key is its original start, so when the series start
    // moves, the occurrences the event has move with it (10:00 to 10:30
    // makes `2026-12-08T10:00:00` `2026-12-08T10:30:00`): they stay
    // attached to their occurrence instead of naming one that no longer
    // exists.
    let shift = patch.get("start").and_then(|start| {
        let new = tz::parse_local(start.as_str()?)?;
        let old = tz::parse_local(current["start"].as_str()?)?;
        Some(new.duration_since(old)).filter(|d| !d.is_zero())
    });
    let rekey = |key: &str| -> String {
        shift
            .and_then(|by| tz::parse_local(key)?.checked_add(by).ok())
            .map_or_else(|| key.to_owned(), local_string)
    };
    let rekeyed = |overrides: Option<&Value>| -> Option<Map<String, Value>> {
        let overrides = overrides?.as_object()?;
        Some(
            overrides
                .iter()
                .map(|(key, entry)| (rekey(key), entry.clone()))
                .collect(),
        )
    };
    let old_overrides = rekeyed(current.get("recurrenceOverrides"));
    // A device may still name an occurrence by its start before the move: a
    // key the event had that no longer names one is read as moved too.
    let device_key = |key: String| -> String {
        let before = current["recurrenceOverrides"].as_object();
        let stale = shift.is_some()
            && before.is_some_and(|b| b.contains_key(&key))
            && old_overrides.as_ref().is_none_or(|o| !o.contains_key(&key));
        if stale { rekey(&key) } else { key }
    };
    let mut written: Option<Map<String, Value>> = old_overrides.clone().filter(|_| shift.is_some());
    // The occurrences the device sent, each merged into what it was.
    if let Some(Value::Object(overrides)) = event.remove("recurrenceOverrides")
        && sent(cal::Exceptions)
    {
        // Each occurrence the device sent, by its recurrence id.
        let timing = Timing::of(&Value::Object(series.clone()));
        let sent_exceptions: std::collections::HashMap<String, &Element> = data
            .find(cal::Exceptions)
            .into_iter()
            .flat_map(|list| list.find_all(cal::Exception))
            .filter_map(|exception| {
                let single = Element::new(cal::Exceptions).child(exception.clone());
                let key = overrides_from_eas(&single, timing.tz.as_ref(), timing.all_day)
                    .into_iter()
                    .next()?
                    .0;
                Some((device_key(key), exception))
            })
            .collect();
        // What the device sent back of an occurrence as it was shown stays as
        // it is (a truncated description, say), and so does an occurrence
        // sent back whole.
        let shown_overrides = rekeyed(shown_event.get("recurrenceOverrides"));
        let old_of = |key: &str| {
            old_overrides
                .as_ref()
                .and_then(|o| o.get(key))
                .unwrap_or(&Value::Null)
        };
        let overrides: Map<String, Value> = overrides
            .into_iter()
            .map(|(key, entry)| (device_key(key), entry))
            .filter_map(|(key, entry)| {
                let shown = shown_overrides.as_ref().and_then(|o| o.get(&key));
                let entry = match (entry, shown) {
                    (Value::Object(mut entry), Some(Value::Object(shown))) => {
                        let content_type = entry.get(DESCRIPTION_TYPE).cloned();
                        entry.retain(|k, v| shown.get(k) != Some(v));
                        // A new description is written with its type.
                        if entry.contains_key("description")
                            && let Some(content_type) = content_type
                        {
                            entry.insert(DESCRIPTION_TYPE.to_owned(), content_type);
                        }
                        if entry.is_empty() {
                            return None;
                        }
                        Value::Object(entry)
                    }
                    (entry, _) => entry,
                };
                Some((key, entry))
            })
            .map(|(key, mut entry)| {
                if let Value::Object(fields) = &mut entry {
                    let old = old_of(&key);
                    inherit(fields, &series, old);
                    if let Some(exception) = sent_exceptions.get(&key) {
                        occurrence_attendees(fields, exception, own_email, current, old);
                    }
                }
                (key, entry)
            })
            .filter(|(_, entry)| entry.as_object().is_none_or(|e| !e.is_empty()))
            .collect();
        for (key, entry) in overrides {
            let entry = match (old_of(&key), entry) {
                (Value::Object(old), Value::Object(new)) if new.get("excluded").is_none() => {
                    Value::Object(merge_override(old, new))
                }
                (_, entry) => entry,
            };
            match &mut written {
                Some(all) => {
                    all.insert(key, entry);
                }
                // A patch path needs an existing object, so a first one
                // writes them whole.
                None if old_overrides.is_none() => {
                    written = Some(Map::from_iter([(key, entry)]));
                }
                None => {
                    patch.insert(format!("recurrenceOverrides/{key}"), entry);
                }
            }
        }
    }
    if let Some(all) = written {
        patch.insert("recurrenceOverrides".to_owned(), Value::Object(all));
    }
    patch
}

/// Keeps the event's own time zone name when the device's zone has the same
/// rules: devices send the zone as rules, and iOS without names, so it would
/// otherwise become the most common zone with those rules (Europe/Zurich
/// turning into Europe/Amsterdam).
fn keep_zone_name(event: &mut Map<String, Value>, current: &Value) {
    let new = event.get("timeZone").and_then(Value::as_str);
    let (Some(new), Some(own)) = (new, current["timeZone"].as_str()) else {
        return;
    };
    if new == own {
        return;
    }
    let (Some(new_zone), Some(own_zone)) = (tz::zone_named(new), tz::zone_named(own)) else {
        return;
    };
    let year = event_year(
        event
            .get("start")
            .and_then(Value::as_str)
            .and_then(tz::parse_local),
    );
    if tz::same_zone(
        &tz::eas_timezone(Some(&new_zone), year),
        &tz::eas_timezone(Some(&own_zone), year),
    ) {
        let own = own.to_owned();
        event.insert("timeZone".to_owned(), own.into());
    }
}

/// Whether two `ApplicationData` have the same start, end, all-day flag and
/// time zone rules.
fn same_timing(a: &Element, b: &Element) -> bool {
    let time = |data: &Element, tag| data.child_text(tag).and_then(parse_eas_time);
    let all_day = |data: &Element| data.child_text(cal::AllDayEvent).map(str::trim) == Some("1");
    let zone = |data: &Element| {
        data.child_text(cal::TimeZone)
            .map(str::trim)
            .filter(|z| !z.is_empty())
            .map(str::to_owned)
    };
    time(a, cal::StartTime) == time(b, cal::StartTime)
        && time(a, cal::EndTime) == time(b, cal::EndTime)
        && all_day(a) == all_day(b)
        && match (zone(a), zone(b)) {
            (Some(a), Some(b)) => tz::same_zone(&a, &b),
            (a, b) => a == b,
        }
}

/// An EAS `<Recurrence>` as a JSCalendar recurrence rule.
fn recurrence_from_eas(recurrence: &Element, tz: Option<&Tz>) -> Option<Value> {
    let number = |tag| {
        recurrence
            .child_text(tag)
            .and_then(|v| v.trim().parse::<i64>().ok())
    };
    let days = |mask: i64, nth: Option<i64>| -> Vec<Value> {
        // Monday first, as JSCalendar writes them.
        let order = [1, 2, 3, 4, 5, 6, 0];
        order
            .iter()
            .map(|&i| WEEKDAYS[i])
            .filter(|(_, bit)| mask & i64::from(*bit) != 0)
            .map(|(day, _)| match nth {
                Some(n) => json!({ "@type": "NDay", "day": day, "nthOfPeriod": if n == 5 { -1 } else { n } }),
                None => json!({ "@type": "NDay", "day": day }),
            })
            .collect()
    };
    // The nth weekday, or with a day, weekday or weekend day sum the nth of
    // those days (MS-ASCAL 2.2.2.37.1): one occurrence a month either way.
    let nth_days = |rule: &mut Map<String, Value>| -> Option<()> {
        let mask = number(cal::DayOfWeek)?;
        let week = number(cal::WeekOfMonth).filter(|w| (1..=5).contains(w))?;
        if u32::try_from(mask).is_ok_and(|m| SET_MASKS.contains(&m)) {
            rule.insert("byDay".to_owned(), days(mask, None).into());
            rule.insert(
                "bySetPosition".to_owned(),
                json!([if week == 5 { -1 } else { week }]),
            );
        } else {
            rule.insert("byDay".to_owned(), days(mask, Some(week)).into());
        }
        Some(())
    };
    let mut rule = Map::new();
    rule.insert("@type".to_owned(), "RecurrenceRule".into());
    let frequency = match number(cal::Type)? {
        0 => "daily",
        1 => {
            rule.insert(
                "byDay".to_owned(),
                days(number(cal::DayOfWeek)?, None).into(),
            );
            "weekly"
        }
        2 => {
            rule.insert("byMonthDay".to_owned(), json!([number(cal::DayOfMonth)?]));
            "monthly"
        }
        3 => {
            nth_days(&mut rule)?;
            "monthly"
        }
        5 => {
            rule.insert(
                "byMonth".to_owned(),
                json!([number(cal::MonthOfYear)?.to_string()]),
            );
            rule.insert("byMonthDay".to_owned(), json!([number(cal::DayOfMonth)?]));
            "yearly"
        }
        6 => {
            rule.insert(
                "byMonth".to_owned(),
                json!([number(cal::MonthOfYear)?.to_string()]),
            );
            nth_days(&mut rule)?;
            "yearly"
        }
        _ => return None,
    };
    rule.insert("frequency".to_owned(), frequency.into());
    if let Some(interval) = number(cal::Interval).filter(|i| *i > 1) {
        rule.insert("interval".to_owned(), interval.into());
    }
    if let Some(count) = number(cal::Occurrences) {
        rule.insert("count".to_owned(), count.into());
    } else if let Some(until) = recurrence.child_text(cal::Until).and_then(parse_eas_time) {
        rule.insert("until".to_owned(), local_string(local_in(until, tz)).into());
    }
    Some(Value::Object(rule))
}

/// The `recurrenceOverrides` entry for an EAS 16 change to one occurrence
/// of `event`: the occurrence's InstanceId (its original start, UTC) and
/// the changed fields (`None`: the occurrence is deleted). Returns the
/// recurrence id and the override.
pub fn occurrence_override(
    event: &Value,
    instance_id: &str,
    data: Option<&Element>,
    own_email: &str,
) -> Option<(String, Value)> {
    let timing = Timing::of(event);
    let mut exception = Element::new(cal::Exception)
        .child(Element::with_text(cal::ExceptionStartTime, instance_id));
    match data {
        None => exception.push(Element::with_text(cal::Deleted, "1")),
        Some(data) => {
            for element in data.elements() {
                if EXCEPTION_FIELDS.contains(&element.tag) {
                    exception.push(element.clone());
                }
            }
        }
    }
    let exceptions = Element::new(cal::Exceptions).child(exception.clone());
    let (key, mut entry) = overrides_from_eas(&exceptions, timing.tz.as_ref(), timing.all_day)
        .into_iter()
        .next()?;
    if let Value::Object(entry) = &mut entry {
        let series = from_eas(&to_eas(event, &BodyPreference::whole(), own_email));
        let old = &event["recurrenceOverrides"][&key];
        inherit(entry, &series, old);
        occurrence_attendees(entry, &exception, own_email, event, old);
    }
    Some((key, entry))
}

/// The fields a change to one occurrence can carry (MS-ASCAL 2.2.2.21).
const EXCEPTION_FIELDS: &[Tag] = &[
    cal::Subject,
    cal::Location,
    cal::StartTime,
    cal::EndTime,
    airsyncbase::Body,
    cal::Reminder,
    cal::BusyStatus,
    cal::Categories,
    cal::Attendees,
    cal::OrganizerEmail,
    cal::MeetingStatus,
];

/// The occurrence fields beyond its title, place and time, which an
/// override only keeps where they differ from the series. Sensitivity is not
/// one: RFC 8984 (4.3.5) allows no `privacy` in an override.
const OCCURRENCE_FIELDS: &[&str] = &["description", "alerts", "freeBusyStatus", "keywords"];

/// Whether the device gives an occurrence a sensitivity other than the
/// series', which JSCalendar cannot store (RFC 8984 4.3.5). `current` is the
/// event a Change is for; `one_occurrence` an EAS 16 change by InstanceId.
pub fn occurrence_privacy(current: Option<&Value>, data: &Element, one_occurrence: bool) -> bool {
    let read = |s: &str| privacy_from_eas(Some(s));
    let stored = current.map_or_else(|| read("0"), |event| read(sensitivity(event)));
    if one_occurrence {
        return data
            .child_text(cal::Sensitivity)
            .is_some_and(|s| read(s) != stored);
    }
    let series = data.child_text(cal::Sensitivity).map_or(stored, read);
    data.find(cal::Exceptions)
        .into_iter()
        .flat_map(|list| list.find_all(cal::Exception))
        .filter(|e| e.child_text(cal::Deleted).map(str::trim) != Some("1"))
        .filter_map(|e| e.child_text(cal::Sensitivity))
        .any(|s| read(s) != series)
}

/// Leaves out of an occurrence's override the fields that only repeat the
/// series (`series` as [`from_eas`] reads it), so a later change to the
/// series reaches the occurrence; a field the occurrence had its own value
/// for (`old`) is written, now as the series has it.
fn inherit(entry: &mut Map<String, Value>, series: &Map<String, Value>, old: &Value) {
    for key in OCCURRENCE_FIELDS {
        let repeats = entry
            .get(*key)
            .is_some_and(|value| value == series.get(*key).unwrap_or(&Value::Null));
        let below = format!("{key}/");
        let own_before = old
            .as_object()
            .is_some_and(|o| o.keys().any(|k| k == key || k.starts_with(&below)));
        if repeats && !own_before {
            entry.remove(*key);
        }
    }
    // The description's type is kept only with a description, and only
    // where it is not the series' (unless the occurrence had its own).
    fn content_type(v: Option<&Value>) -> &str {
        v.and_then(Value::as_str).unwrap_or("text/plain")
    }
    let own_before = old.get(DESCRIPTION_TYPE).is_some();
    if !entry.contains_key("description")
        || (content_type(entry.get(DESCRIPTION_TYPE)) == content_type(series.get(DESCRIPTION_TYPE))
            && !own_before)
    {
        entry.remove(DESCRIPTION_TYPE);
    }
}

/// The attendees of one occurrence of a meeting the user organizes, when the
/// device sent others than the occurrence has.
fn occurrence_attendees(
    entry: &mut Map<String, Value>,
    exception: &Element,
    own_email: &str,
    event: &Value,
    old: &Value,
) {
    let Some(attendees) = attendees_from_eas(exception, own_email) else {
        return;
    };
    let occurrence = apply_patch(event, old);
    let current = &occurrence["participants"];
    let participants = participants(own_email, &attendees, current);
    if participants != *current {
        entry.insert("participants".to_owned(), participants);
    }
}

/// The `recurrenceOverrides` key (local recurrence id) of the occurrence an
/// EAS InstanceId (its original start in UTC) names.
pub fn occurrence_key(event: &Value, instance_id: &str) -> Option<String> {
    let timing = Timing::of(event);
    let original = parse_eas_time(instance_id)?;
    let key = if timing.all_day {
        timing_from_eas(original, original, timing.tz.as_ref(), true).0
    } else {
        local_in(original, timing.tz.as_ref())
    };
    Some(local_string(key))
}

const DESCRIPTION_TYPE: &str = "descriptionContentType";
const HTML_TYPE: &str = "text/html";

/// The body types a description can be: plain text, HTML kept as
/// `text/html` (RFC 8984 4.2.3), or a message, read for its HTML or text.
pub const BODY_TYPES: &[u8] = &[device_body::PLAIN, device_body::HTML, device_body::MIME];

/// The description in a device's `ApplicationData` or `<Exception>`, with
/// its content type; `None` for an empty body or one of a type it cannot be
/// (see [`BODY_TYPES`]).
fn description_from_eas(parent: &Element) -> Option<(String, &'static str)> {
    let body = device_body::of(parent).filter(|b| !b.is_empty())?;
    let (description, html) = body.text_or_html(BODY_TYPES)?;
    Some((description, if html { HTML_TYPE } else { "text/plain" }))
}

/// The body types the device sent that a description cannot be, in the
/// event and its exceptions, and messages with files it has no place for.
pub fn unsupported_bodies(data: &Element) -> Vec<&'static str> {
    let exceptions = data
        .find(cal::Exceptions)
        .into_iter()
        .flat_map(|list| list.find_all(cal::Exception));
    std::iter::once(data)
        .chain(exceptions)
        .filter_map(|parent| {
            device_body::unsupported(parent, BODY_TYPES)
                .or_else(|| device_body::unsupported_files(parent))
        })
        .collect()
}

/// EAS `<Exceptions>` as JSCalendar `recurrenceOverrides`.
fn overrides_from_eas(exceptions: &Element, tz: Option<&Tz>, all_day: bool) -> Map<String, Value> {
    let mut overrides = Map::new();
    for exception in exceptions.find_all(cal::Exception) {
        let Some(original) = exception
            .child_text(cal::ExceptionStartTime)
            .and_then(parse_eas_time)
        else {
            continue;
        };
        let mut key = local_in(original, tz);
        if all_day {
            key = timing_from_eas(original, original, tz, true).0;
        }
        let patch = if exception.child_text(cal::Deleted) == Some("1") {
            json!({ "excluded": true })
        } else {
            let mut patch = Map::new();
            if let Some(subject) = exception.child_text(cal::Subject) {
                patch.insert("title".to_owned(), subject.into());
            }
            if let Some(location) = exception.child_text(cal::Location) {
                patch.insert(
                    "locations".to_owned(),
                    json!({ "l1": { "@type": "Location", "name": location } }),
                );
            }
            // An empty body is no change: devices send one for an
            // occurrence whose description they did not load. The type is
            // written too, so it does not stay the series'.
            if let Some((description, content_type)) = description_from_eas(exception) {
                patch.insert("description".to_owned(), description.into());
                patch.insert(DESCRIPTION_TYPE.to_owned(), content_type.into());
            }
            if let Some(reminder) = exception.find(cal::Reminder) {
                patch.insert("alerts".to_owned(), alerts_from_eas(reminder.text()));
            }
            if let Some(status) = exception.child_text(cal::BusyStatus) {
                patch.insert(
                    "freeBusyStatus".to_owned(),
                    free_busy_from_eas(Some(status)),
                );
            }
            if let Some(categories) = exception.find(cal::Categories) {
                patch.insert("keywords".to_owned(), keywords_from_eas(categories));
            }
            if let (Some(start), Some(end)) = (
                exception
                    .child_text(cal::StartTime)
                    .and_then(parse_eas_time),
                exception.child_text(cal::EndTime).and_then(parse_eas_time),
            ) {
                let (local_start, duration) = timing_from_eas(start, end, tz, all_day);
                if local_start != key {
                    patch.insert("start".to_owned(), local_string(local_start).into());
                }
                patch.insert("duration".to_owned(), format_duration(duration).into());
            }
            Value::Object(patch)
        };
        overrides.insert(local_string(key), patch);
    }
    overrides
}

/// The attendees of a meeting the user organizes on the device, as
/// (email, name, optional); `None` when the device sent no attendees or the
/// meeting was organized by someone else.
pub fn attendees_from_eas(data: &Element, own_email: &str) -> Option<Vec<(String, String, bool)>> {
    let organizer = data.child_text(cal::OrganizerEmail);
    if organizer.is_some_and(|o| !is_own(own_email, o))
        || data
            .child_text(cal::MeetingStatus)
            .is_some_and(|s| s == meeting::RECEIVED || s == "5" || s == "7")
    {
        return None;
    }
    let attendees: Vec<(String, String, bool)> = data
        .find(cal::Attendees)?
        .find_all(cal::Attendee)
        .filter_map(|a| {
            let email = a.child_text(cal::Email)?.trim().to_owned();
            let name = a
                .child_text(cal::Name)
                .unwrap_or_default()
                .trim()
                .to_owned();
            Some((email, name, a.child_text(cal::AttendeeType) == Some("2")))
        })
        .filter(|(email, _, _)| !email.is_empty() && !is_own(own_email, email))
        .collect();
    (!attendees.is_empty()).then_some(attendees)
}

/// JSCalendar 2.0 participants for `attendees`, with the user as organizer
/// (`own_email`: the user's addresses, the primary one first). Existing
/// participants keep their entry (and response) when still invited, the user
/// also under an alias.
pub fn participants(
    own_email: &str,
    attendees: &[(String, String, bool)],
    existing: &Value,
) -> Value {
    let existing: Vec<(&String, &Value)> = existing
        .as_object()
        .map(|p| p.iter().collect())
        .unwrap_or_default();
    let find = |email: &str| {
        existing
            .iter()
            .find(|(_, p)| participant_email(p).is_some_and(|e| e.eq_ignore_ascii_case(email)))
    };
    let mut out = Map::new();
    let primary = own_email.split(',').next().unwrap_or_default().trim();
    let own = existing
        .iter()
        .find(|(_, p)| participant_email(p).is_some_and(|e| is_own(own_email, e)));
    let (owner_key, owner) = match own {
        Some((key, p)) => ((*key).clone(), (*p).clone()),
        None => (
            "owner".to_owned(),
            json!({ "@type": "Participant", "calendarAddress": format!("mailto:{primary}"),
                    "roles": { "owner": true, "attendee": true }, "participationStatus": "accepted" }),
        ),
    };
    out.insert(owner_key, owner);
    for (i, (email, name, optional)) in attendees.iter().enumerate() {
        let (key, mut participant) = match find(email) {
            Some((key, p)) => ((*key).clone(), (*p).clone()),
            None => (
                format!("p{}", i + 1),
                json!({ "@type": "Participant", "calendarAddress": format!("mailto:{email}"),
                        "participationStatus": "needs-action", "expectReply": true }),
            ),
        };
        if !name.is_empty() {
            participant["name"] = name.as_str().into();
        }
        participant["roles"] = if *optional {
            json!({ "attendee": true, "optional": true })
        } else {
            json!({ "attendee": true })
        };
        out.insert(key, participant);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn plain() -> BodyPreference {
        BodyPreference {
            body_type: body_type::PLAIN,
            truncation_size: None,
            all_or_none: false,
            preview: None,
        }
    }

    fn text(data: &Element, tag: crate::wbxml::Tag) -> Option<String> {
        data.child_text(tag).map(str::to_owned)
    }

    #[test]
    fn maps_a_timed_event() {
        let event = json!({
            "uid": "u1",
            "title": "Standup",
            "description": "Daily sync",
            "start": "2026-10-30T10:00:00",
            "timeZone": "Europe/Amsterdam",
            "duration": "PT30M",
            "locations": { "l": { "name": "Room 1" } },
            "alerts": { "a": { "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M" } } },
            "updated": "2026-10-29T12:00:00Z",
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        assert_eq!(
            text(&data, cal::StartTime).as_deref(),
            Some("20261030T090000Z")
        );
        assert_eq!(
            text(&data, cal::EndTime).as_deref(),
            Some("20261030T093000Z")
        );
        assert_eq!(text(&data, cal::AllDayEvent).as_deref(), Some("0"));
        assert_eq!(text(&data, cal::Subject).as_deref(), Some("Standup"));
        assert_eq!(text(&data, cal::Location).as_deref(), Some("Room 1"));
        assert_eq!(text(&data, cal::Reminder).as_deref(), Some("15"));
        assert_eq!(text(&data, cal::UID).as_deref(), Some("u1"));
        assert_eq!(
            text(&data, cal::DtStamp).as_deref(),
            Some("20261029T120000Z")
        );
        assert_eq!(text(&data, cal::MeetingStatus).as_deref(), Some("0"));
        assert_eq!(
            data.find(airsyncbase::Body)
                .and_then(|b| b.child_text(airsyncbase::Data)),
            Some("Daily sync")
        );
        assert!(data.find(cal::Recurrence).is_none());
    }

    #[test]
    fn maps_all_day_events_to_local_midnight() {
        let event = json!({
            "title": "Holiday", "start": "2026-12-25T00:00:00", "timeZone": "Europe/Amsterdam",
            "showWithoutTime": true, "duration": "P2D",
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        assert_eq!(text(&data, cal::AllDayEvent).as_deref(), Some("1"));
        assert_eq!(
            text(&data, cal::StartTime).as_deref(),
            Some("20261224T230000Z")
        );
        assert_eq!(
            text(&data, cal::EndTime).as_deref(),
            Some("20261226T230000Z")
        );
    }

    #[test]
    fn maps_recurrence_and_exceptions() {
        let event = json!({
            "title": "Gym", "start": "2026-11-02T18:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT1H",
            "recurrenceRules": [{ "frequency": "weekly", "byDay": [{ "day": "mo" }, { "day": "th" }], "count": 10 }],
            "recurrenceOverrides": {
                "2026-11-05T18:00:00": { "excluded": true },
                "2026-11-09T18:00:00": { "start": "2026-11-09T19:00:00", "title": "Gym (late)" },
            },
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        let recurrence = data.find(cal::Recurrence).unwrap();
        assert_eq!(text(recurrence, cal::Type).as_deref(), Some("1"));
        assert_eq!(text(recurrence, cal::DayOfWeek).as_deref(), Some("18"));
        assert_eq!(text(recurrence, cal::Occurrences).as_deref(), Some("10"));
        assert_eq!(text(recurrence, cal::Interval).as_deref(), Some("1"));

        let exceptions: Vec<&Element> = data.find(cal::Exceptions).unwrap().elements().collect();
        assert_eq!(
            text(exceptions[0], cal::ExceptionStartTime).as_deref(),
            Some("20261105T170000Z")
        );
        assert_eq!(text(exceptions[0], cal::Deleted).as_deref(), Some("1"));
        assert_eq!(
            text(exceptions[1], cal::StartTime).as_deref(),
            Some("20261109T180000Z")
        );
        assert_eq!(
            text(exceptions[1], cal::EndTime).as_deref(),
            Some("20261109T190000Z")
        );
        assert_eq!(
            text(exceptions[1], cal::Subject).as_deref(),
            Some("Gym (late)")
        );
    }

    #[test]
    fn maps_monthly_and_yearly_rules() {
        let rule = |r: Value| {
            let event = json!({ "start": "2026-03-10T09:00:00", "recurrenceRules": [r] });
            recurrence(
                &event,
                tz::parse_local("2026-03-10T09:00:00").unwrap(),
                &Timing::of(&event),
            )
        };
        let last_friday =
            rule(json!({ "frequency": "monthly", "byDay": [{ "day": "fr", "nthOfPeriod": -1 }] }))
                .unwrap();
        assert_eq!(text(&last_friday, cal::Type).as_deref(), Some("3"));
        assert_eq!(text(&last_friday, cal::DayOfWeek).as_deref(), Some("32"));
        assert_eq!(text(&last_friday, cal::WeekOfMonth).as_deref(), Some("5"));

        let yearly =
            rule(json!({ "frequency": "yearly", "interval": 2, "until": "2030-03-10T09:00:00" }))
                .unwrap();
        assert_eq!(text(&yearly, cal::Type).as_deref(), Some("5"));
        assert_eq!(text(&yearly, cal::DayOfMonth).as_deref(), Some("10"));
        assert_eq!(text(&yearly, cal::MonthOfYear).as_deref(), Some("3"));
        assert_eq!(text(&yearly, cal::Interval).as_deref(), Some("2"));
        assert_eq!(
            text(&yearly, cal::Until).as_deref(),
            Some("20300310T090000Z")
        );

        assert!(rule(json!({ "frequency": "hourly" })).is_none());
        assert!(rule(json!({ "frequency": "monthly", "bySetPosition": [1] })).is_none());
    }

    /// The nth of a set of days (MS-ASCAL 2.2.2.37.1), and the patterns EAS
    /// cannot say, which are left out instead of sent as something else.
    #[test]
    fn maps_nth_of_a_set_of_days() {
        let rule = |r: Value| {
            let event = json!({ "start": "2026-03-10T09:00:00", "recurrenceRule": r });
            recurrence(
                &event,
                tz::parse_local("2026-03-10T09:00:00").unwrap(),
                &Timing::of(&event),
            )
        };
        let fields = |r: &Element| {
            [
                cal::Type,
                cal::DayOfWeek,
                cal::WeekOfMonth,
                cal::MonthOfYear,
            ]
            .map(|tag| text(r, tag).unwrap_or_default())
        };
        let all = json!([{ "day": "mo" }, { "day": "tu" }, { "day": "we" }, { "day": "th" },
            { "day": "fr" }, { "day": "sa" }, { "day": "su" }]);
        let last_day = rule(json!({ "frequency": "monthly", "byDay": all, "bySetPosition": [-1] }));
        assert_eq!(fields(&last_day.unwrap()), ["3", "127", "5", ""]);
        let first_weekday = rule(json!({ "frequency": "monthly", "bySetPosition": [1],
            "byDay": [{ "day": "mo" }, { "day": "tu" }, { "day": "we" }, { "day": "th" }, { "day": "fr" }] }));
        assert_eq!(fields(&first_weekday.unwrap()), ["3", "62", "1", ""]);
        let last_weekend_day = rule(json!({ "frequency": "yearly", "byMonth": ["3"],
            "byDay": [{ "day": "sa" }, { "day": "su" }], "bySetPosition": [-1] }));
        assert_eq!(fields(&last_weekend_day.unwrap()), ["6", "65", "5", "3"]);
        // The same n for every listed day.
        let second = rule(json!({ "frequency": "monthly",
            "byDay": [{ "day": "mo", "nthOfPeriod": 2 }, { "day": "we", "nthOfPeriod": 2 }] }));
        assert_eq!(fields(&second.unwrap()), ["3", "10", "2", ""]);

        for unsayable in [
            // A fifth Monday is not the last one, nor is the second to last.
            json!({ "frequency": "monthly", "byDay": [{ "day": "mo", "nthOfPeriod": 5 }] }),
            json!({ "frequency": "monthly", "byDay": [{ "day": "mo", "nthOfPeriod": -2 }] }),
            // Different n per day.
            json!({ "frequency": "monthly",
                "byDay": [{ "day": "mo", "nthOfPeriod": 1 }, { "day": "we", "nthOfPeriod": 3 }] }),
            // Every Monday of the month.
            json!({ "frequency": "monthly", "byDay": [{ "day": "mo" }] }),
            // The last day by day of the month, or several days.
            json!({ "frequency": "monthly", "byMonthDay": [-1] }),
            json!({ "frequency": "monthly", "byMonthDay": [1, 15] }),
            // Several positions, or a position in a week.
            json!({ "frequency": "monthly", "byDay": all, "bySetPosition": [1, -1] }),
            json!({ "frequency": "weekly", "byDay": all, "bySetPosition": [1] }),
            json!({ "frequency": "monthly", "byDay": [{ "day": "xx" }] }),
        ] {
            assert!(rule(unsayable.clone()).is_none(), "{unsayable}");
        }
    }

    #[test]
    fn reads_nth_of_a_set_of_days() {
        let read = |kind: &str, mask: &str, week: &str| {
            let recurrence = Element::new(cal::Recurrence)
                .child(Element::with_text(cal::Type, kind))
                .child(Element::with_text(cal::DayOfWeek, mask))
                .child(Element::with_text(cal::WeekOfMonth, week))
                .child(Element::with_text(cal::MonthOfYear, "3"));
            recurrence_from_eas(&recurrence, None).unwrap()
        };
        let day = |d: &str| json!({ "@type": "NDay", "day": d });
        let last_day = read("3", "127", "5");
        assert_eq!(
            last_day["byDay"],
            json!(["mo", "tu", "we", "th", "fr", "sa", "su"].map(day))
        );
        assert_eq!(last_day["bySetPosition"], json!([-1]));
        let first_weekday = read("3", "62", "1");
        assert_eq!(
            first_weekday["byDay"],
            json!(["mo", "tu", "we", "th", "fr"].map(day))
        );
        assert_eq!(first_weekday["bySetPosition"], json!([1]));
        let last_weekend_day = read("6", "65", "5");
        assert_eq!(last_weekend_day["frequency"], "yearly");
        assert_eq!(last_weekend_day["byDay"], json!(["sa", "su"].map(day)));
        assert_eq!(last_weekend_day["bySetPosition"], json!([-1]));
        // A week of the month outside 1 to 5 is no rule.
        let sixth = Element::new(cal::Recurrence)
            .child(Element::with_text(cal::Type, "3"))
            .child(Element::with_text(cal::DayOfWeek, "2"))
            .child(Element::with_text(cal::WeekOfMonth, "6"));
        assert!(recurrence_from_eas(&sixth, None).is_none());

        // Read back as it was sent.
        for (kind, mask, week) in [
            ("3", "127", "5"),
            ("3", "62", "1"),
            ("6", "65", "5"),
            ("3", "32", "5"),
        ] {
            let event =
                json!({ "start": "2026-03-10T09:00:00", "recurrenceRule": read(kind, mask, week) });
            let shown = recurrence(
                &event,
                tz::parse_local("2026-03-10T09:00:00").unwrap(),
                &Timing::of(&event),
            )
            .unwrap();
            assert_eq!(text(&shown, cal::Type).as_deref(), Some(kind));
            assert_eq!(text(&shown, cal::DayOfWeek).as_deref(), Some(mask));
            assert_eq!(text(&shown, cal::WeekOfMonth).as_deref(), Some(week));
        }
    }

    /// Gmail's own blob for Amsterdam (see `tz` tests).
    const AMSTERDAM: &str = "xP///wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAoAAAAFAAMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMAAAAFAAIAAAAAAAAAxP///w==";

    #[test]
    fn reads_a_device_event() {
        // The shape Gmail sends for a new event (personal fields replaced).
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::TimeZone, AMSTERDAM))
            .child(Element::with_text(cal::AllDayEvent, "0"))
            .child(Element::with_text(cal::StartTime, "20261101T140000Z"))
            .child(Element::with_text(cal::EndTime, "20261101T150000Z"))
            .child(Element::with_text(cal::DtStamp, "20261030T175426Z"))
            .child(Element::with_text(cal::Subject, "Dentist"))
            .child(
                Element::new(airsyncbase::Body)
                    .child(Element::with_text(airsyncbase::Type, "1"))
                    .child(Element::new(airsyncbase::Data)),
            )
            .child(Element::with_text(cal::OrganizerEmail, "rob@example.com"))
            .child(Element::with_text(cal::UID, "2ddfcc79"))
            .child(Element::with_text(cal::BusyStatus, "2"))
            .child(Element::with_text(cal::MeetingStatus, "0"))
            .child(Element::with_text(cal::Sensitivity, "0"))
            .child(Element::with_text(cal::Reminder, "10"));
        let event = from_eas(&data);
        assert_eq!(event["timeZone"], "Europe/Amsterdam");
        assert_eq!(event["start"], "2026-11-01T15:00:00");
        assert_eq!(event["duration"], "PT1H");
        assert_eq!(event["title"], "Dentist");
        assert_eq!(event["description"], Value::Null);
        assert_eq!(event["freeBusyStatus"], "busy");
        assert_eq!(event["alerts"]["a1"]["trigger"]["offset"], "-PT10M");
        assert_eq!(attendees_from_eas(&data, "rob@example.com"), None);

        // Back to EAS, the device sees the same times.
        let back = to_eas(&Value::Object(event), &plain(), "rob@example.com");
        assert_eq!(
            text(&back, cal::StartTime).as_deref(),
            Some("20261101T140000Z")
        );
        assert_eq!(
            text(&back, cal::EndTime).as_deref(),
            Some("20261101T150000Z")
        );
        assert_eq!(text(&back, cal::Reminder).as_deref(), Some("10"));
    }

    #[test]
    fn round_trips_recurrence_exceptions_and_all_day() {
        let original = json!({
            "title": "Gym", "start": "2026-11-02T18:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT1H",
            "recurrenceRules": [{ "frequency": "weekly", "byDay": [{ "day": "mo" }, { "day": "th" }], "until": "2026-12-31T18:00:00" }],
            "recurrenceOverrides": {
                "2026-11-05T18:00:00": { "excluded": true },
                "2026-11-09T18:00:00": { "start": "2026-11-09T19:00:00", "title": "Gym (late)" },
            },
        });
        let eas = to_eas(&original, &plain(), "rob@example.com");
        let back = from_eas(&eas);
        assert_eq!(back["start"], "2026-11-02T18:00:00");
        assert_eq!(back["recurrenceRule"]["frequency"], "weekly");
        assert_eq!(
            back["recurrenceRule"]["byDay"],
            json!([{ "@type": "NDay", "day": "mo" }, { "@type": "NDay", "day": "th" }])
        );
        assert_eq!(back["recurrenceRule"]["until"], "2026-12-31T18:00:00");
        assert_eq!(back["recurrenceRules"], Value::Null);
        assert_eq!(
            back["recurrenceOverrides"]["2026-11-05T18:00:00"]["excluded"],
            true
        );
        assert_eq!(
            back["recurrenceOverrides"]["2026-11-09T18:00:00"]["start"],
            "2026-11-09T19:00:00"
        );
        assert_eq!(
            back["recurrenceOverrides"]["2026-11-09T18:00:00"]["title"],
            "Gym (late)"
        );
        // And the device would see exactly the same event again.
        assert_eq!(
            crate::wbxml::encode(&eas),
            crate::wbxml::encode(&to_eas(&Value::Object(back), &plain(), "rob@example.com"))
        );

        let holiday = json!({ "title": "Holiday", "start": "2026-12-25T00:00:00", "timeZone": "Europe/Amsterdam",
                              "showWithoutTime": true, "duration": "P2D" });
        let back = from_eas(&to_eas(&holiday, &plain(), "rob@example.com"));
        assert_eq!(back["start"], "2026-12-25T00:00:00");
        assert_eq!(back["duration"], "P2D");
        assert_eq!(back["showWithoutTime"], true);
    }

    #[test]
    fn prefers_the_jscalendar_2_rule() {
        // The JSCalendar 2.0 rule in `recurrenceRule` wins over a stale copy
        // in RFC 8984's `recurrenceRules`, which other software may leave.
        let event = json!({
            "start": "2026-10-30T19:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT1H",
            "recurrenceRule": { "frequency": "daily", "count": 40 },
            "recurrenceRules": [{ "@type": "RecurrenceRule", "frequency": "daily", "count": 17 }],
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        assert_eq!(
            data.find(cal::Recurrence)
                .and_then(|r| r.child_text(cal::Occurrences)),
            Some("40")
        );

        let rfc8984 = json!({ "start": "2026-10-30T19:00:00", "recurrenceRules": [{ "frequency": "weekly" }] });
        let data = to_eas(&rfc8984, &plain(), "rob@example.com");
        assert_eq!(
            data.find(cal::Recurrence)
                .and_then(|r| r.child_text(cal::Type)),
            Some("1")
        );
    }

    #[test]
    fn maps_monthly_nth_weekday_back() {
        let recurrence = Element::new(cal::Recurrence)
            .child(Element::with_text(cal::Type, "3"))
            .child(Element::with_text(cal::Interval, "1"))
            .child(Element::with_text(cal::DayOfWeek, "32"))
            .child(Element::with_text(cal::WeekOfMonth, "5"));
        let rule = recurrence_from_eas(&recurrence, None).unwrap();
        assert_eq!(rule["frequency"], "monthly");
        assert_eq!(
            rule["byDay"],
            json!([{ "@type": "NDay", "day": "fr", "nthOfPeriod": -1 }])
        );
        assert_eq!(rule.get("interval"), None);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_mins(90)), "PT1H30M");
        assert_eq!(format_duration(tz::days(2)), "P2D");
        assert_eq!(format_duration(Duration::from_hours(25)), "P1DT1H");
        assert_eq!(format_duration(Duration::ZERO), "PT0S");
    }

    #[test]
    fn builds_participants_for_own_meetings() {
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::OrganizerEmail, "rob@example.com"))
            .child(Element::with_text(cal::MeetingStatus, "1"))
            .child(
                Element::new(cal::Attendees)
                    .child(
                        Element::new(cal::Attendee)
                            .child(Element::with_text(cal::Email, "ann@example.com"))
                            .child(Element::with_text(cal::Name, "Ann"))
                            .child(Element::with_text(cal::AttendeeType, "1")),
                    )
                    .child(
                        Element::new(cal::Attendee)
                            .child(Element::with_text(cal::Email, "bob@example.com"))
                            .child(Element::with_text(cal::AttendeeType, "2")),
                    ),
            );
        let attendees = attendees_from_eas(&data, "rob@example.com").unwrap();
        assert_eq!(attendees.len(), 2);
        // Ann already accepted; her response survives the update.
        let existing = json!({ "x": { "email": "ann@example.com", "participationStatus": "accepted", "roles": { "attendee": true } } });
        let people = participants("rob@example.com", &attendees, &existing);
        assert_eq!(people["x"]["participationStatus"], "accepted");
        assert_eq!(people["x"]["name"], "Ann");
        assert_eq!(people["p2"]["roles"]["optional"], true);
        assert_eq!(people["p2"]["participationStatus"], "needs-action");
        assert_eq!(people["owner"]["roles"]["owner"], true);
        assert_eq!(people["owner"]["calendarAddress"], "mailto:rob@example.com");
        assert_eq!(people["p2"]["calendarAddress"], "mailto:bob@example.com");

        let received = Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::OrganizerEmail, "ann@example.com"));
        assert_eq!(attendees_from_eas(&received, "rob@example.com"), None);
    }

    /// What Stalwart's CalendarEvent/parse returns for an invitation.
    fn parsed_invite() -> Value {
        json!({
            "@type": "Event", "version": "2.0", "method": "request", "uid": "parse-test-1",
            "title": "Planning", "start": "2026-11-05T14:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT1H",
            "recurrenceRule": { "frequency": "weekly", "count": 3 },
            "updated": "2026-10-30T12:00:00Z",
            "organizerCalendarAddress": "mailto:ann@example.invalid",
            "participants": {
                "a": { "@type": "Participant", "calendarAddress": "mailto:rob@example.com", "participationStatus": "needs-action", "expectReply": true },
                "b": { "@type": "Participant", "calendarAddress": "mailto:ann@example.invalid", "roles": { "owner": true }, "name": "Ann" },
            },
        })
    }

    #[test]
    fn maps_invitations_to_meeting_requests() {
        use crate::wbxml::cp::{email as e, email2 as e2};
        let request = meeting_request(&parsed_invite());
        let text = |tag| request.child_text(tag);
        assert_eq!(text(e::StartTime), Some("2026-11-05T13:00:00.000Z"));
        assert_eq!(text(e::EndTime), Some("2026-11-05T14:00:00.000Z"));
        assert_eq!(text(e::InstanceType), Some("1"));
        assert_eq!(text(e::Organizer), Some("\"Ann\" <ann@example.invalid>"));
        assert_eq!(text(e2::MeetingMessageType), Some("1"));
        let recurrence = request
            .find(e::Recurrences)
            .and_then(|r| r.find(e::Recurrence))
            .unwrap();
        assert_eq!(recurrence.child_text(e::Type), Some("1"));
        assert_eq!(recurrence.child_text(e::Occurrences), Some("3"));

        // The GlobalObjId carries the UID the way devices look it up.
        use base64::{Engine, engine::general_purpose::STANDARD};
        let id = STANDARD.decode(text(e::GlobalObjId).unwrap()).unwrap();
        assert_eq!(&id[..4], &[4, 0, 0, 0]);
        assert_eq!(
            u32::from_le_bytes(id[36..40].try_into().unwrap()) as usize,
            id.len() - 40
        );
        let expected = b"vCal-Uid\x01\0\0\0parse-test-1\0";
        assert!(id.ends_with(expected));
        assert_eq!(id.last(), Some(&0));
    }

    #[test]
    fn reads_jscalendar_2_participants() {
        let data = to_eas(&parsed_invite(), &plain(), "rob@example.com");
        assert_eq!(
            text(&data, cal::OrganizerEmail).as_deref(),
            Some("ann@example.invalid")
        );
        assert_eq!(text(&data, cal::MeetingStatus).as_deref(), Some("3"));
        assert_eq!(text(&data, cal::ResponseType).as_deref(), Some("5"));
        let attendees: Vec<&Element> = data.find(cal::Attendees).unwrap().elements().collect();
        assert_eq!(attendees.len(), 1);
        assert_eq!(attendees[0].child_text(cal::Email), Some("rob@example.com"));
        assert_eq!(
            own_participant_key(&parsed_invite(), "ROB@example.com"),
            Some("a")
        );
    }

    #[test]
    fn maps_meetings() {
        let event = json!({
            "title": "Review", "start": "2026-11-01T10:00:00", "duration": "PT1H",
            "participants": {
                "o": { "name": "Ann", "email": "ann@example.com", "roles": { "owner": true, "attendee": true } },
                "r": { "name": "Rob", "sendTo": { "imip": "mailto:rob@example.com" }, "roles": { "attendee": true },
                       "participationStatus": "tentative" },
                "x": { "email": "x@example.com", "roles": { "attendee": true, "optional": true } },
            },
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        assert_eq!(
            text(&data, cal::OrganizerEmail).as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(text(&data, cal::MeetingStatus).as_deref(), Some("3"));
        assert_eq!(text(&data, cal::ResponseType).as_deref(), Some("2"));
        let attendees: Vec<&Element> = data.find(cal::Attendees).unwrap().elements().collect();
        assert_eq!(attendees.len(), 2);
        let optional = attendees
            .iter()
            .find(|a| a.child_text(cal::Email) == Some("x@example.com"))
            .unwrap();
        assert_eq!(optional.child_text(cal::AttendeeType), Some("2"));
        assert_eq!(optional.child_text(cal::AttendeeStatus), Some("5"));

        let mine = to_eas(
            &json!({ "start": "2026-11-01T10:00:00", "participants": {
                "o": { "email": "rob@example.com", "roles": { "owner": true } },
                "a": { "email": "ann@example.com", "roles": { "attendee": true } },
            }}),
            &plain(),
            "rob@example.com",
        );
        assert_eq!(text(&mine, cal::MeetingStatus).as_deref(), Some("1"));
        assert!(mine.find(cal::ResponseType).is_none());
    }

    #[test]
    fn keeps_starts_in_the_repeated_hour_exact() {
        // 06:00 UTC on 1 Nov 2026 is the second 01:00 in New York.
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(
                cal::TimeZone,
                tz::eas_timezone(tz::zone_named("America/New_York").as_ref(), 2026),
            ))
            .child(Element::with_text(cal::Subject, "Late"))
            .child(Element::with_text(cal::StartTime, "20261101T060000Z"))
            .child(Element::with_text(cal::EndTime, "20261101T070000Z"));
        let event = Value::Object(from_eas(&data));
        let back = to_eas(&event, &plain(), "me@example.com");
        assert_eq!(back.child_text(cal::StartTime), Some("20261101T060000Z"));
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;
        use crate::eas::tz;

        proptest! {
            /// A timed event a device creates keeps its title, place and
            /// times through JSCalendar, in any common time zone.
            #[test]
            fn device_events_round_trip(
                subject in "[A-Za-z0-9 ]{1,30}",
                location in proptest::option::of("[A-Za-z0-9 ]{1,20}"),
                start_minutes in 0i64..(366 * 24 * 4),
                length_quarters in 1i64..(4 * 24 * 3),
                zone in prop::sample::select(vec![
                    "Europe/Amsterdam", "America/New_York", "Asia/Tokyo",
                    "Australia/Sydney", "America/Sao_Paulo", "UTC",
                ]),
            ) {
                let subject = subject.trim().to_owned();
                prop_assume!(!subject.is_empty());
                let tz = tz::zone_named(zone).unwrap();
                let start = tz::to_utc(jiff::civil::date(2026, 1, 1).at(0, 0, 0, 0), None)
                    + Duration::from_mins(15 * start_minutes);
                let end = start + Duration::from_mins(15 * length_quarters);
                let mut data = Element::new(airsync::ApplicationData)
                    .child(Element::with_text(cal::TimeZone, tz::eas_timezone(Some(&tz), 2026)))
                    .child(Element::with_text(cal::AllDayEvent, "0"))
                    .child(Element::with_text(cal::Subject, subject.as_str()))
                    .child(Element::with_text(cal::StartTime, tz::compact(start)))
                    .child(Element::with_text(cal::EndTime, tz::compact(end)));
                if let Some(location) = location.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
                    data.push(Element::with_text(cal::Location, location));
                }

                let mut event = from_eas(&data);
                event.insert("@type".to_owned(), "Event".into());
                let back = to_eas(&Value::Object(event), &plain(), "me@example.com");
                prop_assert_eq!(back.child_text(cal::Subject), Some(subject.as_str()));
                let (start, end) = (tz::compact(start), tz::compact(end));
                prop_assert_eq!(back.child_text(cal::StartTime), Some(start.as_str()));
                prop_assert_eq!(back.child_text(cal::EndTime), Some(end.as_str()));
                prop_assert_eq!(
                    back.child_text(cal::Location),
                    data.child_text(cal::Location)
                );
            }
        }
    }

    fn series() -> Value {
        json!({
            "uid": "u1",
            "title": "Review",
            "description": "Bring the numbers",
            "start": "2026-11-18T17:00:00",
            "timeZone": "Europe/Amsterdam",
            "duration": "PT1H",
            "alerts": { "a": { "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M" } } },
            "keywords": { "Work": true },
            "recurrenceRule": { "@type": "RecurrenceRule", "frequency": "weekly", "count": 6 },
            "recurrenceOverrides": {
                "2026-11-25T17:00:00": { "title": "Review (moved)", "start": "2026-11-25T18:00:00",
                    "description": "Other room" },
                "2026-12-02T17:00:00": { "excluded": true },
            },
        })
    }

    /// The device's form of `event` with `edit` applied.
    fn device(event: &Value, edit: impl FnOnce(&mut Element)) -> Element {
        let mut data = to_eas(event, &plain(), "rob@example.com");
        edit(&mut data);
        data
    }

    fn without(data: &mut Element, tags: &[crate::wbxml::Tag]) {
        data.children.retain(|n| match n {
            crate::wbxml::Node::Element(e) => !tags.contains(&e.tag),
            _ => true,
        });
    }

    fn set(data: &mut Element, tag: crate::wbxml::Tag, value: &str) {
        without(data, &[tag]);
        data.push(Element::with_text(tag, value));
    }

    /// An EAS 16 change to one occurrence, shaped like Gmail's (recorded
    /// session 038): every field of the occurrence, some empty.
    fn occurrence_change(edit: impl FnOnce(&mut Element)) -> Element {
        let mut data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::AllDayEvent, "0"))
            .child(Element::with_text(cal::StartTime, "20261118T160000Z"))
            .child(Element::with_text(cal::EndTime, "20261118T170000Z"))
            .child(Element::with_text(cal::Subject, "Review"))
            .child(
                Element::new(airsyncbase::Body)
                    .child(Element::with_text(airsyncbase::Type, "1"))
                    .child(Element::new(airsyncbase::Data)),
            )
            .child(Element::with_text(cal::Reminder, "15"))
            .child(Element::new(cal::Attendees))
            .child(Element::with_text(cal::BusyStatus, "2"))
            .child(Element::with_text(cal::MeetingStatus, "0"))
            .child(Element::with_text(cal::Sensitivity, "0"));
        edit(&mut data);
        data
    }

    #[test]
    fn an_occurrence_keeps_its_own_fields() {
        let event = series();
        let data = occurrence_change(|d| {
            set(d, cal::Reminder, "30");
            set(d, cal::BusyStatus, "0");
            set(d, cal::Sensitivity, "2");
            without(d, &[airsyncbase::Body]);
            d.push(
                Element::new(airsyncbase::Body)
                    .child(Element::with_text(airsyncbase::Type, "1"))
                    .child(Element::with_text(airsyncbase::Data, "Bring slides")),
            );
            d.push(
                Element::new(cal::Categories).child(Element::with_text(cal::Category, "Travel")),
            );
        });
        let (key, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        assert_eq!(key, "2026-11-18T17:00:00");
        assert_eq!(entry["description"], "Bring slides");
        assert_eq!(
            entry["alerts"]["a1"]["trigger"]["offset"], "-PT30M",
            "{entry}"
        );
        assert_eq!(entry["freeBusyStatus"], "free");
        assert!(
            entry.get("privacy").is_none(),
            "not in an override: {entry}"
        );
        assert_eq!(entry["keywords"], json!({ "Travel": true }));
        assert!(entry.get("participants").is_none(), "no attendees: {entry}");
    }

    #[test]
    fn a_sensitivity_of_one_occurrence_is_unsupported() {
        let event = series();
        let occurrence = |s: &str| occurrence_change(|d| set(d, cal::Sensitivity, s));
        // EAS 16: the series' sensitivity (Gmail always sends one) is fine.
        assert!(!occurrence_privacy(Some(&event), &occurrence("0"), true));
        assert!(occurrence_privacy(Some(&event), &occurrence("2"), true));
        assert!(!occurrence_privacy(
            Some(&event),
            &occurrence_change(|d| without(d, &[cal::Sensitivity])),
            true
        ));
        // EAS 14: an exception against the series as the device sends it.
        let with_exception = |series: &str, exception: &str| {
            device(&event, |d| {
                set(d, cal::Sensitivity, series);
                without(d, &[cal::Exceptions]);
                d.push(
                    Element::new(cal::Exceptions).child(
                        Element::new(cal::Exception)
                            .child(Element::with_text(
                                cal::ExceptionStartTime,
                                "20261125T160000Z",
                            ))
                            .child(Element::with_text(cal::Sensitivity, exception)),
                    ),
                );
            })
        };
        assert!(!occurrence_privacy(
            Some(&event),
            &with_exception("2", "2"),
            false
        ));
        assert!(occurrence_privacy(
            Some(&event),
            &with_exception("0", "2"),
            false
        ));
        assert!(occurrence_privacy(None, &with_exception("2", "0"), false));
    }

    #[test]
    fn an_occurrence_does_not_repeat_the_series() {
        let event = series();
        // As Gmail sends it: the series' values, an empty body.
        let data = occurrence_change(|_| {});
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        for key in OCCURRENCE_FIELDS {
            assert!(entry.get(*key).is_none(), "{key} in {entry}");
        }
        // No reminder for this occurrence is one of its own.
        let data = occurrence_change(|d| {
            without(d, &[cal::Reminder]);
            d.push(Element::new(cal::Reminder));
        });
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        assert_eq!(entry["alerts"], Value::Null);
        assert!(entry.as_object().unwrap().contains_key("alerts"));
        // Back to the series' description: written over the occurrence's own.
        let moved = Element::with_text(airsyncbase::Data, "Bring the numbers");
        let data = occurrence_change(|d| {
            without(d, &[airsyncbase::Body]);
            d.push(
                Element::new(airsyncbase::Body)
                    .child(Element::with_text(airsyncbase::Type, "1"))
                    .child(moved),
            );
        });
        let (key, entry) =
            occurrence_override(&event, "20261125T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        assert_eq!(key, "2026-11-25T17:00:00");
        assert_eq!(entry["description"], "Bring the numbers");
    }

    #[test]
    fn occurrence_fields_are_shown_and_read_back() {
        let mut event = series();
        event["recurrenceOverrides"]["2026-11-25T17:00:00"] = json!({
            "title": "Review (moved)", "start": "2026-11-25T18:00:00",
            "description": "Other room",
            "locations": { "x": { "name": "Room 2" } },
            "alerts": { "b": { "trigger": { "@type": "OffsetTrigger", "offset": "-PT1H" } } },
            "freeBusyStatus": "free", "keywords": { "Travel": true },
            "participants/p1/participationStatus": "declined",
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        let exception = data
            .find(cal::Exceptions)
            .unwrap()
            .find_all(cal::Exception)
            .find(|e| e.child_text(cal::ExceptionStartTime) == Some("20261125T160000Z"))
            .unwrap();
        assert_eq!(text(exception, cal::Location).as_deref(), Some("Room 2"));
        assert_eq!(text(exception, cal::Reminder).as_deref(), Some("60"));
        assert_eq!(text(exception, cal::BusyStatus).as_deref(), Some("0"));
        assert_eq!(
            exception
                .find(airsyncbase::Body)
                .and_then(|b| b.child_text(airsyncbase::Data)),
            Some("Other room")
        );
        assert_eq!(
            exception
                .find(cal::Categories)
                .and_then(|c| c.child_text(cal::Category)),
            Some("Travel")
        );
        assert!(
            exception.find(cal::Attendees).is_none(),
            "a participant without an address"
        );
        // Read back, the occurrence is what it was (apart from the path).
        let read = from_eas(&data);
        let entry = &read["recurrenceOverrides"]["2026-11-25T17:00:00"];
        assert_eq!(entry["title"], "Review (moved)");
        assert_eq!(entry["description"], "Other room");
        assert_eq!(entry["freeBusyStatus"], "free");
        assert_eq!(entry["keywords"], json!({ "Travel": true }));
        assert_eq!(entry["alerts"]["a1"]["trigger"]["offset"], "-PT1H");
    }

    /// Stalwart stores an override as a minimal patch, with paths into the
    /// series' values; such an occurrence is shown and merged like any other.
    #[test]
    fn an_override_with_paths_is_shown_and_merged() {
        let mut event = series();
        event["recurrenceOverrides"]["2026-12-09T17:00:00"] = json!({
            "alerts/a/trigger": { "@type": "OffsetTrigger", "offset": "-PT45M" },
            "description": "Bring the slides",
            "keywords/Travel": true,
        });
        let data = to_eas(&event, &plain(), "rob@example.com");
        let exception = data
            .find(cal::Exceptions)
            .unwrap()
            .find_all(cal::Exception)
            .find(|e| e.child_text(cal::ExceptionStartTime) == Some("20261209T160000Z"))
            .unwrap();
        assert_eq!(text(exception, cal::Reminder).as_deref(), Some("45"));
        assert_eq!(
            text(exception, cal::StartTime).as_deref(),
            Some("20261209T160000Z"),
            "at its recurrence id"
        );
        assert_eq!(
            text(exception, cal::EndTime).as_deref(),
            Some("20261209T170000Z")
        );
        let categories: Vec<&str> = exception
            .find(cal::Categories)
            .unwrap()
            .find_all(cal::Category)
            .filter_map(Element::text)
            .collect();
        assert_eq!(categories, ["Travel", "Work"]);
        assert!(
            exception.child_text(cal::Subject).is_none(),
            "the series' title"
        );
        assert!(
            exception.find(cal::BusyStatus).is_none(),
            "the series' status"
        );

        // A new reminder replaces the path below it.
        let old = event["recurrenceOverrides"]["2026-12-09T17:00:00"]
            .as_object()
            .unwrap();
        let new = json!({ "alerts": { "a1": { "trigger": { "offset": "-PT5M" } } } });
        let merged = merge_override(old, new.as_object().unwrap().clone());
        let keys: Vec<&String> = merged.keys().collect();
        assert_eq!(
            keys,
            ["alerts", "description", "keywords/Travel"],
            "{merged:?}"
        );

        // A removed field and an escaped path segment.
        let patched = apply_patch(
            &json!({ "title": "A", "links": { "a/b": { "href": "x" } } }),
            &json!({ "title": null, "links/a~1b/href": "y" }),
        );
        assert_eq!(patched, json!({ "links": { "a/b": { "href": "y" } } }));
    }

    #[test]
    fn eas14_an_exception_sent_back_truncated_keeps_its_description() {
        let mut event = series();
        event["recurrenceOverrides"]["2026-11-25T17:00:00"]["description"] =
            json!("A long note about the room ".repeat(20));
        let short = BodyPreference {
            truncation_size: Some(40),
            ..plain()
        };
        let mut data = to_eas(&event, &short, "rob@example.com");
        // Rename the moved occurrence, the rest as the device was shown.
        let exceptions = data
            .children
            .iter_mut()
            .find_map(|n| match n {
                crate::wbxml::Node::Element(e) if e.tag == cal::Exceptions => Some(e),
                _ => None,
            })
            .unwrap();
        for node in &mut exceptions.children {
            if let crate::wbxml::Node::Element(e) = node
                && e.child_text(cal::ExceptionStartTime) == Some("20261125T160000Z")
            {
                set(e, cal::Subject, "Review (room 2)");
            }
        }
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &short);
        let entry = &patch["recurrenceOverrides/2026-11-25T17:00:00"];
        assert_eq!(entry["title"], "Review (room 2)");
        assert_eq!(
            entry["description"],
            event["recurrenceOverrides"]["2026-11-25T17:00:00"]["description"],
            "not cut: {entry}"
        );
        assert!(
            !patch.contains_key("recurrenceOverrides/2026-12-02T17:00:00"),
            "{patch:?}"
        );
    }

    #[test]
    fn the_user_is_matched_under_every_address() {
        let own = "rob@example.com,robert@example.org";
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(
                cal::OrganizerEmail,
                "robert@example.org",
            ))
            .child(
                Element::new(cal::Attendees)
                    .child(
                        Element::new(cal::Attendee)
                            .child(Element::with_text(cal::Email, "Robert@example.org")),
                    )
                    .child(
                        Element::new(cal::Attendee)
                            .child(Element::with_text(cal::Email, "ann@example.com")),
                    ),
            );
        let attendees = attendees_from_eas(&data, own).unwrap();
        assert_eq!(attendees.len(), 1, "{attendees:?}");
        assert_eq!(attendees[0].0, "ann@example.com");
        // The user's entry under an alias is kept, not added again.
        let existing = json!({ "me": { "calendarAddress": "mailto:robert@example.org",
            "roles": { "owner": true }, "participationStatus": "accepted" } });
        let participants = participants(own, &attendees, &existing);
        assert_eq!(participants.as_object().unwrap().len(), 2, "{participants}");
        assert!(participants["me"].is_object());
        // A new organizer entry uses the primary address.
        let participants = super::participants(own, &attendees, &Value::Null);
        assert_eq!(
            participants["owner"]["calendarAddress"],
            "mailto:rob@example.com"
        );
    }

    #[test]
    fn an_occurrence_gets_its_own_attendees() {
        let mut event = series();
        event["participants"] = json!({
            "o": { "calendarAddress": "mailto:rob@example.com", "roles": { "owner": true } },
            "a": { "calendarAddress": "mailto:ann@example.com", "roles": { "attendee": true },
                "participationStatus": "accepted" },
        });
        let attendee =
            |email: &str| Element::new(cal::Attendee).child(Element::with_text(cal::Email, email));
        let with = |list: Element| {
            occurrence_change(move |d| {
                without(d, &[cal::Attendees]);
                d.push(list);
            })
        };
        // The same attendees: nothing for the occurrence.
        let same = with(Element::new(cal::Attendees).child(attendee("ann@example.com")));
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&same), "rob@example.com")
                .unwrap();
        assert!(entry.get("participants").is_none(), "{entry}");
        // One more for this occurrence.
        let more = with(
            Element::new(cal::Attendees)
                .child(attendee("ann@example.com"))
                .child(attendee("bob@example.com")),
        );
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&more), "rob@example.com")
                .unwrap();
        let participants = entry["participants"].as_object().unwrap();
        assert_eq!(participants.len(), 3, "{entry}");
        assert_eq!(participants["a"]["participationStatus"], "accepted");
    }

    /// The case from #24: the series moved half an hour later on the phone,
    /// which sends its occurrences under their new original starts.
    #[test]
    fn moving_the_series_moves_its_occurrences() {
        let event = series();
        let exception = |original: &str, fields: &[(Tag, &str)]| {
            fields.iter().fold(
                Element::new(cal::Exception)
                    .child(Element::with_text(cal::ExceptionStartTime, original)),
                |e, (tag, value)| e.child(Element::with_text(*tag, *value)),
            )
        };
        let data = device(&event, |d| {
            set(d, cal::StartTime, "20261118T163000Z");
            set(d, cal::EndTime, "20261118T173000Z");
            without(d, &[cal::Exceptions]);
            d.push(
                Element::new(cal::Exceptions)
                    .child(exception(
                        "20261125T163000Z",
                        &[
                            (cal::Subject, "Review (moved)"),
                            (cal::StartTime, "20261125T170000Z"),
                            (cal::EndTime, "20261125T180000Z"),
                        ],
                    ))
                    .child(exception("20261202T163000Z", &[(cal::Deleted, "1")])),
            );
        });
        for version in [141, 161] {
            let patch = change_from_eas(&event, &data, version, None, "rob@example.com", &plain());
            assert_eq!(patch["start"], "2026-11-18T17:30:00");
            let overrides = patch["recurrenceOverrides"].as_object().unwrap();
            let keys: Vec<&String> = overrides.keys().collect();
            assert_eq!(
                keys,
                ["2026-11-25T17:30:00", "2026-12-02T17:30:00"],
                "{patch:?}"
            );
            let moved = &overrides["2026-11-25T17:30:00"];
            assert_eq!(moved["start"], "2026-11-25T18:00:00");
            assert_eq!(moved["description"], "Other room", "its own fields stay");
            assert_eq!(overrides["2026-12-02T17:30:00"]["excluded"], true);
            assert!(
                !patch.keys().any(|k| k.starts_with("recurrenceOverrides/")),
                "{patch:?}"
            );
        }
        // A device that still names the occurrences by their old starts.
        let stale = device(&event, |d| {
            set(d, cal::StartTime, "20261118T163000Z");
            set(d, cal::EndTime, "20261118T173000Z");
        });
        let patch = change_from_eas(&event, &stale, 141, None, "rob@example.com", &plain());
        let overrides = patch["recurrenceOverrides"].as_object().unwrap();
        let keys: Vec<&String> = overrides.keys().collect();
        assert_eq!(
            keys,
            ["2026-11-25T17:30:00", "2026-12-02T17:30:00"],
            "{patch:?}"
        );
        assert_eq!(
            overrides["2026-11-25T17:30:00"]["description"],
            "Other room"
        );
        // EAS 16 with `<Exceptions>` left out: the occurrences move too.
        let mut data = data.clone();
        without(&mut data, &[cal::Exceptions]);
        let patch = change_from_eas(&event, &data, 161, None, "rob@example.com", &plain());
        let overrides = patch["recurrenceOverrides"].as_object().unwrap();
        assert_eq!(overrides["2026-11-25T17:30:00"]["title"], "Review (moved)");
        assert_eq!(overrides.len(), 2);
        // Only the end moving keeps the keys.
        let data = device(&event, |d| set(d, cal::EndTime, "20261118T173000Z"));
        let patch = change_from_eas(&event, &data, 161, None, "rob@example.com", &plain());
        assert!(!patch.contains_key("recurrenceOverrides"), "{patch:?}");
    }

    /// A time change sent with the zone's rules but no names (as iOS sends
    /// it) keeps the event's own zone name; another zone is taken.
    #[test]
    fn a_time_change_keeps_the_zone_name() {
        use base64::Engine;
        let mut event = series();
        event["timeZone"] = json!("Europe/Zurich");
        let nameless = |zone: &str| {
            let blob = tz::eas_timezone(tz::zone_named(zone).as_ref(), 2026);
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .unwrap();
            bytes[4..68].fill(0);
            bytes[88..152].fill(0);
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let data = device(&event, |d| {
            set(d, cal::StartTime, "20261118T163000Z");
            set(d, cal::EndTime, "20261118T173000Z");
            set(d, cal::TimeZone, &nameless("Europe/Zurich"));
        });
        let patch = change_from_eas(&event, &data, 161, None, "rob@example.com", &plain());
        assert_eq!(patch["start"], "2026-11-18T17:30:00");
        assert_eq!(patch["timeZone"], "Europe/Zurich");
        let data = device(&event, |d| {
            set(d, cal::StartTime, "20261118T163000Z");
            set(d, cal::EndTime, "20261118T173000Z");
            set(d, cal::TimeZone, &nameless("Europe/London"));
        });
        let patch = change_from_eas(&event, &data, 161, None, "rob@example.com", &plain());
        assert_eq!(patch["timeZone"], "Europe/London");
        assert_eq!(patch["start"], "2026-11-18T16:30:00");
    }

    #[test]
    fn eas14_changes_clear_what_is_left_out_except_body_and_exceptions() {
        let event = series();
        let data = device(&event, |d| {
            without(d, &[airsyncbase::Body, cal::Exceptions, cal::Reminder]);
            set(d, cal::Subject, "Review 2");
        });
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        assert_eq!(patch["title"], "Review 2");
        assert_eq!(patch["alerts"], Value::Null);
        assert!(!patch.contains_key("description"));
        assert!(patch.keys().all(|k| !k.starts_with("recurrenceOverrides")));
        // Sent back as shown: not written.
        assert!(!patch.contains_key("recurrenceRule"));
    }

    #[test]
    fn fields_sent_back_as_shown_are_not_written() {
        let mut event = series();
        event["alerts"] = json!({
            "a": { "trigger": { "@type": "OffsetTrigger", "offset": "-PT15M" } },
            "b": { "trigger": { "@type": "AbsoluteTrigger", "when": "2026-11-18T08:00:00Z" } },
        });
        event["locations"] = json!({ "l1": { "name": "Room 1" }, "l2": { "name": "Online" } });
        event["privacy"] = json!("secret");
        // Like iOS: the whole event back, only the subject changed.
        let data = device(&event, |d| set(d, cal::Subject, "Review 2"));
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        let keys: Vec<&String> = patch.keys().collect();
        assert_eq!(keys, ["title"], "{patch:?}");
    }

    #[test]
    fn eas14_rules_eas_cannot_show_stay() {
        let mut event = series();
        event["recurrenceRule"] = json!({ "@type": "RecurrenceRule", "frequency": "monthly",
            "byDay": [{ "day": "mo" }, { "day": "tu" }], "bySetPosition": [-1] });
        let data = device(&event, |d| set(d, cal::Subject, "Review 2"));
        assert!(data.find(cal::Recurrence).is_none(), "not shown");
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        assert!(!patch.contains_key("recurrenceRule"));
        assert!(!patch.contains_key("recurrenceOverrides"));
    }

    #[test]
    fn a_time_zone_sent_back_without_names_is_not_written() {
        use base64::Engine;
        let event = series();
        let data = device(&event, |d| {
            let blob = d.child_text(cal::TimeZone).unwrap().to_owned();
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .unwrap();
            // StandardName and DaylightName, as iOS sends them: empty.
            bytes[4..68].fill(0);
            bytes[88..152].fill(0);
            set(
                d,
                cal::TimeZone,
                &base64::engine::general_purpose::STANDARD.encode(bytes),
            );
        });
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        assert!(patch.is_empty(), "{patch:?}");
    }

    #[test]
    fn a_truncated_description_sent_back_is_not_written() {
        let event = series();
        let short = BodyPreference {
            truncation_size: Some(5),
            ..plain()
        };
        let data = {
            let mut data = to_eas(&event, &short, "rob@example.com");
            set(&mut data, cal::Subject, "Review 2");
            data
        };
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &short);
        assert!(!patch.contains_key("description"), "{patch:?}");
    }

    #[test]
    fn cancelled_meetings_are_marked() {
        let meeting = |organizer: &str| {
            json!({
                "title": "Review", "start": "2026-11-18T17:00:00", "duration": "PT1H",
                "status": "cancelled",
                "participants": {
                    "o": { "roles": { "owner": true }, "sendTo": { "imip": format!("mailto:{organizer}") } },
                    "a": { "roles": { "attendee": true }, "sendTo": { "imip": "mailto:ann@example.com" } },
                },
            })
        };
        let mine = to_eas(&meeting("rob@example.com"), &plain(), "rob@example.com");
        assert_eq!(text(&mine, cal::MeetingStatus).as_deref(), Some("5"));
        let theirs = to_eas(&meeting("eve@example.com"), &plain(), "rob@example.com");
        assert_eq!(text(&theirs, cal::MeetingStatus).as_deref(), Some("7"));
    }

    #[test]
    fn eas14_changes_keep_fields_the_device_does_not_support() {
        let event = series();
        let data = device(&event, |d| without(d, &[cal::Reminder, cal::Categories]));
        let supported = [cal::Subject.to_string(), cal::Reminder.to_string()];
        let patch = change_from_eas(
            &event,
            &data,
            141,
            Some(&supported),
            "rob@example.com",
            &plain(),
        );
        // Supported but left out: cleared. Not supported: unchanged.
        assert_eq!(patch["alerts"], Value::Null);
        assert!(!patch.contains_key("keywords"));
    }

    #[test]
    fn eas16_changes_write_only_what_the_device_sent() {
        let event = series();
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(cal::StartTime, "20261118T150000Z"))
            .child(Element::with_text(cal::EndTime, "20261118T170000Z"));
        let patch = change_from_eas(&event, &data, 161, None, "rob@example.com", &plain());
        assert_eq!(patch["start"], "2026-11-18T16:00:00");
        assert_eq!(patch["duration"], "PT2H");
        assert_eq!(patch["timeZone"], "Europe/Amsterdam");
        for key in [
            "title",
            "description",
            "alerts",
            "keywords",
            "recurrenceRule",
        ] {
            assert!(!patch.contains_key(key), "{key}: {patch:?}");
        }
    }

    #[test]
    fn sent_exceptions_merge_and_others_stay() {
        let event = series();
        let data = device(&event, |d| {
            without(d, &[cal::Exceptions]);
            d.push(
                Element::new(cal::Exceptions).child(
                    Element::new(cal::Exception)
                        .child(Element::with_text(
                            cal::ExceptionStartTime,
                            "20261125T160000Z",
                        ))
                        .child(Element::with_text(cal::Subject, "Review (room 2)"))
                        .child(Element::with_text(cal::StartTime, "20261125T170000Z"))
                        .child(Element::with_text(cal::EndTime, "20261125T180000Z")),
                ),
            );
        });
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        let moved = &patch["recurrenceOverrides/2026-11-25T17:00:00"];
        assert_eq!(moved["title"], "Review (room 2)");
        assert_eq!(moved["description"], "Other room");
        assert!(!patch.contains_key("recurrenceOverrides"));
        assert!(!patch.contains_key("recurrenceOverrides/2026-12-02T17:00:00"));
    }

    #[test]
    fn first_exceptions_are_written_whole() {
        let mut event = series();
        event.as_object_mut().unwrap().remove("recurrenceOverrides");
        let data = device(&series(), |_| {});
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        assert_eq!(
            patch["recurrenceOverrides"]["2026-12-02T17:00:00"]["excluded"],
            true
        );
        assert!(patch.keys().all(|k| !k.starts_with("recurrenceOverrides/")));
    }

    #[test]
    fn a_series_made_single_drops_its_occurrences() {
        let event = series();
        let data = device(&event, |d| without(d, &[cal::Recurrence, cal::Exceptions]));
        let patch = change_from_eas(&event, &data, 141, None, "rob@example.com", &plain());
        assert_eq!(patch["recurrenceRule"], Value::Null);
        assert_eq!(patch["recurrenceOverrides"], Value::Null);
    }

    fn body(body_type: &str, content: &str) -> Element {
        Element::new(airsyncbase::Body)
            .child(Element::with_text(airsyncbase::Type, body_type))
            .child(Element::with_text(airsyncbase::Data, content))
    }

    #[test]
    fn an_html_description_stays_html() {
        let event = json!({
            "uid": "u1", "title": "Plan", "start": "2026-11-18T17:00:00",
            "timeZone": "Europe/Amsterdam", "duration": "PT1H",
            "description": "<p>Bring <b>slides</b></p>",
            "descriptionContentType": "text/html",
        });
        let own = "rob@example.com";
        // A device that reads HTML gets it as it is; one that reads text
        // gets it as text.
        let html = BodyPreference {
            body_type: body_type::HTML,
            ..plain()
        };
        let shown = to_eas(&event, &html, own);
        let shown_body = shown.find(airsyncbase::Body).unwrap();
        assert_eq!(shown_body.child_text(airsyncbase::Type), Some("2"));
        assert_eq!(
            shown_body.child_text(airsyncbase::Data),
            Some("<p>Bring <b>slides</b></p>")
        );
        let mut data = to_eas(&event, &plain(), own);
        let text_body = data.find(airsyncbase::Body).unwrap();
        assert_eq!(text_body.child_text(airsyncbase::Type), Some("1"));
        assert_eq!(
            text_body.child_text(airsyncbase::Data).map(str::trim),
            Some("Bring slides")
        );
        // Sent back as shown, by either: the HTML stays.
        for (data, pref) in [(&shown, &html), (&data, &plain())] {
            let patch = change_from_eas(&event, data, 160, None, own, pref);
            assert!(patch.is_empty(), "{patch:?}");
        }
        // A new text is written as text.
        without(&mut data, &[airsyncbase::Body]);
        data.push(body("1", "Bring the numbers"));
        let patch = change_from_eas(&event, &data, 160, None, own, &plain());
        assert_eq!(patch["description"], "Bring the numbers");
        assert_eq!(patch["descriptionContentType"], Value::Null);
        // HTML from the device is written as HTML.
        without(&mut data, &[airsyncbase::Body]);
        data.push(body("2", "<p>Bring <i>numbers</i></p>"));
        let patch = change_from_eas(&event, &data, 160, None, own, &plain());
        assert_eq!(patch["description"], "<p>Bring <i>numbers</i></p>");
        assert_eq!(patch["descriptionContentType"], "text/html");
    }

    #[test]
    fn an_occurrence_keeps_the_type_of_its_description() {
        let event = series();
        let data = occurrence_change(|d| {
            without(d, &[airsyncbase::Body]);
            d.push(body("2", "<p>Other <b>room</b></p>"));
        });
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        assert_eq!(entry["description"], "<p>Other <b>room</b></p>");
        assert_eq!(entry["descriptionContentType"], "text/html");
        // Text like the series' needs no type of its own.
        let data = occurrence_change(|d| {
            without(d, &[airsyncbase::Body]);
            d.push(body("1", "Other room"));
        });
        let (_, entry) =
            occurrence_override(&event, "20261118T160000Z", Some(&data), "rob@example.com")
                .unwrap();
        assert_eq!(entry["description"], "Other room");
        assert!(entry.get("descriptionContentType").is_none(), "{entry}");
        // An HTML occurrence is shown as HTML to a device that reads it.
        let mut event = series();
        event["recurrenceOverrides"]["2026-11-25T17:00:00"]["description"] =
            "<p>Other <b>room</b></p>".into();
        event["recurrenceOverrides"]["2026-11-25T17:00:00"]["descriptionContentType"] =
            "text/html".into();
        let html = BodyPreference {
            body_type: body_type::HTML,
            ..plain()
        };
        let data = to_eas(&event, &html, "rob@example.com");
        let exception = data
            .find(cal::Exceptions)
            .unwrap()
            .find_all(cal::Exception)
            .find(|e| e.find(airsyncbase::Body).is_some())
            .unwrap();
        assert_eq!(
            exception
                .find(airsyncbase::Body)
                .and_then(|b| b.child_text(airsyncbase::Data)),
            Some("<p>Other <b>room</b></p>")
        );
    }

    #[test]
    fn bodies_a_description_cannot_be_are_reported_not_read() {
        let mut data = to_eas(&series(), &plain(), "rob@example.com");
        without(&mut data, &[airsyncbase::Body]);
        data.push(body("3", "e1xydGYxfQ=="));
        assert_eq!(unsupported_bodies(&data), ["Body of type 3 (RTF)"]);
        assert_eq!(from_eas(&data)["description"], Value::Null);
        // In an exception too.
        let mut data = to_eas(&series(), &plain(), "rob@example.com");
        without(&mut data, &[cal::Exceptions]);
        data.push(
            Element::new(cal::Exceptions).child(
                Element::new(cal::Exception)
                    .child(Element::with_text(
                        cal::ExceptionStartTime,
                        "20261209T160000Z",
                    ))
                    .child(body("0", "hi")),
            ),
        );
        assert_eq!(unsupported_bodies(&data), ["Body of an unknown type"]);
    }

    #[test]
    fn a_mime_body_gives_its_html_or_text() {
        let alternative = "Content-Type: multipart/alternative; boundary=b\r\n\
            \r\n\
            --b\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            Bring slides\r\n\
            --b\r\n\
            Content-Type: text/html\r\n\
            \r\n\
            <p>Bring <b>slides</b></p>\r\n\
            --b--\r\n";
        let mut data = to_eas(&series(), &plain(), "rob@example.com");
        without(&mut data, &[airsyncbase::Body, cal::Exceptions]);
        data.push(body("4", alternative));
        assert!(unsupported_bodies(&data).is_empty());
        let event = from_eas(&data);
        assert_eq!(
            event["description"].as_str().map(str::trim),
            Some("<p>Bring <b>slides</b></p>")
        );
        assert_eq!(event["descriptionContentType"], "text/html");
        without(&mut data, &[airsyncbase::Body]);
        data.push(body(
            "4",
            "Content-Type: text/plain\r\n\r\nBring slides\r\n",
        ));
        let event = from_eas(&data);
        assert_eq!(
            event["description"].as_str().map(str::trim),
            Some("Bring slides")
        );
        assert_eq!(event["descriptionContentType"], Value::Null);
        // A description has no place for files.
        let with_file = "Content-Type: multipart/mixed; boundary=b\r\n\
            \r\n\
            --b\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            See the plan\r\n\
            --b\r\n\
            Content-Type: application/pdf; name=plan.pdf\r\n\
            Content-Disposition: attachment; filename=plan.pdf\r\n\
            \r\n\
            %PDF-\r\n\
            --b--\r\n";
        without(&mut data, &[airsyncbase::Body]);
        data.push(body("4", with_file));
        assert_eq!(
            unsupported_bodies(&data),
            ["Body of type 4 (MIME) with attachments"]
        );
    }
}
