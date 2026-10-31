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

use chrono::{DateTime, Datelike, Duration, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use super::{
    email::{self, BodyPreference, body_type},
    tz,
};
use crate::wbxml::{
    Element,
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
            tz: event["timeZone"]
                .as_str()
                .and_then(|name| name.parse().ok()),
            all_day,
            duration: event["duration"]
                .as_str()
                .and_then(tz::parse_duration)
                .unwrap_or(if all_day {
                    Duration::days(1)
                } else {
                    Duration::zero()
                }),
        }
    }

    /// Start and end in UTC for a local start; all-day events span whole
    /// days from local midnight.
    fn span(&self, start: NaiveDateTime, duration: Duration) -> (DateTime<Utc>, DateTime<Utc>) {
        if self.all_day {
            let day = start.date().and_time(NaiveTime::MIN);
            let days = duration.num_days().max(1);
            (
                tz::to_utc(day, self.tz),
                tz::to_utc(day + Duration::days(days), self.tz),
            )
        } else {
            let begin = tz::to_utc(start, self.tz);
            (begin, begin + duration)
        }
    }
}

/// The event's `ApplicationData`. `own_email` tells meetings the user
/// organizes from invitations they received.
pub fn to_eas(event: &Value, body: &BodyPreference, own_email: &str) -> Element {
    let mut data = Element::new(airsync::ApplicationData);
    let timing = Timing::of(event);
    let start = event["start"].as_str().and_then(tz::parse_local);
    let year = start.map_or_else(|| Utc::now().year(), |s| s.year().max(Utc::now().year()));

    data.push(Element::with_text(
        cal::TimeZone,
        tz::eas_timezone(timing.tz, year),
    ));
    data.push(Element::with_text(
        cal::AllDayEvent,
        if timing.all_day { "1" } else { "0" },
    ));
    if let Some(description) = event["description"].as_str().filter(|d| !d.is_empty()) {
        data.push(body_element(description, body));
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
        match (is_meeting, organized_by_me) {
            (false, _) => meeting::APPOINTMENT,
            (true, true) => meeting::ORGANIZER,
            (true, false) => meeting::RECEIVED,
        },
    ));
    if !attendees.is_empty() {
        let list = attendees
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
            });
        data.push(list);
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
        if let Some(exceptions) = exceptions(event, &timing) {
            data.push(exceptions);
        }
    }
    data
}

fn body_element(text: &str, pref: &BodyPreference) -> Element {
    let (kind, mut data) = match pref.body_type {
        body_type::HTML => (body_type::HTML, email::text_to_html(text)),
        _ => (body_type::PLAIN, text.to_owned()),
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

fn utc_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.with_timezone(&Utc))
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
        (offset <= Duration::zero()).then(|| -offset.num_minutes())
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
fn recurrence(event: &Value, start: NaiveDateTime, timing: &Timing) -> Option<Element> {
    let rule = match (&event["recurrenceRule"], &event["recurrenceRules"]) {
        (rule @ Value::Object(_), _) => rule,
        (_, Value::Array(rules)) if rules.len() == 1 => &rules[0],
        _ => return None,
    };
    if rule.get("bySetPosition").is_some() || rule.get("byHour").is_some() {
        return None;
    }
    let by_day: Vec<(u32, Option<i64>)> = rule["byDay"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| Some((weekday_bit(d["day"].as_str()?)?, d["nthOfPeriod"].as_i64())))
        .collect();
    let day_mask = by_day.iter().fold(0, |mask, (bit, _)| mask | bit);
    let nth = by_day
        .iter()
        .find_map(|(bit, n)| n.map(|n| (*bit, if n < 0 { 5 } else { n })));
    let month_day = rule["byMonthDay"]
        .as_array()
        .and_then(|d| d.first())
        .and_then(Value::as_i64);
    let month = rule["byMonth"]
        .as_array()
        .and_then(|m| m.first())
        .and_then(|m| m.as_str()?.trim_end_matches('L').parse::<u32>().ok())
        .unwrap_or(start.month());
    let start_bit = 1 << start.weekday().num_days_from_sunday();

    let mut fields: Vec<(crate::wbxml::Tag, String)> = Vec::new();
    let kind = match rule["frequency"].as_str()? {
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
            tz::compact(tz::to_utc(until, timing.tz)),
        ));
    }
    for (tag, value) in fields {
        recurrence.push(Element::with_text(tag, value));
    }
    Some(recurrence)
}

/// `recurrenceOverrides` as EAS exceptions: removed occurrences, and moved
/// or renamed ones.
fn exceptions(event: &Value, timing: &Timing) -> Option<Element> {
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
            let start = patch["start"]
                .as_str()
                .and_then(tz::parse_local)
                .unwrap_or(original);
            let duration = patch["duration"]
                .as_str()
                .and_then(tz::parse_duration)
                .unwrap_or(timing.duration);
            let (begin, end) = timing.span(start, duration);
            if let Some(title) = patch["title"].as_str() {
                exception.push(Element::with_text(cal::Subject, title));
            }
            exception.push(Element::with_text(cal::StartTime, tz::compact(begin)));
            exception.push(Element::with_text(cal::EndTime, tz::compact(end)));
        }
        list.push(exception);
    }
    (!list.children.is_empty()).then_some(list)
}

/// The `<email:MeetingRequest>` of an invitation email, so the device shows
/// it as a meeting request it can answer (MS-ASEMAIL 2.2.2.48).
pub fn meeting_request(event: &Value) -> Element {
    use crate::wbxml::cp::{email as e, email2 as e2};
    let timing = Timing::of(event);
    let start = event["start"].as_str().and_then(tz::parse_local);
    let year = start.map_or_else(|| Utc::now().year(), |s| s.year().max(Utc::now().year()));
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
        tz::eas_timezone(timing.tz, year),
    ));
    if let Some(uid) = event["uid"].as_str() {
        request.push(Element::with_text(e::GlobalObjId, global_obj_id(uid)));
    }
    request.push(Element::with_text(e::DisallowNewTimeProposal, "1"));
    request.push(Element::with_text(e2::MeetingMessageType, "1"));
    request
}

/// EAS email dates: `2026-11-05T13:00:00.000Z`.
fn extended(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
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
fn parse_eas_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|t| Utc.from_utc_datetime(&t))
        .or_else(|| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })
}

fn local_in(time: DateTime<Utc>, tz: Option<Tz>) -> NaiveDateTime {
    match tz {
        Some(tz) => time.with_timezone(&tz).naive_local(),
        None => time.naive_utc(),
    }
}

fn local_string(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// An ISO 8601 duration such as `PT1H30M` or `P2D`.
fn format_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
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
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    tz: Option<Tz>,
    all_day: bool,
) -> (NaiveDateTime, Duration) {
    let (local_start, local_end) = (local_in(start, tz), local_in(end, tz));
    if all_day {
        let first = local_start.date();
        // A start just before local midnight is a device rounding issue.
        let first = if local_start.time() >= NaiveTime::from_hms_opt(12, 0, 0).unwrap() {
            first.succ_opt().unwrap_or(first)
        } else {
            first
        };
        let days = (local_end.date() - first).num_days().max(1);
        (first.and_time(NaiveTime::MIN), Duration::days(days))
    } else {
        (local_start, end - start)
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
        && tz::to_utc(local_in(start, tz), tz) != start
    {
        tz = None;
    }
    event.insert(
        "timeZone".to_owned(),
        tz.map_or("Etc/UTC", |tz| tz.name()).into(),
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
        let (local_start, duration) = timing_from_eas(start, end, tz, all_day);
        event.insert("start".to_owned(), local_string(local_start).into());
        event.insert("duration".to_owned(), format_duration(duration).into());
        if let Some(recurrence) = data.find(cal::Recurrence) {
            event.insert(
                "recurrenceRule".to_owned(),
                recurrence_from_eas(recurrence, tz).unwrap_or(Value::Null),
            );
        }
        if let Some(exceptions) = data.find(cal::Exceptions) {
            let overrides = overrides_from_eas(exceptions, tz, all_day);
            if !overrides.is_empty() {
                event.insert("recurrenceOverrides".to_owned(), Value::Object(overrides));
            }
        }
    }
    if let Some(description) = data
        .find(airsyncbase::Body)
        .and_then(|b| b.child_text(airsyncbase::Data))
        .filter(|d| !d.trim().is_empty())
    {
        event.insert("description".to_owned(), description.into());
    }
    if let Some(location) = text(cal::Location) {
        event.insert(
            "locations".to_owned(),
            json!({ "l1": { "@type": "Location", "name": location } }),
        );
    }
    if let Some(minutes) = text(cal::Reminder).and_then(|m| m.parse::<i64>().ok()) {
        event.insert(
            "alerts".to_owned(),
            json!({ "a1": { "@type": "Alert", "action": "display",
                "trigger": { "@type": "OffsetTrigger", "offset": format!("-{}", format_duration(Duration::minutes(minutes))) } } }),
        );
    }
    event.insert(
        "freeBusyStatus".to_owned(),
        (if text(cal::BusyStatus) == Some(busy::FREE) {
            "free"
        } else {
            "busy"
        })
        .into(),
    );
    event.insert(
        "privacy".to_owned(),
        (if matches!(text(cal::Sensitivity), Some("2" | "3")) {
            "private"
        } else {
            "public"
        })
        .into(),
    );
    if let Some(categories) = data.find(cal::Categories) {
        let keywords: Map<String, Value> = categories
            .find_all(cal::Category)
            .filter_map(|c| c.text().filter(|t| !t.is_empty()))
            .map(|name| (name.to_owned(), Value::Bool(true)))
            .collect();
        if !keywords.is_empty() {
            event.insert("keywords".to_owned(), Value::Object(keywords));
        }
    }
    event
}

/// An EAS `<Recurrence>` as a JSCalendar recurrence rule.
fn recurrence_from_eas(recurrence: &Element, tz: Option<Tz>) -> Option<Value> {
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
            rule.insert(
                "byDay".to_owned(),
                days(number(cal::DayOfWeek)?, number(cal::WeekOfMonth)).into(),
            );
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
            rule.insert(
                "byDay".to_owned(),
                days(number(cal::DayOfWeek)?, number(cal::WeekOfMonth)).into(),
            );
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
) -> Option<(String, Value)> {
    let timing = Timing::of(event);
    let mut exception = Element::new(cal::Exception)
        .child(Element::with_text(cal::ExceptionStartTime, instance_id));
    match data {
        None => exception.push(Element::with_text(cal::Deleted, "1")),
        Some(data) => {
            for tag in [cal::Subject, cal::Location, cal::StartTime, cal::EndTime] {
                if let Some(value) = data.child_text(tag) {
                    exception.push(Element::with_text(tag, value));
                }
            }
        }
    }
    let exceptions = Element::new(cal::Exceptions).child(exception);
    overrides_from_eas(&exceptions, timing.tz, timing.all_day)
        .into_iter()
        .next()
}

/// EAS `<Exceptions>` as JSCalendar `recurrenceOverrides`.
fn overrides_from_eas(exceptions: &Element, tz: Option<Tz>, all_day: bool) -> Map<String, Value> {
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
        .filter(|(email, _, _)| !email.is_empty() && !email.eq_ignore_ascii_case(own_email))
        .collect();
    (!attendees.is_empty()).then_some(attendees)
}

/// JSCalendar 2.0 participants for `attendees`, with the user as organizer.
/// Existing participants keep their entry (and response) when still invited.
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
    let (owner_key, owner) = match find(own_email) {
        Some((key, p)) => ((*key).clone(), (*p).clone()),
        None => (
            "owner".to_owned(),
            json!({ "@type": "Participant", "calendarAddress": format!("mailto:{own_email}"),
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
        assert_eq!(format_duration(Duration::minutes(90)), "PT1H30M");
        assert_eq!(format_duration(Duration::days(2)), "P2D");
        assert_eq!(format_duration(Duration::hours(25)), "P1DT1H");
        assert_eq!(format_duration(Duration::zero()), "PT0S");
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
                tz::eas_timezone(Some(chrono_tz::America::New_York), 2026),
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
                let tz: Tz = zone.parse().unwrap();
                let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                    + Duration::minutes(15 * start_minutes);
                let end = start + Duration::minutes(15 * length_quarters);
                let mut data = Element::new(airsync::ApplicationData)
                    .child(Element::with_text(cal::TimeZone, tz::eas_timezone(Some(tz), 2026)))
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
}
