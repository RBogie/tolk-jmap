//! SendMail, SmartReply and SmartForward (MS-ASCMD 2.2.1.17, .19, .20).
//!
//! The device's MIME message is delivered through JMAP (`Email/import` into
//! Sent plus `EmailSubmission/set`) with an explicit envelope, so a Bcc
//! header never reaches the recipients. SmartReply and SmartForward without
//! `<ReplaceMime>` carry only the new text: the server adds the quoted
//! original and, for a forward, the original's attachments.

use mail_builder::{MessageBuilder, headers::address::Address as BuilderAddress};
use mail_parser::{AddressList, Message, MessageParser};
use serde_json::{Map, Value};

use super::{CommandError, Request, email, query::EasQuery, search, sync};
use crate::{
    jmap::{self, BodyValues, Email, JmapClient},
    wbxml::{Element, Tag, cp::compose_mail as cm},
};

mod status {
    pub const INVALID_MIME: &str = "107";
    pub const SERVER_ERROR: &str = "110";
    pub const ALREADY_SENT: &str = "118";
    pub const NO_RECIPIENT: &str = "119";
    pub const SUBMISSION_FAILED: &str = "120";
    pub const SOURCE_NOT_FOUND: &str = "150";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Send,
    Reply,
    Forward,
}

impl Kind {
    pub fn from_command(cmd: &str) -> Option<Self> {
        Some(match cmd {
            "SendMail" => Kind::Send,
            "SmartReply" => Kind::Reply,
            "SmartForward" => Kind::Forward,
            _ => return None,
        })
    }

    fn root(self) -> Tag {
        match self {
            Kind::Send => cm::SendMail,
            Kind::Reply => cm::SmartReply,
            Kind::Forward => cm::SmartForward,
        }
    }
}

/// Sends the message; `None` means success (answered with an empty body),
/// otherwise the error response.
pub async fn handle(
    req: &Request<'_>,
    jmap: &JmapClient,
    query: &EasQuery,
    kind: Kind,
) -> Result<Option<Element>, CommandError> {
    let body = req.body_with_root(kind.root())?;
    let failure = |status: &str| {
        Some(Element::new(kind.root()).child(Element::with_text(cm::Status, status)))
    };

    let Some(mime) = body.find(cm::Mime).and_then(|m| {
        m.opaque()
            .map(<[u8]>::to_vec)
            .or_else(|| m.text().map(|t| t.as_bytes().to_vec()))
    }) else {
        return Ok(failure(status::INVALID_MIME));
    };
    let save_in_sent =
        body.find(cm::SaveInSentItems).is_some() || query_param(query, "SaveInSent") == Some("T");
    let client_id = body.child_text(cm::ClientId).unwrap_or_default();
    if !client_id.is_empty()
        && !req
            .store
            .claim_client_id(req.user, req.device_id, client_id)?
    {
        tracing::info!(client_id, "message already sent, ignoring retry");
        return Ok(failure(status::ALREADY_SENT));
    }

    let result = send(req, jmap, query, body, kind, &mime, save_in_sent).await;
    if !matches!(result, Ok(None)) && !client_id.is_empty() {
        req.store
            .release_client_id(req.user, req.device_id, client_id)?;
    }
    Ok(result?.and_then(failure))
}

fn query_param<'a>(query: &'a EasQuery, name: &str) -> Option<&'a str> {
    query
        .params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Returns `None` on success or the failure status.
async fn send(
    req: &Request<'_>,
    jmap: &JmapClient,
    query: &EasQuery,
    body: &Element,
    kind: Kind,
    mime: &[u8],
    save_in_sent: bool,
) -> Result<Option<&'static str>, CommandError> {
    let Some(parsed) = MessageParser::default().parse(mime) else {
        return Ok(Some(status::INVALID_MIME));
    };
    let from = parsed
        .from()
        .and_then(|list| list.first())
        .and_then(|m| m.address())
        .unwrap_or(req.email)
        .to_owned();
    let recipients = recipients(&parsed);
    if recipients.is_empty() {
        return Ok(Some(status::NO_RECIPIENT));
    }

    // The original and the client for its account (it may be in a shared
    // mailbox); the message itself is always sent from the user's account.
    let (original, source) = if kind == Kind::Send || body.find(cm::ReplaceMime).is_some() {
        (None, std::borrow::Cow::Borrowed(jmap))
    } else {
        match source_email(req, jmap, query, body).await? {
            Some((original, source)) => (Some(original), source),
            None => return Ok(Some(status::SOURCE_NOT_FOUND)),
        }
    };
    let raw = match &original {
        None => strip_bcc(mime),
        Some(original) => {
            let mut attachments = Vec::new();
            if kind == Kind::Forward {
                for part in &original.attachments {
                    let Some(blob_id) = &part.blob_id else {
                        continue;
                    };
                    let name = part.name.clone().unwrap_or_else(|| "attachment".to_owned());
                    let data = source.download(blob_id, &name, &part.mime_type).await?;
                    attachments.push((part.mime_type.clone(), name, data));
                }
            }
            match with_original(&parsed, original, kind, attachments) {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "could not build the message");
                    return Ok(Some(status::SERVER_ERROR));
                }
            }
        }
    };

    if let Err(e) = jmap
        .send_message(raw, &from, &recipients, save_in_sent)
        .await
    {
        if jmap::is_unauthorized(&e) {
            return Err(e.into());
        }
        tracing::warn!(error = format!("{e:#}"), "message submission failed");
        return Ok(Some(status::SUBMISSION_FAILED));
    }
    tracing::info!(?kind, recipients = recipients.len(), "message sent");

    if let Some(original) = original {
        let keyword = if kind == Kind::Reply {
            "$answered"
        } else {
            "$forwarded"
        };
        let mut patch = Map::new();
        patch.insert(format!("keywords/{keyword}"), Value::Bool(true));
        let mut update = Map::new();
        update.insert(original.id.clone(), Value::Object(patch));
        if let Err(e) = source
            .set(
                crate::jmap::EMAIL,
                crate::jmap::SetRequest::default().update(update),
            )
            .await
        {
            tracing::warn!(error = format!("{e:#}"), "could not mark the original");
        }
    }
    Ok(None)
}

/// The email a SmartReply or SmartForward refers to, with both bodies, and
/// the client for its account.
async fn source_email<'a>(
    req: &Request<'_>,
    jmap: &'a JmapClient,
    query: &EasQuery,
    body: &Element,
) -> Result<Option<(Email, std::borrow::Cow<'a, JmapClient>)>, CommandError> {
    let source = body.find(cm::Source);
    // A message found by Search is referenced by its LongId (`folder:item`).
    let long_id = source.and_then(|s| s.child_text(cm::LongId));
    let folder_id = source
        .and_then(|s| s.child_text(cm::FolderId))
        .or_else(|| long_id.and_then(search::long_id_folder))
        .or_else(|| query_param(query, "CollectionId"));
    let item_id = source
        .and_then(|s| s.child_text(cm::ItemId))
        .or(long_id)
        .or_else(|| query_param(query, "ItemId"));
    let (Some(folder_id), Some(item_id)) = (folder_id, item_id) else {
        return Ok(None);
    };
    let Some(folder) = sync::resolve(req, folder_id)?.filter(|f| f.kind == sync::FolderKind::Mail)
    else {
        return Ok(None);
    };
    let Some(email_id) = folder.email_for(req, item_id)? else {
        return Ok(None);
    };
    let client = folder.client(jmap);
    let email = client
        .get_emails(&[email_id.as_str()], BodyValues::Both, 0)
        .await?
        .into_iter()
        .next();
    Ok(email.map(|email| (email, client)))
}

/// Every To, Cc and Bcc address, once each.
pub(super) fn recipients(msg: &Message) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for list in [msg.to(), msg.cc(), msg.bcc()].into_iter().flatten() {
        for mailbox in list.mailboxes() {
            if let Some(address) = mailbox.address()
                && !out.iter().any(|o| o.eq_ignore_ascii_case(address))
            {
                out.push(address.to_owned());
            }
        }
    }
    out
}

/// Removes Bcc header fields (with their continuation lines).
pub(super) fn strip_bcc(raw: &[u8]) -> Vec<u8> {
    let header_end = find(raw, b"\r\n\r\n")
        .map(|i| i + 2)
        .or_else(|| find(raw, b"\n\n").map(|i| i + 1))
        .unwrap_or(raw.len());
    let (headers, rest) = raw.split_at(header_end);
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    for line in headers.split_inclusive(|&b| b == b'\n') {
        if !matches!(line.first(), Some(b' ' | b'\t')) {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(rest);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Rebuilds the device's message with the original quoted below the new
/// text, threading headers, and the given extra attachments.
fn with_original(
    client: &Message,
    original: &Email,
    kind: Kind,
    extra_attachments: Vec<(String, String, Vec<u8>)>,
) -> anyhow::Result<Vec<u8>> {
    let mut builder = MessageBuilder::new();
    if let Some(a) = builder_addresses(client.from()) {
        builder = builder.from(a);
    }
    if let Some(a) = builder_addresses(client.to()) {
        builder = builder.to(a);
    }
    if let Some(a) = builder_addresses(client.cc()) {
        builder = builder.cc(a);
    }
    if let Some(a) = builder_addresses(client.reply_to()) {
        builder = builder.reply_to(a);
    }
    if let Some(subject) = client.subject() {
        builder = builder.subject(subject.to_owned());
    }
    if let Some(id) = client.message_id() {
        builder = builder.message_id(id.to_owned());
    }
    if let Some(id) = original.message_id.first() {
        if kind == Kind::Reply {
            builder = builder.in_reply_to(id.clone());
        }
        let mut references = original.references.clone();
        references.push(id.clone());
        builder = builder.references(references);
    }

    let quote = Quote::new(original, kind);
    let text = client.body_text(0).unwrap_or_default();
    builder = builder.text_body(format!("{text}{}", quote.text()));
    if client
        .html_body()
        .any(|p| p.is_content_type("text", "html"))
        && let Some(html) = client.body_html(0)
    {
        builder = builder.html_body(format!("{html}{}", quote.html()));
    }

    for part in client.attachments() {
        let content_type = part
            .content_type()
            .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let name = part.attachment_name().unwrap_or("attachment").to_owned();
        builder = builder.attachment(content_type, name, part.decoded().into_owned());
    }
    for (content_type, name, data) in extra_attachments {
        builder = builder.attachment(content_type, name, data);
    }
    Ok(builder.write_to_vec()?)
}

pub(super) fn builder_addresses(list: Option<AddressList>) -> Option<BuilderAddress<'static>> {
    let items: Vec<BuilderAddress<'static>> = list?
        .mailboxes()
        .filter_map(|m| {
            let address = m.address()?.to_owned();
            Some(match m.name() {
                Some(name) => BuilderAddress::from((name.to_owned(), address)),
                None => BuilderAddress::from(address),
            })
        })
        .collect();
    (!items.is_empty()).then(|| BuilderAddress::new_list(items))
}

/// The original message as it is quoted below a reply or forward.
struct Quote {
    kind: Kind,
    from: String,
    to: String,
    date: String,
    subject: String,
    text: String,
    html: Option<String>,
}

impl Quote {
    fn new(original: &Email, kind: Kind) -> Self {
        let text_of = |parts: &[jmap::BodyPart], want_html: bool| {
            parts
                .iter()
                .filter_map(|p| {
                    let value = &original.body_values.get(p.part_id.as_deref()?)?.value;
                    let is_html = p.mime_type.eq_ignore_ascii_case("text/html");
                    Some(match (want_html, is_html) {
                        (false, true) => email::html_to_text(value),
                        (true, false) => email::text_to_html(value),
                        _ => value.clone(),
                    })
                })
                .collect::<String>()
        };
        let has_html = original
            .html_body
            .iter()
            .any(|p| p.mime_type.eq_ignore_ascii_case("text/html"));
        Self {
            kind,
            from: email::format_addresses(original.from.as_deref()).unwrap_or_default(),
            to: email::format_addresses(original.to.as_deref()).unwrap_or_default(),
            date: readable_date(&original.received_at),
            subject: original.subject.clone().unwrap_or_default(),
            text: text_of(&original.text_body, false),
            html: has_html.then(|| text_of(&original.html_body, true)),
        }
    }

    fn forward_header(&self) -> [(&'static str, &str); 4] {
        [
            ("From", &self.from),
            ("Date", &self.date),
            ("Subject", &self.subject),
            ("To", &self.to),
        ]
    }

    fn text(&self) -> String {
        match self.kind {
            Kind::Reply => {
                let quoted: String = self.text.lines().map(|l| format!("> {l}\n")).collect();
                format!("\n\nOn {}, {} wrote:\n{quoted}", self.date, self.from)
            }
            _ => {
                let header: String = self
                    .forward_header()
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\n"))
                    .collect();
                format!(
                    "\n\n---------- Forwarded message ---------\n{header}\n{}",
                    self.text
                )
            }
        }
    }

    fn html(&self) -> String {
        let body = self
            .html
            .clone()
            .unwrap_or_else(|| email::text_to_html(&self.text));
        match self.kind {
            Kind::Reply => format!(
                "<br><div>On {}, {} wrote:<br></div><blockquote style=\"margin:0 0 0 .8ex;\
                 border-left:1px #ccc solid;padding-left:1ex\">{body}</blockquote>",
                email::escape_html(&self.date),
                email::escape_html(&self.from),
            ),
            _ => {
                let header: String = self
                    .forward_header()
                    .iter()
                    .map(|(name, value)| format!("{name}: {}<br>", email::escape_html(value)))
                    .collect();
                format!(
                    "<br><div>---------- Forwarded message ---------<br>{header}</div><br>{body}"
                )
            }
        }
    }
}

/// `2026-10-25T14:00:11Z` as `2026-10-25 14:00 UTC`.
fn readable_date(utc: &str) -> String {
    match (utc.get(..10), utc.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time} UTC"),
        _ => utc.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::jmap::{BodyPart, BodyValue, EmailAddress};

    const CLIENT_MIME: &[u8] = b"From: Rob <rob@example.com>\r\n\
        To: Ann <ann@example.com>\r\n\
        Bcc: secret@example.com,\r\n hidden@example.com\r\n\
        Subject: Re: Plans\r\n\
        Content-Type: text/plain; charset=utf-8\r\n\
        \r\n\
        Sounds good.\r\n";

    fn original() -> Email {
        let part = |id: &str, mime: &str| BodyPart {
            part_id: Some(id.to_owned()),
            blob_id: None,
            size: 10,
            name: None,
            mime_type: mime.to_owned(),
            cid: None,
            disposition: None,
        };
        let value = |v: &str| BodyValue {
            value: v.to_owned(),
            is_truncated: false,
        };
        Email {
            id: "orig".to_owned(),
            blob_id: "b".to_owned(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            size: 100,
            received_at: "2026-10-25T14:00:11Z".to_owned(),
            subject: Some("Plans".to_owned()),
            from: Some(vec![EmailAddress {
                name: Some("Ann".to_owned()),
                email: Some("ann@example.com".to_owned()),
            }]),
            to: Some(vec![EmailAddress {
                name: None,
                email: Some("rob@example.com".to_owned()),
            }]),
            cc: None,
            reply_to: None,
            html_body: vec![part("2", "text/html")],
            text_body: vec![part("1", "text/plain")],
            attachments: Vec::new(),
            body_values: [
                ("1".to_owned(), value("Lunch?\nAt noon.")),
                ("2".to_owned(), value("<p>Lunch?</p>")),
            ]
            .into(),
            message_id: vec!["orig@example.com".to_owned()],
            references: vec!["root@example.com".to_owned()],
            thread_id: None,
            preview: None,
            importance: None,
            priority: None,
        }
    }

    #[test]
    fn collects_all_recipients_once() {
        let msg = MessageParser::default().parse(CLIENT_MIME).unwrap();
        assert_eq!(
            recipients(&msg),
            [
                "ann@example.com",
                "secret@example.com",
                "hidden@example.com"
            ]
        );
    }

    #[test]
    fn strips_bcc_with_continuation_lines() {
        let stripped = String::from_utf8(strip_bcc(CLIENT_MIME)).unwrap();
        assert!(!stripped.to_lowercase().contains("bcc"));
        assert!(!stripped.contains("hidden@example.com"));
        assert!(stripped.contains("To: Ann <ann@example.com>\r\nSubject: Re: Plans"));
        assert!(stripped.ends_with("\r\n\r\nSounds good.\r\n"));
    }

    #[test]
    fn reply_quotes_the_original_and_threads() {
        let client = MessageParser::default().parse(CLIENT_MIME).unwrap();
        let raw = with_original(&client, &original(), Kind::Reply, Vec::new()).unwrap();
        let rebuilt = MessageParser::default().parse(&raw).unwrap();
        let text = rebuilt.body_text(0).unwrap().replace("\r\n", "\n");
        assert!(text.starts_with("Sounds good."), "{text}");
        assert!(
            text.contains("On 2026-10-25 14:00 UTC, \"Ann\" <ann@example.com> wrote:"),
            "{text}"
        );
        assert!(text.contains("> Lunch?\n> At noon."), "{text}");
        assert_eq!(
            rebuilt.in_reply_to().and_then(|l| l.first()),
            Some("orig@example.com")
        );
        let references: Vec<&str> = rebuilt.references().unwrap().iter().collect();
        assert_eq!(references, ["root@example.com", "orig@example.com"]);
        assert!(rebuilt.bcc().is_none());
    }

    #[test]
    fn forward_includes_header_and_attachments() {
        let client = MessageParser::default().parse(CLIENT_MIME).unwrap();
        let extra = vec![(
            "application/pdf".to_owned(),
            "plan.pdf".to_owned(),
            b"%PDF".to_vec(),
        )];
        let raw = with_original(&client, &original(), Kind::Forward, extra).unwrap();
        let rebuilt = MessageParser::default().parse(&raw).unwrap();
        let text = rebuilt.body_text(0).unwrap().replace("\r\n", "\n");
        assert!(
            text.contains("---------- Forwarded message ---------\nFrom: \"Ann\""),
            "{text}"
        );
        assert!(text.contains("Subject: Plans"), "{text}");
        let attachment = rebuilt.attachments().next().unwrap();
        assert_eq!(attachment.attachment_name(), Some("plan.pdf"));
        assert_eq!(&*attachment.decoded(), b"%PDF");
        assert!(rebuilt.in_reply_to().is_none());
    }
}
