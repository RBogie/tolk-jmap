//! MeetingResponse (MS-ASCMD 2.2.1.11): accepting, tentatively accepting or
//! declining a meeting, from its calendar item or from the invitation email.
//!
//! Only the user's own participation status on the calendar event changes,
//! for the whole series or, with an InstanceId, for that occurrence (in its
//! `recurrenceOverrides` entry). The email is only read: the invitation's
//! UID finds the event, which is created in the default calendar if the
//! server has not added it yet.
//!
//! The reply to the organizer: up to EAS 14.1 the device sends its own (with
//! SendMail); with 16.x the server sends one when the request has
//! `<SendResponse>` (MS-ASCMD 2.2.1.11), here the JMAP server, through
//! `sendSchedulingMessages`. Its text is the server's: a body or a proposed
//! new time in `<SendResponse>` is not passed on.

use serde_json::{Map, Value, json};

use super::{
    CommandResult, Request, calendar, email, jscalendar,
    sync::{self, FolderKind},
};
use crate::{
    jmap::{BodyValues, JmapClient},
    wbxml::{Element, cp::meeting_response as mr},
};

mod status {
    pub const OK: &str = "1";
    pub const INVALID_REQUEST: &str = "2";
    pub const SERVER_ERROR: &str = "4";
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(mr::MeetingResponse)?;
    let mut out = Element::new(mr::MeetingResponse);
    for request in body.find_all(mr::Request) {
        out.push(respond(req, jmap, request).await?);
    }
    Ok(out)
}

/// JSCalendar participation status for an EAS UserResponse.
fn participation(user_response: &str) -> Option<&'static str> {
    match user_response.trim() {
        "1" => Some("accepted"),
        "2" => Some("tentative"),
        "3" => Some("declined"),
        _ => None,
    }
}

async fn respond(req: &Request<'_>, jmap: &JmapClient, request: &Element) -> CommandResult {
    let request_id = request.child_text(mr::RequestId).unwrap_or_default();
    let reply = |status: &str| {
        Element::new(mr::Result)
            .child(Element::with_text(mr::RequestId, request_id))
            .child(Element::with_text(mr::Status, status))
    };
    let Some(answer) = request.child_text(mr::UserResponse).and_then(participation) else {
        return Ok(reply(status::INVALID_REQUEST));
    };
    let Some(folder) = sync::resolve(
        req,
        request.child_text(mr::CollectionId).unwrap_or_default(),
    )?
    else {
        return Ok(reply(status::INVALID_REQUEST));
    };
    let send = req.version >= super::v16::VERSION && request.find(mr::SendResponse).is_some();
    if send
        && request
            .find(mr::SendResponse)
            .is_some_and(|r| r.elements().next().is_some())
    {
        tracing::info!("the reply's own text or proposed time is not passed on");
    }
    // An event in a shared calendar is answered in its account.
    let jmap = &*folder.client(jmap);

    // Find (or place) the calendar event the response is about.
    let event_id = match folder.kind {
        FolderKind::Calendar => folder.email_for(req, request_id)?,
        FolderKind::Mail => {
            let Some(email_id) = folder.email_for(req, request_id)? else {
                return Ok(reply(status::INVALID_REQUEST));
            };
            let emails = jmap
                .get_emails(&[email_id.as_str()], BodyValues::None, 0)
                .await?;
            let invite = match emails.first() {
                Some(msg) => email::invitation(msg, jmap).await,
                None => None,
            };
            let Some(invite) = invite else {
                return Ok(reply(status::INVALID_REQUEST));
            };
            let uid = invite["uid"].as_str().unwrap_or_default().to_owned();
            match jmap
                .calendar_event_ids_by_uid(&uid)
                .await?
                .into_iter()
                .next()
            {
                Some(id) => Some(id),
                None => match place_invitation(jmap, invite, req.own, answer, send).await? {
                    Some(id) => {
                        tracing::info!(uid = %uid, answer, "invitation added to the calendar");
                        return finish(req, jmap, reply(status::OK), &id, answer).await;
                    }
                    None => return Ok(reply(status::SERVER_ERROR)),
                },
            }
        }
        FolderKind::Contacts => None,
    };
    let Some(event_id) = event_id else {
        return Ok(reply(status::INVALID_REQUEST));
    };

    let events = jmap
        .get_objects(crate::jmap::CALENDAR_EVENT, &[event_id.as_str()])
        .await?;
    let Some(event) = events.first() else {
        return Ok(reply(status::INVALID_REQUEST));
    };
    let Some(key) = jscalendar::own_participant_key(event, req.own) else {
        tracing::warn!(event = event_id, "not an invitee of this event");
        return Ok(reply(status::INVALID_REQUEST));
    };
    let status_path = format!("participants/{key}/participationStatus");
    let patch = match request.child_text(mr::InstanceId) {
        // One occurrence: its override (a patch of the series) gets it.
        Some(instance) => {
            let Some(occurrence) = jscalendar::occurrence_key(event, instance) else {
                return Ok(reply(status::INVALID_REQUEST));
            };
            let overrides = &event["recurrenceOverrides"];
            let mut entry = overrides[&occurrence]
                .as_object()
                .cloned()
                .unwrap_or_default();
            match entry.get_mut("participants") {
                Some(participants) if participants[key].is_object() => {
                    participants[key]["participationStatus"] = answer.into();
                }
                _ => {
                    entry.insert(status_path, answer.into());
                }
            }
            // A patch path needs an existing object.
            if overrides.is_object() {
                json!({ format!("recurrenceOverrides/{occurrence}"): entry })
            } else {
                json!({ "recurrenceOverrides": { occurrence: entry } })
            }
        }
        None => json!({ status_path: answer }),
    };
    let mut update = Map::new();
    update.insert(event_id.clone(), patch);
    let result = jmap
        .set(
            crate::jmap::CALENDAR_EVENT,
            crate::jmap::SetRequest::default()
                .update(update)
                .with("sendSchedulingMessages", send),
        )
        .await?;
    if let Some(error) = result.error(&event_id) {
        tracing::warn!(event = event_id, error, "meeting response rejected");
        return Ok(reply(status::SERVER_ERROR));
    }
    tracing::info!(
        event = event_id,
        answer,
        reply_sent = send,
        "meeting answered"
    );
    finish(req, jmap, reply(status::OK), &event_id, answer).await
}

/// Adds the CalendarId (the event's ServerId in its calendar folder) unless
/// the meeting was declined.
async fn finish(
    req: &Request<'_>,
    jmap: &JmapClient,
    mut reply: Element,
    event_id: &str,
    answer: &str,
) -> CommandResult {
    if answer == "declined" {
        return Ok(reply);
    }
    let events = jmap
        .get_objects(crate::jmap::CALENDAR_EVENT, &[event_id])
        .await?;
    let calendar_id = events.first().and_then(|e| {
        e["calendarIds"]
            .as_object()?
            .iter()
            .find(|(_, v)| **v == true)
            .map(|(k, _)| k.clone())
    });
    if let Some(calendar_id) = calendar_id {
        let key = sync::stored_id(calendar::PREFIX, jmap.shared_account(), &calendar_id);
        let folder_id = req.store.folder_server_ids(req.user, &[key.as_str()])?[&key];
        let item = req.store.item_ids(req.user, folder_id, &[event_id])?[event_id];
        reply.push(Element::with_text(
            mr::CalendarId,
            format!("{folder_id}:{item}"),
        ));
    }
    Ok(reply)
}

/// Creates the invited event in the default calendar with the user's
/// answer, for invitations the server has not put in a calendar itself.
async fn place_invitation(
    jmap: &JmapClient,
    mut event: Value,
    own_email: &str,
    answer: &str,
    send: bool,
) -> anyhow::Result<Option<String>> {
    let calendars = jmap.calendars().await?;
    let Some(calendar) = calendars
        .iter()
        .find(|c| c.is_default)
        .or(calendars.first())
    else {
        return Ok(None);
    };
    let Some(object) = event.as_object_mut() else {
        return Ok(None);
    };
    // Server-assigned or iTIP-only properties of a parsed object.
    object.retain(|k, v| {
        !v.is_null() && !matches!(k.as_str(), "id" | "method" | "iCalendar" | "baseEventId")
    });
    object.insert(
        "calendarIds".to_owned(),
        json!({ (calendar.id.clone()): true }),
    );
    if let Some(key) = jscalendar::own_participant_key(&event, own_email).map(str::to_owned) {
        event["participants"][&key]["participationStatus"] = answer.into();
    }
    let mut create = Map::new();
    create.insert("i".to_owned(), event);
    let result = jmap
        .set(
            crate::jmap::CALENDAR_EVENT,
            crate::jmap::SetRequest::default()
                .create(create)
                .with("sendSchedulingMessages", send),
        )
        .await?;
    Ok(result
        .created
        .get("i")
        .and_then(|c| c["id"].as_str())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_user_responses() {
        assert_eq!(participation("1"), Some("accepted"));
        assert_eq!(participation("2"), Some("tentative"));
        assert_eq!(participation("3"), Some("declined"));
        assert_eq!(participation("4"), None);
    }
}
