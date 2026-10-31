//! Calendar collections: JMAP calendars as EAS calendar folders.
//!
//! Works like contacts: a per-collection view caught up with
//! `CalendarEvent/changes`, and a version per event that hashes what the
//! device receives. The sync window (FilterType) limits which events are
//! listed; recurring events are included when any occurrence falls in it.
//! Events created, changed or deleted on the device are written back; for
//! meetings the user organizes, the server sends the invitations.

use std::collections::HashSet;

use super::tz;
use chrono::Utc;

use serde_json::{Map, Value, json};

use super::{
    Request,
    contacts::fnv1a,
    device_changes::{Kind, Patch},
    email::BodyPreference,
    jscalendar,
    view::{self, Item, Source},
};
use crate::{
    jmap::{Changes, JmapClient, SetResult},
    wbxml::{
        self, Element,
        cp::{airsync, airsyncbase, calendar as cal},
    },
};

/// Folder ids of calendars carry this prefix in the store.
pub const PREFIX: &str = "calendar:";

/// An event's version: the high half hashes the event itself, the low half
/// its attendees and the user's response. Clients built on AOSP Exchange
/// (Gmail) ignore attendees in a normal Change, so a change in the low half
/// is sent as a separate attendees-only Change (see [`attendees_only`]).
pub(super) fn version_of(data: &Element) -> u64 {
    let (event, attendees) = split_attendees(data);
    (fnv1a(&wbxml::encode(&event)) & 0xFFFF_FFFF_0000_0000)
        | (fnv1a(&wbxml::encode(&attendees)) & 0xFFFF_FFFF)
}

pub(super) const EVENT_HALF: u64 = 0xFFFF_FFFF_0000_0000;
pub(super) const ATTENDEE_HALF: u64 = 0xFFFF_FFFF;

/// The event without, and the attendees with, the attendee information.
/// `DtStamp` is in neither: the server's value does not change when only
/// responses change, so changes are stamped when they are sent instead.
fn split_attendees(data: &Element) -> (Element, Element) {
    let is_attendee_part = |tag| tag == cal::Attendees || tag == cal::ResponseType;
    let mut event = Element::new(data.tag);
    let mut attendees = Element::new(data.tag);
    for child in &data.children {
        match child {
            wbxml::Node::Element(el) if el.tag == cal::DtStamp => {}
            wbxml::Node::Element(el) if is_attendee_part(el.tag) => {
                attendees.children.push(child.clone())
            }
            _ => event.children.push(child.clone()),
        }
    }
    (event, attendees)
}

/// Sets the `DtStamp` of a Change to now, so the device sees it as newer
/// than what it has.
pub(super) fn stamp_now(data: &mut Element) {
    let now = tz::compact(Utc::now());
    data.children
        .retain(|n| !matches!(n, wbxml::Node::Element(e) if e.tag == cal::DtStamp));
    data.children.insert(
        0,
        wbxml::Node::Element(Element::with_text(cal::DtStamp, now)),
    );
}

/// An `ApplicationData` that starts with `<Attendees>` (after an optional
/// `<DtStamp>`): AOSP-based clients treat such a Change as an attendees-only
/// update and refresh the attendees, including the user's own response from
/// `<ResponseType>`. `None` when the event has no attendees.
pub(super) fn attendees_only(data: &Element) -> Option<Element> {
    let (_, attendees) = split_attendees(data);
    attendees.find(cal::Attendees)?;
    // Attendees first, then the response type.
    let mut ordered = Element::new(data.tag);
    ordered.children = attendees.children;
    ordered
        .children
        .sort_by_key(|n| !matches!(n, wbxml::Node::Element(e) if e.tag == cal::Attendees));
    stamp_now(&mut ordered);
    Some(ordered)
}

/// A calendar's events in the sync window, versioned by a hash of what the
/// device sees.
pub struct CalendarSource<'a> {
    pub jmap: &'a JmapClient,
    pub calendar: &'a str,
    /// The sync window in days (see [`view::window_days`]), `None` for all.
    pub days: Option<u32>,
    pub body: &'a BodyPreference,
    /// The user's addresses, comma separated (see [`jscalendar::is_own`]).
    pub own: &'a str,
}

impl CalendarSource<'_> {
    async fn ids_in_window(&self) -> anyhow::Result<Vec<String>> {
        self.jmap
            .calendar_event_ids(self.calendar, view::window_start(self.days).as_deref())
            .await
    }

    fn items(&self, events: &[Value]) -> Vec<Item> {
        events
            .iter()
            .filter(|event| event["calendarIds"][self.calendar] == true)
            .filter_map(|event| {
                let data = jscalendar::to_eas(event, self.body, self.own);
                Some(Item {
                    id: event["id"].as_str()?.to_owned(),
                    version: version_of(&data),
                    order: None,
                })
            })
            .collect()
    }
}

impl Source for CalendarSource<'_> {
    fn key(&self) -> String {
        format!("calendar:{}:{}d", self.calendar, self.days.unwrap_or(0))
    }

    async fn state(&self) -> anyhow::Result<String> {
        self.jmap.state(crate::jmap::CALENDAR_EVENT).await
    }

    async fn changes(&self, since: &str, max: usize) -> anyhow::Result<Changes> {
        self.jmap
            .changes(crate::jmap::CALENDAR_EVENT, since, max)
            .await
    }

    async fn list(&self) -> anyhow::Result<Vec<Item>> {
        let ids = self.ids_in_window().await?;
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        Ok(self.items(
            &self
                .jmap
                .get_objects(crate::jmap::CALENDAR_EVENT, &id_refs)
                .await?,
        ))
    }

    async fn refresh(&self, ids: &[&str]) -> anyhow::Result<Vec<Item>> {
        // Recurring events make "in the window" a question for the server.
        let in_window: Option<HashSet<String>> = match self.days {
            None => None,
            Some(_) => Some(self.ids_in_window().await?.into_iter().collect()),
        };
        let wanted: Vec<&str> = ids
            .iter()
            .copied()
            .filter(|id| in_window.as_ref().is_none_or(|w| w.contains(*id)))
            .collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.items(
            &self
                .jmap
                .get_objects(crate::jmap::CALENDAR_EVENT, &wanted)
                .await?,
        ))
    }
}

/// Events with their version and `ApplicationData`; missing ids are skipped.
pub async fn fetch(
    jmap: &JmapClient,
    ids: &[&str],
    body: &BodyPreference,
    own_email: &str,
) -> anyhow::Result<Vec<(String, u64, Element)>> {
    Ok(jmap
        .get_objects(crate::jmap::CALENDAR_EVENT, ids)
        .await?
        .iter()
        .filter_map(|event| {
            let data = jscalendar::to_eas(event, body, own_email);
            Some((event["id"].as_str()?.to_owned(), version_of(&data), data))
        })
        .collect())
}

/// The occurrence an EAS 16 Change or Delete is about, if any.
fn instance_id(command: &Element) -> Option<&str> {
    command.child_text(airsyncbase::InstanceId).or_else(|| {
        command
            .find(airsync::ApplicationData)
            .and_then(|d| d.child_text(airsyncbase::InstanceId))
    })
}

/// How the device's changes to events become JMAP (see [`device_changes`]).
pub(super) struct CalendarChanges<'a> {
    pub req: &'a Request<'a>,
    pub jmap: &'a JmapClient,
    pub calendar: &'a str,
    pub body: &'a BodyPreference,
    /// The events being changed: meetings keep their participants'
    /// responses, and occurrences are found in their series.
    current: Vec<Value>,
    /// Whether invitations or updates must go out.
    scheduling: bool,
}

impl<'a> CalendarChanges<'a> {
    pub fn new(
        req: &'a Request<'a>,
        jmap: &'a JmapClient,
        calendar: &'a str,
        body: &'a BodyPreference,
    ) -> Self {
        Self {
            req,
            jmap,
            calendar,
            body,
            current: Vec::new(),
            scheduling: false,
        }
    }

    fn current(&self, id: &str) -> Option<&Value> {
        self.current.iter().find(|e| e["id"] == id)
    }

    /// Participants for a meeting the user organizes on the device.
    fn organize(&mut self, event: &mut Map<String, Value>, data: &Element, existing: &Value) {
        let (email, own) = (self.req.email, self.req.own);
        if let Some(attendees) = jscalendar::attendees_from_eas(data, own) {
            event.insert(
                "participants".to_owned(),
                jscalendar::participants(email, &attendees, existing),
            );
            event.insert(
                "organizerCalendarAddress".to_owned(),
                format!("mailto:{email}").into(),
            );
            self.scheduling = true;
        }
    }
}

impl Kind for CalendarChanges<'_> {
    async fn prepare(&mut self, ids: &[&str]) -> anyhow::Result<()> {
        if !ids.is_empty() {
            self.current = self
                .jmap
                .get_objects(crate::jmap::CALENDAR_EVENT, ids)
                .await?;
        }
        Ok(())
    }

    fn create(&mut self, data: &Element, client_id: &str) -> Map<String, Value> {
        let mut event: Map<String, Value> = jscalendar::from_eas(data)
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .collect();
        event.insert("@type".to_owned(), "Event".into());
        // Cyrus requires it; Stalwart would fill it in.
        event.insert("version".to_owned(), "2.0".into());
        let uid = data
            .child_text(cal::UID)
            .or_else(|| data.child_text(cal::ClientUid))
            .filter(|u| !u.is_empty())
            .unwrap_or(client_id);
        event.insert("uid".to_owned(), uid.into());
        event.insert("calendarIds".to_owned(), json!({ self.calendar: true }));
        self.organize(&mut event, data, &Value::Null);
        event
    }

    // EAS 16 deletes one occurrence of a series with an InstanceId.
    fn is_patch(&self, command: &Element) -> bool {
        instance_id(command).is_some()
    }

    fn patch(&mut self, id: &str, command: &Element) -> Patch {
        let data = command.find(airsync::ApplicationData);
        // EAS 16: a change to one occurrence of a series.
        if let Some(instance) = instance_id(command) {
            let data = data.filter(|_| command.tag == airsync::Change);
            let Some(event) = self.current(id) else {
                return Patch::NotFound;
            };
            let has_participants = event["participants"].is_object();
            let Some((recurrence_id, entry)) =
                jscalendar::occurrence_override(event, instance, data)
            else {
                return Patch::NotFound;
            };
            self.scheduling |= has_participants;
            let mut patch = Map::new();
            patch.insert(format!("recurrenceOverrides/{recurrence_id}"), entry);
            return Patch::Apply(patch);
        }
        let Some(data) = data else {
            return Patch::Skip;
        };
        let mut patch = jscalendar::from_eas(data);
        let existing = self
            .current(id)
            .map_or(Value::Null, |e| e["participants"].clone());
        self.organize(&mut patch, data, &existing);
        Patch::Apply(patch)
    }

    async fn writable(&self) -> anyhow::Result<bool> {
        if !self.jmap.is_shared() {
            return Ok(true);
        }
        self.jmap.calendar_writable(self.calendar).await
    }

    async fn set(
        &self,
        create: Map<String, Value>,
        update: Map<String, Value>,
        destroy: Vec<String>,
    ) -> anyhow::Result<SetResult> {
        self.jmap
            .set(
                crate::jmap::CALENDAR_EVENT,
                crate::jmap::SetRequest::default()
                    .create(create)
                    .update(update)
                    .destroy(destroy)
                    .with("sendSchedulingMessages", self.scheduling),
            )
            .await
    }

    async fn fetch(&self, ids: &[&str]) -> anyhow::Result<Vec<(String, u64, Element)>> {
        fetch(self.jmap, ids, self.body, self.req.own).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn meeting(status: &str, title: &str) -> Element {
        let event = json!({
            "title": title, "start": "2026-11-13T10:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT30M",
            "organizerCalendarAddress": "mailto:ann@example.com",
            "participants": {
                "o": { "calendarAddress": "mailto:ann@example.com", "roles": { "owner": true } },
                "a": { "calendarAddress": "mailto:rob@example.com", "participationStatus": status },
            },
        });
        let body = BodyPreference {
            body_type: 1,
            truncation_size: None,
            all_or_none: false,
            preview: None,
        };
        jscalendar::to_eas(&event, &body, "rob@example.com")
    }

    #[test]
    fn versions_separate_responses_from_event_changes() {
        let invited = version_of(&meeting("needs-action", "Review"));
        let accepted = version_of(&meeting("accepted", "Review"));
        let renamed = version_of(&meeting("needs-action", "Review v2"));
        assert_eq!(invited & EVENT_HALF, accepted & EVENT_HALF);
        assert_ne!(invited & ATTENDEE_HALF, accepted & ATTENDEE_HALF);
        assert_ne!(invited & EVENT_HALF, renamed & EVENT_HALF);
        assert_eq!(invited & ATTENDEE_HALF, renamed & ATTENDEE_HALF);
    }

    #[test]
    fn attendees_only_update_starts_with_attendees() {
        let update = attendees_only(&meeting("accepted", "Review")).unwrap();
        let tags: Vec<_> = update.elements().map(|e| e.tag).collect();
        assert_eq!(tags, vec![cal::DtStamp, cal::Attendees, cal::ResponseType]);
        assert_eq!(update.child_text(cal::ResponseType), Some("3"));
    }
}
