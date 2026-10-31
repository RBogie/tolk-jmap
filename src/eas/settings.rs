//! Settings (MS-ASCMD 2.2.1.18): device information, user information and
//! out-of-office. We store device information, report the account address,
//! and map out-of-office to the JMAP vacation response (one message for
//! every audience; JMAP has no internal/external distinction).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{CommandResult, Request};
use crate::{
    jmap::{self, JmapClient},
    wbxml::{Element, cp::settings},
};

const OK: &str = "1";
const PROTOCOL_ERROR: &str = "2";
const SERVER_ERROR: &str = "6";

/// EAS OofState values.
mod oof_state {
    pub const DISABLED: &str = "0";
    pub const GLOBAL: &str = "1";
    pub const TIME_BASED: &str = "2";
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(settings::Settings)?;
    let mut response =
        Element::new(settings::Settings).child(Element::with_text(settings::Status, OK));
    for part in body.elements() {
        let reply = match part.tag {
            settings::DeviceInformation => {
                store_device_info(req, part)?;
                status(part.tag, OK)
            }
            settings::UserInformation if part.find(settings::Get).is_some() => {
                user_information(req.email, req.own)
            }
            settings::Oof => match oof(part, jmap).await {
                Ok(reply) => reply,
                Err(e) if jmap::is_unauthorized(&e) => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "out-of-office failed");
                    status(settings::Oof, SERVER_ERROR)
                }
            },
            // Accepted and ignored: we set no password policy.
            settings::DevicePassword => status(part.tag, OK),
            other => status(other, PROTOCOL_ERROR),
        };
        response.push(reply);
    }
    Ok(response)
}

/// Out-of-office get or set, through the vacation response.
async fn oof(part: &Element, jmap: &JmapClient) -> anyhow::Result<Element> {
    if let Some(get) = part.find(settings::Get) {
        let html = get
            .child_text(settings::BodyType)
            .is_some_and(|t| t.eq_ignore_ascii_case("html"));
        let vacation = jmap.vacation_response().await?.unwrap_or(Value::Null);
        return Ok(status(settings::Oof, OK).child(oof_get(&vacation, html)));
    }
    let Some(set) = part.find(settings::Set) else {
        return Ok(status(settings::Oof, PROTOCOL_ERROR));
    };
    let Some(patch) = oof_patch(set) else {
        return Ok(status(settings::Oof, PROTOCOL_ERROR));
    };
    Ok(match jmap.set_vacation_response(patch).await? {
        true => status(settings::Oof, OK),
        false => status(settings::Oof, SERVER_ERROR),
    })
}

/// The `<Get>` reply for a JMAP VacationResponse.
fn oof_get(vacation: &Value, html: bool) -> Element {
    let state = match (
        vacation["isEnabled"].as_bool().unwrap_or(false),
        vacation["fromDate"].is_string() || vacation["toDate"].is_string(),
    ) {
        (false, _) => oof_state::DISABLED,
        (true, false) => oof_state::GLOBAL,
        (true, true) => oof_state::TIME_BASED,
    };
    let mut get = Element::new(settings::Get).child(Element::with_text(settings::OofState, state));
    if state == oof_state::TIME_BASED {
        for (tag, key) in [
            (settings::StartTime, "fromDate"),
            (settings::EndTime, "toDate"),
        ] {
            if let Some(date) = vacation[key].as_str() {
                get.push(Element::with_text(tag, eas_time(date)));
            }
        }
    }
    let (body, body_type) = match (
        html,
        vacation["htmlBody"].as_str(),
        vacation["textBody"].as_str(),
    ) {
        (true, Some(html), _) => (html.to_owned(), "HTML"),
        (_, _, Some(text)) => (text.to_owned(), "Text"),
        (_, Some(html), None) => (super::email::html_to_text(html), "Text"),
        _ => (String::new(), "Text"),
    };
    let enabled = if body.is_empty() { "0" } else { "1" };
    for audience in [
        settings::AppliesToInternal,
        settings::AppliesToExternalKnown,
        settings::AppliesToExternalUnknown,
    ] {
        let mut message = Element::new(settings::OofMessage)
            .child(Element::new(audience))
            .child(Element::with_text(settings::Enabled, enabled));
        if !body.is_empty() {
            message.push(Element::with_text(settings::ReplyMessage, body.as_str()));
            message.push(Element::with_text(settings::BodyType, body_type));
        }
        get.push(message);
    }
    get
}

/// The VacationResponse patch for an Oof `<Set>`; `None` when malformed.
/// The first enabled message (internal first) becomes the reply.
fn oof_patch(set: &Element) -> Option<Map<String, Value>> {
    let state = set.child_text(settings::OofState)?;
    let mut patch = Map::new();
    patch.insert(
        "isEnabled".to_owned(),
        Value::Bool(state != oof_state::DISABLED),
    );
    let date = |tag| set.child_text(tag).map(|d| Value::String(jmap_time(d)));
    let (from, to) = if state == oof_state::TIME_BASED {
        (date(settings::StartTime)?, date(settings::EndTime)?)
    } else {
        (Value::Null, Value::Null)
    };
    patch.insert("fromDate".to_owned(), from);
    patch.insert("toDate".to_owned(), to);
    let message = [
        settings::AppliesToInternal,
        settings::AppliesToExternalKnown,
        settings::AppliesToExternalUnknown,
    ]
    .iter()
    .find_map(|audience| {
        set.find_all(settings::OofMessage)
            .find(|m| m.find(*audience).is_some() && m.child_text(settings::Enabled) == Some("1"))
    });
    if let Some(message) = message
        && let Some(reply) = message.child_text(settings::ReplyMessage)
    {
        let html = message
            .child_text(settings::BodyType)
            .is_some_and(|t| t.eq_ignore_ascii_case("html"));
        let (text, html_body) = if html {
            (
                super::email::html_to_text(reply),
                Value::String(reply.to_owned()),
            )
        } else {
            (reply.to_owned(), Value::Null)
        };
        patch.insert("textBody".to_owned(), Value::String(text));
        patch.insert("htmlBody".to_owned(), html_body);
    }
    Some(patch)
}

/// `2026-10-30T10:00:00Z` (JMAP UTCDate) as EAS `2026-10-30T10:00:00.000Z`.
fn eas_time(date: &str) -> String {
    match date.strip_suffix('Z') {
        Some(base) if !base.contains('.') => format!("{base}.000Z"),
        _ => date.to_owned(),
    }
}

/// An EAS time as a JMAP UTCDate (no fractional seconds).
fn jmap_time(date: &str) -> String {
    let base = date.trim().trim_end_matches('Z');
    let base = base.split('.').next().unwrap_or(base);
    format!("{base}Z")
}

/// Saves the `<DeviceInformation><Set>` fields, also sent inside Provision.
pub fn store_device_info(req: &Request, info: &Element) -> anyhow::Result<()> {
    let Some(set) = info.find(settings::Set) else {
        return Ok(());
    };
    let fields: BTreeMap<String, String> = set
        .elements()
        .filter_map(|el| Some((el.tag.name()?.to_owned(), el.text()?.to_owned())))
        .collect();
    req.store.set_device_info(req.user, req.device_id, &fields)
}

/// The account's addresses: the primary one and every alias.
fn user_information(email: &str, own: &str) -> Element {
    let mut addresses = Element::new(settings::EmailAddresses)
        .child(Element::with_text(settings::PrimarySmtpAddress, email));
    for alias in own.split(',').map(str::trim).filter(|a| !a.is_empty()) {
        addresses.push(Element::with_text(settings::SmtpAddress, alias));
    }
    status(settings::UserInformation, OK).child(Element::new(settings::Get).child(
        Element::new(settings::Accounts).child(Element::new(settings::Account).child(addresses)),
    ))
}

fn status(tag: crate::wbxml::Tag, value: &str) -> Element {
    Element::new(tag).child(Element::with_text(settings::Status, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn answers_each_part() {
        let store = Store::open_in_memory().unwrap();
        store.touch_device("rob", "d1", None).unwrap();
        let body = Element::new(settings::Settings)
            .child(Element::new(settings::DeviceInformation).child(
                Element::new(settings::Set).child(Element::with_text(settings::Model, "Pixel")),
            ))
            .child(Element::new(settings::UserInformation).child(Element::new(settings::Get)))
            .child(Element::new(settings::RightsManagementInformation));
        let response = handle_offline(&store, body);

        let parts: Vec<_> = response
            .elements()
            .map(|e| (e.tag, e.child_text(settings::Status)))
            .collect();
        assert_eq!(
            parts,
            vec![
                (settings::Status, None),
                (settings::DeviceInformation, Some("1")),
                (settings::UserInformation, Some("1")),
                (settings::RightsManagementInformation, Some("2")),
            ]
        );
        let address = response
            .find(settings::UserInformation)
            .and_then(|u| u.find(settings::Get))
            .and_then(|g| g.find(settings::Accounts))
            .and_then(|a| a.find(settings::Account))
            .and_then(|a| a.find(settings::EmailAddresses))
            .and_then(|e| e.child_text(settings::PrimarySmtpAddress));
        assert_eq!(address, Some("rob@example.com"));
    }

    /// Runs `handle` for parts that need no JMAP server.
    fn handle_offline(store: &Store, body: Element) -> Element {
        let jmap = crate::jmap::JmapClient::for_tests();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(handle(&Request::for_tests(store, Some(body)), &jmap))
            .unwrap()
    }

    #[test]
    fn maps_oof_both_ways() {
        let set = Element::new(settings::Set)
            .child(Element::with_text(settings::OofState, "2"))
            .child(Element::with_text(
                settings::StartTime,
                "2026-11-01T00:00:00.000Z",
            ))
            .child(Element::with_text(
                settings::EndTime,
                "2026-11-08T00:00:00.000Z",
            ))
            .child(
                Element::new(settings::OofMessage)
                    .child(Element::new(settings::AppliesToExternalUnknown))
                    .child(Element::with_text(settings::Enabled, "1"))
                    .child(Element::with_text(
                        settings::ReplyMessage,
                        "Away until the 8th",
                    ))
                    .child(Element::with_text(settings::BodyType, "Text")),
            );
        let patch = Value::Object(oof_patch(&set).unwrap());
        assert_eq!(patch["isEnabled"], true);
        assert_eq!(patch["fromDate"], "2026-11-01T00:00:00Z");
        assert_eq!(patch["textBody"], "Away until the 8th");

        let get = oof_get(&patch, false);
        assert_eq!(get.child_text(settings::OofState), Some("2"));
        assert_eq!(
            get.child_text(settings::EndTime),
            Some("2026-11-08T00:00:00.000Z")
        );
        let messages: Vec<_> = get.find_all(settings::OofMessage).collect();
        assert_eq!(messages.len(), 3);
        assert!(
            messages
                .iter()
                .all(|m| m.child_text(settings::ReplyMessage) == Some("Away until the 8th"))
        );

        let off = Element::new(settings::Set).child(Element::with_text(settings::OofState, "0"));
        let patch = Value::Object(oof_patch(&off).unwrap());
        assert_eq!(patch["isEnabled"], false);
        assert_eq!(
            oof_get(&patch, false).child_text(settings::OofState),
            Some("0")
        );
    }
}
