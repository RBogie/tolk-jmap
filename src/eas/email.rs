//! JMAP `Email` to EAS email `ApplicationData` (MS-ASEMAIL, MS-ASAIRS).

use serde::{Deserialize, Serialize};

use super::jscalendar;

use crate::{
    jmap::{BodyPart, BodyValues, Email, EmailAddress, JmapClient},
    wbxml::{
        Element,
        cp::{airsyncbase as base, email, email2},
    },
};

/// AirSyncBase body types we can produce.
pub mod body_type {
    pub const PLAIN: u8 = 1;
    pub const HTML: u8 = 2;
    pub const MIME: u8 = 4;
}

pub const SEEN: &str = "$seen";
pub const FLAGGED: &str = "$flagged";
pub const ANSWERED: &str = "$answered";
pub const FORWARDED: &str = "$forwarded";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyPreference {
    pub body_type: u8,
    pub truncation_size: Option<u32>,
    pub all_or_none: bool,
    /// Characters of plain text preview the device wants (`<Preview>`).
    #[serde(default)]
    pub preview: Option<u32>,
}

impl BodyPreference {
    /// The first requested type we support; plain text when none was asked.
    pub fn choose(preferences: &[BodyPreference]) -> BodyPreference {
        preferences
            .iter()
            .copied()
            .find(|p| {
                matches!(
                    p.body_type,
                    body_type::PLAIN | body_type::HTML | body_type::MIME
                )
            })
            .unwrap_or(BodyPreference {
                body_type: body_type::PLAIN,
                truncation_size: None,
                all_or_none: false,
                preview: None,
            })
    }

    /// What `Email/get` must fetch to build this body.
    pub fn body_values(&self) -> BodyValues {
        match self.body_type {
            body_type::HTML => BodyValues::Html,
            body_type::PLAIN => BodyValues::Text,
            _ => BodyValues::None,
        }
    }
}

pub async fn application_data(
    msg: &Email,
    body: &BodyPreference,
    jmap: &JmapClient,
) -> anyhow::Result<Element> {
    let mut data = Element::new(crate::wbxml::cp::airsync::ApplicationData);
    let mut text = |tag, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            data.push(Element::with_text(tag, value));
        }
    };
    text(email::To, format_addresses(msg.to.as_deref()));
    text(email::Cc, format_addresses(msg.cc.as_deref()));
    text(email::From, format_addresses(msg.from.as_deref()));
    text(email::Subject, msg.subject.clone());
    text(email::ReplyTo, format_addresses(msg.reply_to.as_deref()));
    text(email::DateReceived, Some(eas_date(&msg.received_at)));
    text(email::DisplayTo, display_names(msg.to.as_deref()));
    text(email::ThreadTopic, msg.subject.clone());
    text(email::Importance, Some(importance(msg).to_owned()));
    text(email::Read, Some(flag(msg, SEEN)));

    if !msg.attachments.is_empty() {
        data.push(
            msg.attachments
                .iter()
                .fold(Element::new(base::Attachments), |list, part| {
                    list.child(attachment(part, jmap.shared_account()))
                }),
        );
    }
    data.push(body_element(msg, body, jmap).await?);
    let invite = invitation(msg, jmap).await;
    let message_class = match invite
        .as_ref()
        .map(|e| e["method"].as_str().unwrap_or("request"))
    {
        Some("request") => "IPM.Schedule.Meeting.Request",
        Some("cancel") => "IPM.Schedule.Meeting.Canceled",
        _ => "IPM.Note",
    };
    data.push(Element::with_text(email::MessageClass, message_class));
    if message_class != "IPM.Note"
        && let Some(event) = &invite
    {
        data.push(jscalendar::meeting_request(event));
    }
    data.push(Element::with_text(email::InternetCPID, "65001"));
    data.push(flag_element(msg.keywords.get(FLAGGED) == Some(&true)));
    data.push(Element::with_text(
        email::ContentClass,
        if message_class == "IPM.Note" {
            "urn:content-classes:message"
        } else {
            "urn:content-classes:calendarmessage"
        },
    ));
    data.push(Element::with_text(
        base::NativeBodyType,
        native_body_type(msg).to_string(),
    ));
    if let Some(thread) = &msg.thread_id {
        data.push(Element::with_opaque(
            email2::ConversationId,
            conversation_id(thread),
        ));
        data.push(Element::with_opaque(
            email2::ConversationIndex,
            conversation_index(&msg.received_at),
        ));
    }
    if let Some(verb) = last_verb(crate::eas::view::flags(&msg.keywords)) {
        data.push(verb);
    }
    Ok(data)
}

/// EAS Importance (0 low, 1 normal, 2 high) from the `Importance` or
/// `X-Priority` header.
fn importance(msg: &Email) -> &'static str {
    let header = |h: &Option<String>| h.as_deref().map(|v| v.trim().to_ascii_lowercase());
    match header(&msg.importance).as_deref() {
        Some("high") => return "2",
        Some("low") => return "0",
        _ => {}
    }
    // "1 (Highest)" .. "5 (Lowest)".
    match header(&msg.priority)
        .as_deref()
        .and_then(|p| p.chars().next())
    {
        Some('1' | '2') => "2",
        Some('4' | '5') => "0",
        _ => "1",
    }
}

/// A stable 16-byte ConversationId for a JMAP thread.
fn conversation_id(thread_id: &str) -> Vec<u8> {
    let hash = |seed: u64| {
        thread_id.bytes().fold(seed, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let mut id = hash(0xcbf2_9ce4_8422_2325).to_be_bytes().to_vec();
    id.extend(hash(0x8422_2325_cbf2_9ce4).to_be_bytes());
    id
}

/// The 5-byte ConversationIndex header: the top bytes of the message's
/// FILETIME, so replies sort after what they answer.
fn conversation_index(received_at: &str) -> Vec<u8> {
    const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
    let ticks = chrono::DateTime::parse_from_rfc3339(received_at)
        .map(|t| FILETIME_UNIX_EPOCH + t.timestamp() * 10_000_000)
        .unwrap_or(FILETIME_UNIX_EPOCH);
    ticks.to_be_bytes()[1..6].to_vec()
}

/// `<LastVerbExecuted>` for the answered and forwarded bits of a version:
/// 1 reply to sender, 3 forward.
pub fn last_verb(flags: u64) -> Option<Element> {
    use crate::eas::view::{ANSWERED, FORWARDED};
    let verb = if flags & ANSWERED != 0 {
        "1"
    } else if flags & FORWARDED != 0 {
        "3"
    } else {
        return None;
    };
    Some(Element::with_text(email2::LastVerbExecuted, verb))
}

/// The calendar event an email invites to (its `text/calendar` part, parsed
/// by the server), if any. Read only; failures just mean "no invitation".
pub async fn invitation(msg: &Email, jmap: &JmapClient) -> Option<serde_json::Value> {
    if !jmap.has_calendars() {
        return None;
    }
    let part = msg
        .attachments
        .iter()
        .chain(&msg.text_body)
        .find(|p| p.mime_type.eq_ignore_ascii_case("text/calendar") && p.blob_id.is_some())?;
    match jmap.parse_calendar_blob(part.blob_id.as_deref()?).await {
        Ok(events) => events.into_iter().find(|e| e["uid"].is_string()),
        Err(e) => {
            tracing::debug!(
                email = msg.id,
                error = format!("{e:#}"),
                "could not parse invitation"
            );
            None
        }
    }
}

pub fn flag_element(flagged: bool) -> Element {
    let mut flag = Element::new(email::Flag);
    if flagged {
        flag.push(Element::with_text(email::Status, "2"));
        flag.push(Element::with_text(email::FlagType, "Flag for follow up"));
    }
    flag
}

fn flag(msg: &Email, keyword: &str) -> String {
    if msg.keywords.get(keyword) == Some(&true) {
        "1"
    } else {
        "0"
    }
    .to_owned()
}

fn attachment(part: &BodyPart, account: Option<&str>) -> Element {
    let mut el = Element::new(base::Attachment)
        .child(Element::with_text(
            base::DisplayName,
            part.name.clone().unwrap_or_else(|| "attachment".to_owned()),
        ))
        .child(Element::with_text(
            base::FileReference,
            file_reference(part, account),
        ))
        .child(Element::with_text(base::Method, "1"))
        .child(Element::with_text(
            base::EstimatedDataSize,
            part.size.to_string(),
        ));
    if let Some(cid) = &part.cid {
        el.push(Element::with_text(
            base::ContentId,
            cid.trim_matches(['<', '>']),
        ));
    }
    if part.disposition.as_deref() == Some("inline") && part.cid.is_some() {
        el.push(Element::with_text(base::IsInline, "1"));
    }
    el
}

/// `blobId|mime/type`, plus `|account` for a shared account's message:
/// everything ItemOperations needs to serve the attachment.
pub(super) fn file_reference(part: &BodyPart, account: Option<&str>) -> String {
    let blob = part.blob_id.as_deref().unwrap_or_default();
    match account {
        Some(account) => format!("{blob}|{}|{account}", part.mime_type),
        None => format!("{blob}|{}", part.mime_type),
    }
}

/// Splits a FileReference into blob id, content type and the shared account
/// it is in. A reference without a type gets a generic one.
pub fn parse_file_reference(reference: &str) -> (&str, &str, Option<&str>) {
    let mut parts = reference.splitn(3, '|');
    let blob = parts.next().unwrap_or_default();
    let content_type = parts.next().unwrap_or("application/octet-stream");
    (blob, content_type, parts.next())
}

async fn body_element(
    msg: &Email,
    pref: &BodyPreference,
    jmap: &JmapClient,
) -> anyhow::Result<Element> {
    let (data, estimated, mut truncated) = match pref.body_type {
        body_type::MIME => {
            let raw = jmap
                .download(&msg.blob_id, "message.eml", "message/rfc822")
                .await?;
            // Data is a WBXML string; 8-bit raw messages lose invalid bytes.
            (String::from_utf8_lossy(&raw).into_owned(), msg.size, false)
        }
        kind => {
            let want_html = kind == body_type::HTML;
            let parts = if want_html {
                &msg.html_body
            } else {
                &msg.text_body
            };
            collect_text(msg, parts, want_html)
        }
    };
    let mut data = data;
    if let Some(limit) = pref.truncation_size.map(|l| l as usize)
        && data.len() > limit
    {
        data.truncate(floor_char_boundary(&data, limit));
        truncated = true;
    }

    let mut el = Element::new(base::Body)
        .child(Element::with_text(base::Type, pref.body_type.to_string()))
        .child(Element::with_text(
            base::EstimatedDataSize,
            estimated.to_string(),
        ));
    if truncated {
        el.push(Element::with_text(base::Truncated, "1"));
    }
    if !(truncated && pref.all_or_none) {
        el.push(Element::with_text(base::Data, data));
    }
    if let Some(chars) = pref.preview.filter(|n| *n > 0)
        && let Some(preview) = msg.preview.as_deref().filter(|p| !p.is_empty())
    {
        let preview: String = preview.chars().take(chars.min(255) as usize).collect();
        el.push(Element::with_text(base::Preview, preview));
    }
    Ok(el)
}

/// Joins the text parts' values, converting between plain and HTML as needed.
/// Returns the data, the estimated full size and whether anything was cut.
fn collect_text(msg: &Email, parts: &[BodyPart], want_html: bool) -> (String, u64, bool) {
    let mut data = String::new();
    let mut estimated = 0;
    let mut truncated = false;
    for part in parts {
        let Some(value) = part.part_id.as_ref().and_then(|id| msg.body_values.get(id)) else {
            continue;
        };
        let is_html = part.mime_type.eq_ignore_ascii_case("text/html");
        match (want_html, is_html) {
            (true, false) => data.push_str(&text_to_html(&value.value)),
            (false, true) => data.push_str(&html_to_text(&value.value)),
            _ => data.push_str(&value.value),
        }
        estimated += part.size;
        truncated |= value.is_truncated;
    }
    let estimated = estimated.max(data.len() as u64);
    (data, estimated, truncated)
}

fn native_body_type(msg: &Email) -> u8 {
    if msg
        .html_body
        .iter()
        .any(|p| p.mime_type.eq_ignore_ascii_case("text/html"))
    {
        body_type::HTML
    } else {
        body_type::PLAIN
    }
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    (0..=index.min(s.len()))
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

/// `"Name" <addr>, addr2` as EAS expects in To, Cc, From and ReplyTo.
pub fn format_addresses(list: Option<&[EmailAddress]>) -> Option<String> {
    let formatted: Vec<String> = list?
        .iter()
        .filter_map(|a| {
            let email = a.email.as_deref()?;
            Some(match a.name.as_deref().filter(|n| !n.is_empty()) {
                Some(name) => format!("\"{}\" <{email}>", name.replace(['\\', '"'], "")),
                None => email.to_owned(),
            })
        })
        .collect();
    Some(formatted.join(", "))
}

pub(super) fn display_names(list: Option<&[EmailAddress]>) -> Option<String> {
    let names: Vec<&str> = list?
        .iter()
        .filter_map(|a| {
            a.name
                .as_deref()
                .filter(|n| !n.is_empty())
                .or(a.email.as_deref())
        })
        .collect();
    Some(names.join("; "))
}

/// JMAP UTCDate (`2026-10-30T16:03:08Z`) to EAS (`2026-10-30T16:03:08.000Z`).
pub fn eas_date(utc: &str) -> String {
    match utc.strip_suffix('Z') {
        Some(base) if !base.contains('.') => format!("{base}.000Z"),
        _ => utc.to_owned(),
    }
}

pub fn text_to_html(text: &str) -> String {
    escape_html(text)
        .replace("\r\n", "\n")
        .replace('\n', "<br>\n")
}

pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A crude HTML to text conversion for clients that only want plain text.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let raw = rest[start + 1..start + end].to_ascii_lowercase();
        let closing = raw.starts_with('/');
        let name = raw
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_owned();
        if matches!(
            name.as_str(),
            "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3"
        ) {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
        if matches!(name.as_str(), "style" | "script") && !closing {
            // Skip the contents up to the closing tag.
            let close = format!("</{name}");
            rest = rest
                .to_ascii_lowercase()
                .find(&close)
                .map_or("", |i| &rest[i..]);
        }
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {

    #[test]
    fn maps_importance_conversations_and_verbs() {
        let mut msg: Email = serde_json::from_value(serde_json::json!({
            "id": "m1", "blobId": "b", "receivedAt": "2026-10-30T10:00:00Z",
        }))
        .unwrap();
        assert_eq!(importance(&msg), "1");
        msg.priority = Some("1 (Highest)".to_owned());
        assert_eq!(importance(&msg), "2");
        msg.importance = Some("Low".to_owned());
        assert_eq!(importance(&msg), "0", "Importance wins over X-Priority");

        assert_eq!(conversation_id("t1"), conversation_id("t1"));
        assert_ne!(conversation_id("t1"), conversation_id("t2"));
        assert_eq!(conversation_id("t1").len(), 16);
        let earlier = conversation_index("2026-10-30T10:00:00Z");
        assert_eq!(earlier.len(), 5);
        assert!(earlier < conversation_index("2026-10-31T10:00:00Z"));

        use crate::eas::view::{ANSWERED, FORWARDED, READ};
        assert!(last_verb(READ).is_none());
        assert_eq!(last_verb(ANSWERED).unwrap().text(), Some("1"));
        assert_eq!(last_verb(FORWARDED).unwrap().text(), Some("3"));
    }

    use super::*;

    fn addr(name: Option<&str>, email: &str) -> EmailAddress {
        EmailAddress {
            name: name.map(str::to_owned),
            email: Some(email.to_owned()),
        }
    }

    #[test]
    fn formats_addresses() {
        let list = [
            addr(Some("Rob \"B\""), "rob@example.com"),
            addr(None, "x@example.com"),
        ];
        assert_eq!(
            format_addresses(Some(&list)).unwrap(),
            "\"Rob B\" <rob@example.com>, x@example.com"
        );
        assert_eq!(
            display_names(Some(&list)).unwrap(),
            "Rob \"B\"; x@example.com"
        );
        assert_eq!(format_addresses(None), None);
    }

    #[test]
    fn converts_dates() {
        assert_eq!(eas_date("2026-10-30T16:03:08Z"), "2026-10-30T16:03:08.000Z");
        assert_eq!(
            eas_date("2026-10-30T16:03:08.120Z"),
            "2026-10-30T16:03:08.120Z"
        );
    }

    #[test]
    fn converts_between_html_and_text() {
        assert_eq!(text_to_html("a < b\nc"), "a &lt; b<br>\nc");
        assert_eq!(
            html_to_text("<html><style>p{x}</style><p>Hi&nbsp;there</p>a &amp; b<br/>c</html>"),
            "\nHi there\na & b\nc"
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(floor_char_boundary("héllo", 2), 1);
        assert_eq!(floor_char_boundary("héllo", 3), 3);
        assert_eq!(floor_char_boundary("ab", 10), 2);
    }

    #[test]
    fn file_references_carry_the_content_type() {
        let part = BodyPart {
            part_id: None,
            blob_id: Some("b1".to_owned()),
            size: 3,
            name: None,
            mime_type: "image/png".to_owned(),
            cid: None,
            disposition: None,
        };
        let reference = file_reference(&part, None);
        assert_eq!(parse_file_reference(&reference), ("b1", "image/png", None));
        let shared = file_reference(&part, Some("acc"));
        assert_eq!(
            parse_file_reference(&shared),
            ("b1", "image/png", Some("acc"))
        );
        assert_eq!(
            parse_file_reference("old"),
            ("old", "application/octet-stream", None)
        );
    }

    #[test]
    fn chooses_first_supported_body_type() {
        let pref = |t| BodyPreference {
            body_type: t,
            truncation_size: None,
            all_or_none: false,
            preview: None,
        };
        assert_eq!(BodyPreference::choose(&[pref(3), pref(2)]).body_type, 2);
        assert_eq!(BodyPreference::choose(&[]).body_type, 1);
    }
}
