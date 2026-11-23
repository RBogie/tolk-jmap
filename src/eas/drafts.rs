//! Drafts through Sync (EAS 16): the device adds and changes messages in a
//! mail folder (normally Drafts) field by field, and sends one with
//! `<Send>`. A draft is a JMAP email with `$draft`; an edit replaces it by a
//! new email that keeps the device's ServerId. Attachments come as
//! `<Attachments>` with `<Add>` (the file, under a ClientId) and `<Delete>`
//! (a FileReference); the reply maps each ClientId to its FileReference.

use mail_builder::MessageBuilder;
use mail_parser::{Message, MessageParser};

use super::{
    Request,
    compose::{builder_addresses, recipients, strip_bcc},
    device_body::{self, Content},
    outbox,
    sync::Folder,
};
use crate::{
    jmap::{BodyValues, Email, JmapClient},
    store::SendRecord,
    wbxml::{
        Element,
        cp::{airsyncbase as base, email, email2},
    },
};

/// Whether a Change carries draft content rather than only flags.
pub(super) fn is_draft_edit(data: &Element) -> bool {
    [
        email::To,
        email::Cc,
        email2::Bcc,
        email::Subject,
        base::Body,
        base::Attachments,
        email2::Send,
    ]
    .iter()
    .any(|tag| data.find(*tag).is_some())
}

/// Whether the device asks to send the draft.
pub(super) fn wants_send(data: &Element) -> bool {
    data.find(email2::Send).is_some()
}

/// A stored draft and, for each attachment the device added, its ClientId
/// with the new FileReference.
pub(super) struct Saved {
    pub id: String,
    pub attachments: Vec<(String, String)>,
}

/// A file in a draft: one the device adds, or one kept from before.
struct File {
    name: String,
    content_type: String,
    bytes: Vec<u8>,
    cid: Option<String>,
}

/// Stores a new draft from the device's fields.
pub(super) async fn create(
    req: &Request<'_>,
    jmap: &JmapClient,
    folder: &Folder,
    data: &Element,
) -> anyhow::Result<Saved> {
    let added = added_files(data)?;
    let raw = build(req.email, data, None, &[], &added)?;
    let id = jmap
        .import_email(raw, &folder.jmap_id, &["$draft", "$seen"])
        .await?;
    saved(jmap, id, &added).await
}

/// Replaces a draft by one with the device's changes.
pub(super) async fn replace(
    req: &Request<'_>,
    jmap: &JmapClient,
    folder: &Folder,
    old_id: &str,
    data: &Element,
) -> anyhow::Result<Saved> {
    let base = jmap
        .get_emails(&[old_id], BodyValues::Both, 0)
        .await?
        .into_iter()
        .next();
    let added = added_files(data)?;
    let kept = match &base {
        Some(base) => kept_files(jmap, base, data).await?,
        None => Vec::new(),
    };
    let raw = build(req.email, data, base.as_ref(), &kept, &added)?;
    let new_id = jmap
        .import_email(raw, &folder.jmap_id, &["$draft", "$seen"])
        .await?;
    jmap.set(
        crate::jmap::EMAIL,
        crate::jmap::SetRequest::default().destroy(vec![old_id.to_owned()]),
    )
    .await?;
    req.store
        .repoint_item(req.user, folder.id, old_id, &new_id)?;
    saved(jmap, new_id, &added).await
}

/// The files the device adds (`<Attachments><Add>`), with their ClientIds.
fn added_files(data: &Element) -> anyhow::Result<Vec<(String, File)>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let Some(attachments) = data.find(base::Attachments) else {
        return Ok(Vec::new());
    };
    attachments
        .find_all(base::Add)
        .map(|add| {
            let client_id = add
                .child_text(base::ClientId)
                .ok_or_else(|| anyhow::anyhow!("attachment without ClientId"))?;
            let content: String = add
                .child_text(base::Content)
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            let bytes = STANDARD.decode(content)?;
            let inline = add.child_text(base::IsInline) == Some("1");
            Ok((
                client_id.to_owned(),
                File {
                    name: add
                        .child_text(base::DisplayName)
                        .unwrap_or("attachment")
                        .to_owned(),
                    content_type: add
                        .child_text(base::ContentType)
                        .unwrap_or("application/octet-stream")
                        .to_owned(),
                    bytes,
                    cid: add
                        .child_text(base::ContentId)
                        .filter(|_| inline)
                        .map(str::to_owned),
                },
            ))
        })
        .collect()
}

/// The earlier draft's attachments, minus those the device deletes.
async fn kept_files(jmap: &JmapClient, base: &Email, data: &Element) -> anyhow::Result<Vec<File>> {
    let deleted: Vec<&str> = data
        .find(base::Attachments)
        .map(|a| {
            a.find_all(base::Delete)
                .filter_map(|d| d.child_text(base::FileReference))
                .map(|r| super::email::parse_file_reference(r).0)
                .collect()
        })
        .unwrap_or_default();
    let mut files = Vec::new();
    for part in &base.attachments {
        let Some(blob) = part.blob_id.as_deref() else {
            continue;
        };
        if deleted.contains(&blob) {
            continue;
        }
        let name = part.name.clone().unwrap_or_else(|| "attachment".to_owned());
        files.push(File {
            bytes: jmap.download(blob, &name, &part.mime_type).await?,
            name,
            content_type: part.mime_type.clone(),
            cid: part
                .cid
                .as_deref()
                .filter(|_| part.disposition.as_deref() == Some("inline"))
                .map(|c| c.trim_matches(['<', '>']).to_owned()),
        });
    }
    Ok(files)
}

/// Maps the added files to the stored draft's attachments: they are the
/// last ones, in the order they were added.
async fn saved(jmap: &JmapClient, id: String, added: &[(String, File)]) -> anyhow::Result<Saved> {
    if added.is_empty() {
        return Ok(Saved {
            id,
            attachments: Vec::new(),
        });
    }
    let stored = jmap
        .get_emails(&[id.as_str()], BodyValues::None, 0)
        .await?
        .into_iter()
        .next()
        .map(|e| e.attachments)
        .unwrap_or_default();
    let first = stored.len().saturating_sub(added.len());
    let attachments = added
        .iter()
        .zip(&stored[first..])
        .map(|((client_id, _), part)| (client_id.clone(), super::email::file_reference(part, None)))
        .collect();
    Ok(Saved { id, attachments })
}

/// The `<Attachments>` for a Sync response: each ClientId with its
/// FileReference.
pub(super) fn attachment_references(saved: &Saved) -> Option<Element> {
    if saved.attachments.is_empty() {
        return None;
    }
    let mut attachments = Element::new(base::Attachments);
    for (client_id, reference) in &saved.attachments {
        attachments.push(
            Element::new(base::Attachment)
                .child(Element::with_text(base::ClientId, client_id.as_str()))
                .child(Element::with_text(base::FileReference, reference.as_str())),
        );
    }
    Some(attachments)
}

/// The body types a draft can be: plain text, HTML or a whole message.
const BODY_TYPES: &[u8] = &[device_body::PLAIN, device_body::HTML, device_body::MIME];

/// The fields of a draft the device sent that cannot be stored: a body of
/// another type (RTF).
pub(super) fn unsupported(data: &Element) -> Vec<&'static str> {
    device_body::unsupported(data, BODY_TYPES)
        .into_iter()
        .collect()
}

/// The key a draft's send is recorded under (see [`outbox`]).
pub(super) fn send_key(id: &str) -> String {
    format!("draft:{id}")
}

/// What an earlier send of this draft left, if any.
pub(super) fn send_record(req: &Request<'_>, id: &str) -> anyhow::Result<Option<SendRecord>> {
    req.store
        .send_record(req.user, req.device_id, &send_key(id))
}

/// Sends a stored draft once and deletes it (the copy goes to Sent).
pub(super) async fn send(
    req: &Request<'_>,
    jmap: &JmapClient,
    id: &str,
) -> anyhow::Result<outbox::Outcome> {
    let key = send_key(id);
    let outcome = if send_record(req, id)? == Some(SendRecord::Sent) {
        outbox::Outcome::AlreadySent
    } else {
        let Some(draft) = jmap
            .get_emails(&[id], BodyValues::None, 0)
            .await?
            .into_iter()
            .next()
        else {
            tracing::warn!(draft = id, "draft to send is gone");
            return Ok(outbox::Outcome::Rejected(outbox::Refusal::Other));
        };
        let raw = jmap
            .download(&draft.blob_id, "draft.eml", "message/rfc822")
            .await?;
        let to = MessageParser::default()
            .parse(&raw)
            .map(|parsed| recipients(&parsed))
            .unwrap_or_default();
        if to.is_empty() {
            tracing::warn!(draft = id, "draft to send has no recipients");
            return Ok(outbox::Outcome::Rejected(outbox::Refusal::NoRecipients));
        }
        let message = outbox::Message {
            key: Some(&key),
            mail_from: req.email,
            rcpt_to: &to,
            keep_copy: true,
        };
        outbox::send(req, jmap, &message, strip_bcc(&raw)).await?
    };
    if matches!(
        outcome,
        outbox::Outcome::Sent | outbox::Outcome::AlreadySent
    ) {
        if let Err(e) = jmap.destroy_email(id).await {
            tracing::warn!(error = format!("{e:#}"), "sent draft left in Drafts");
        }
        tracing::info!(draft = id, "device sent a draft");
    }
    Ok(outcome)
}

/// The message for the device's fields, on top of an earlier version, with
/// the files kept from it and those the device adds (which come last, see
/// [`saved`]).
fn build(
    from: &str,
    data: &Element,
    base: Option<&Email>,
    kept: &[File],
    added: &[(String, File)],
) -> anyhow::Result<Vec<u8>> {
    let field = |tag| data.child_text(tag).map(str::to_owned);
    // A field the device left out is removed (MS-ASCMD 2.2.3.24), except
    // the body, which stays as it was.
    // A body of a type a draft cannot have (RTF) is refused before this
    // (see `UNSUPPORTED_FIELDS`), or else left out, like a body not sent.
    let content = device_body::of(data).and_then(|b| b.content(BODY_TYPES));
    // A body of Type 4 is a whole message (MS-ASAIRS 2.2.2.22.1): its text,
    // HTML and files make the draft, and its headers stand in for the
    // fields the device does not send next to it.
    let mime = match &content {
        Some(Content::Mime(raw)) => Some(
            MessageParser::default()
                .parse(*raw)
                .ok_or_else(|| anyhow::anyhow!("draft body is not a MIME message"))?,
        ),
        _ => None,
    };
    let (text, html) = match content {
        Some(Content::Text(text)) => (text.into_owned(), None),
        Some(Content::Html(html)) => (super::email::html_to_text(&html), Some(html.into_owned())),
        Some(Content::Mime(_)) => mime
            .as_ref()
            .map(device_body::mime_bodies)
            .unwrap_or_default(),
        None => base.map(base_bodies).unwrap_or_default(),
    };

    // The address fields arrive as header text; let the parser read them.
    let headers: String = [
        ("To", field(email::To)),
        ("Cc", field(email::Cc)),
        ("Bcc", field(email2::Bcc)),
    ]
    .iter()
    .filter_map(|(name, value)| Some(format!("{name}: {}\r\n", value.as_deref()?)))
    .collect();
    let headers = format!("{headers}\r\n");
    let parsed = MessageParser::default().parse(headers.as_bytes());
    let parsed = parsed.as_ref();
    let mime = mime.as_ref();
    let mut builder = MessageBuilder::new().from(from.to_owned());
    let to = parsed.and_then(|p| p.to()).or_else(|| mime?.to());
    if let Some(a) = builder_addresses(to) {
        builder = builder.to(a);
    }
    let cc = parsed.and_then(|p| p.cc()).or_else(|| mime?.cc());
    if let Some(a) = builder_addresses(cc) {
        builder = builder.cc(a);
    }
    let bcc = parsed.and_then(|p| p.bcc()).or_else(|| mime?.bcc());
    if let Some(a) = builder_addresses(bcc) {
        builder = builder.bcc(a);
    }
    let subject = field(email::Subject).or_else(|| Some(mime?.subject()?.to_owned()));
    if let Some(subject) = subject {
        builder = builder.subject(subject);
    }
    // A reply saved as a draft stays in its thread; EAS has no field for
    // these headers, so they come from the message the device sent, or
    // else from the earlier draft.
    let ids = |list: Option<mail_parser::TextList>| -> Vec<String> {
        list.map(|l| l.iter().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let mut in_reply_to = ids(mime.and_then(|m| m.in_reply_to()));
    let mut references = ids(mime.and_then(|m| m.references()));
    if let Some(base) = base {
        if in_reply_to.is_empty() {
            in_reply_to = base.in_reply_to.clone();
        }
        if references.is_empty() {
            references = base.references.clone();
        }
    }
    if let Some(id) = in_reply_to.into_iter().next() {
        builder = builder.in_reply_to(id);
    }
    if !references.is_empty() {
        builder = builder.references(references);
    }
    builder = builder.text_body(text);
    if let Some(html) = html {
        builder = builder.html_body(html);
    }
    let mime_files = mime.map(mime_files).unwrap_or_default();
    // A kept file the message carries again is there once.
    let kept = kept
        .iter()
        .filter(|k| !mime_files.iter().any(|m| m.bytes == k.bytes));
    let files = mime_files
        .iter()
        .chain(kept)
        .chain(added.iter().map(|(_, f)| f));
    for file in files {
        let (content_type, bytes) = (file.content_type.as_str(), file.bytes.as_slice());
        builder = match &file.cid {
            Some(cid) => builder.inline(content_type, cid.as_str(), bytes),
            None => builder.attachment(content_type, file.name.as_str(), bytes),
        };
    }
    Ok(builder.write_to_vec()?)
}

/// The attachments and inline files of a message the device sent.
fn mime_files(mime: &Message) -> Vec<File> {
    mime.attachments()
        .map(|part| {
            let content_type = part
                .content_type()
                .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            File {
                name: part.attachment_name().unwrap_or("attachment").to_owned(),
                content_type,
                bytes: part.decoded().into_owned(),
                cid: part
                    .content_id()
                    .filter(|_| part.is_inline())
                    .map(|c| c.trim_matches(['<', '>']).to_owned()),
            }
        })
        .collect()
}

/// The text and HTML of an earlier draft.
fn base_bodies(email: &Email) -> (String, Option<String>) {
    let join = |parts: &[crate::jmap::BodyPart]| -> String {
        parts
            .iter()
            .filter_map(|p| email.body_values.get(p.part_id.as_deref()?))
            .map(|v| v.value.as_str())
            .collect()
    };
    let html = email
        .html_body
        .iter()
        .any(|p| p.mime_type.eq_ignore_ascii_case("text/html"))
        .then(|| join(&email.html_body));
    (join(&email.text_body), html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_draft_from_device_fields() {
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData)
            .child(Element::with_text(
                email::To,
                "\"Ann Lee\" <ann@example.com>, bob@example.com",
            ))
            .child(Element::with_text(email2::Bcc, "secret@example.com"))
            .child(Element::with_text(email::Subject, "Plan"))
            .child(
                Element::new(base::Body)
                    .child(Element::with_text(base::Type, "1"))
                    .child(Element::with_text(base::Data, "Draft text")),
            );
        let raw = build("rob@example.com", &data, None, &[], &[]).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(parsed.subject(), Some("Plan"));
        assert_eq!(recipients(&parsed).len(), 3);
        assert_eq!(
            parsed.body_text(0).as_deref().map(str::trim),
            Some("Draft text")
        );
        assert!(is_draft_edit(&data));
        assert!(!wants_send(&data));
    }

    /// A draft as iOS Mail sends it: the body as a whole message (#52).
    const IOS_DRAFT: &str = "Content-Type: multipart/alternative;\r\n\
        \tboundary=Apple-Mail-1\r\n\
        Content-Transfer-Encoding: 7bit\r\n\
        From: Someone Else <other@example.com>\r\n\
        Mime-Version: 1.0 (1.0)\r\n\
        Subject: Draft test\r\n\
        To: ann@example.com\r\n\
        In-Reply-To: <orig@example.com>\r\n\
        X-Universally-Unique-Identifier: 1234\r\n\
        \r\n\
        --Apple-Mail-1\r\n\
        Content-Type: text/plain; charset=us-ascii\r\n\
        \r\n\
        Sent from my iPhone\r\n\
        --Apple-Mail-1\r\n\
        Content-Type: text/html; charset=utf-8\r\n\
        \r\n\
        <html><body><div>Sent from my iPhone</div></body></html>\r\n\
        --Apple-Mail-1--\r\n";

    fn mime_body(data: Element) -> Element {
        Element::new(base::Body)
            .child(Element::with_text(base::Type, "4"))
            .child(data)
    }

    #[test]
    fn a_mime_body_gives_the_draft_its_parts() {
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData)
            .child(Element::with_text(email::To, "bob@example.com"))
            .child(Element::with_text(email::Subject, "Plan"))
            .child(mime_body(Element::with_text(base::Data, IOS_DRAFT)));
        let raw = build("rob@example.com", &data, None, &[], &[]).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        // The device's fields win over the message's headers; From is ours.
        assert_eq!(parsed.subject(), Some("Plan"));
        assert_eq!(recipients(&parsed), ["bob@example.com"]);
        assert_eq!(
            parsed.from().unwrap().first().unwrap().address(),
            Some("rob@example.com")
        );
        let text = parsed.body_text(0).unwrap();
        assert_eq!(text.trim(), "Sent from my iPhone");
        assert!(!String::from_utf8_lossy(&raw).contains("X-Universally"));
        assert!(
            parsed
                .body_html(0)
                .unwrap()
                .contains("<div>Sent from my iPhone</div>")
        );
        assert_eq!(
            parsed.in_reply_to().and_then(|l| l.first()),
            Some("orig@example.com")
        );
    }

    #[test]
    fn a_mime_body_stands_in_for_fields_left_out() {
        // Data may also come as opaque bytes.
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData).child(mime_body(
            Element::with_opaque(base::Data, IOS_DRAFT.as_bytes().to_vec()),
        ));
        let raw = build("rob@example.com", &data, None, &[], &[]).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(parsed.subject(), Some("Draft test"));
        assert_eq!(recipients(&parsed), ["ann@example.com"]);
        assert_eq!(
            parsed.body_text(0).as_deref().map(str::trim),
            Some("Sent from my iPhone")
        );
    }

    #[test]
    fn a_mime_body_carries_its_files_once() {
        let message = "Content-Type: multipart/mixed; boundary=b\r\n\
            \r\n\
            --b\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            See the plan\r\n\
            --b\r\n\
            Content-Type: application/pdf; name=plan.pdf\r\n\
            Content-Disposition: attachment; filename=plan.pdf\r\n\
            Content-Transfer-Encoding: base64\r\n\
            \r\n\
            JVBERi0=\r\n\
            --b--\r\n";
        let kept = [
            File {
                name: "plan.pdf".into(),
                content_type: "application/pdf".into(),
                bytes: b"%PDF-".to_vec(),
                cid: None,
            },
            File {
                name: "old.txt".into(),
                content_type: "text/plain".into(),
                bytes: b"old".to_vec(),
                cid: None,
            },
        ];
        let added = [(
            "c1".to_owned(),
            File {
                name: "new.txt".into(),
                content_type: "text/plain".into(),
                bytes: b"new".to_vec(),
                cid: None,
            },
        )];
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData)
            .child(mime_body(Element::with_text(base::Data, message)));
        let raw = build("rob@example.com", &data, None, &kept, &added).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        let names: Vec<&str> = parsed
            .attachments()
            .map(|a| a.attachment_name().unwrap())
            .collect();
        // The added file comes last, where `saved` looks for it.
        assert_eq!(names, ["plan.pdf", "old.txt", "new.txt"]);
        assert_eq!(&*parsed.attachments().next().unwrap().decoded(), b"%PDF-");
        assert_eq!(
            parsed.body_text(0).as_deref().map(str::trim),
            Some("See the plan")
        );
    }

    #[test]
    fn an_edit_removes_left_out_fields_and_keeps_body_and_thread() {
        let base: Email = serde_json::from_value(serde_json::json!({
            "id": "d1", "blobId": "b1", "receivedAt": "2026-11-15T10:00:00Z",
            "subject": "Re: Plans",
            "to": [{ "email": "ann@example.com" }],
            "cc": [{ "email": "bob@example.com" }],
            "inReplyTo": ["orig@example.com"],
            "references": ["first@example.com", "orig@example.com"],
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": { "1": { "value": "Sounds good" } },
        }))
        .unwrap();
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData)
            .child(Element::with_text(email::To, "ann@example.com"));
        let raw = build("rob@example.com", &data, Some(&base), &[], &[]).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert!(parsed.cc().is_none());
        assert_eq!(parsed.subject(), None);
        assert_eq!(
            parsed.body_text(0).as_deref().map(str::trim),
            Some("Sounds good")
        );
        assert_eq!(
            parsed.in_reply_to().and_then(|l| l.first()),
            Some("orig@example.com")
        );
        let references: Vec<&str> = parsed.references().unwrap().iter().collect();
        assert_eq!(references, ["first@example.com", "orig@example.com"]);
    }

    #[test]
    fn an_rtf_body_is_reported_and_not_read_as_text() {
        let rtf = Element::new(base::Body)
            .child(Element::with_text(base::Type, "3"))
            .child(Element::with_text(base::Data, "e1xydGYxIEhpfQ=="));
        let data = Element::new(crate::wbxml::cp::airsync::ApplicationData)
            .child(Element::with_text(email::Subject, "Plan"))
            .child(rtf);
        assert_eq!(unsupported(&data), ["Body of type 3 (RTF)"]);
        // Logged only (UNSUPPORTED_FIELDS=log): the body stays as it was.
        let base: Email = serde_json::from_value(serde_json::json!({
            "id": "d1", "blobId": "b1", "receivedAt": "2026-11-15T10:00:00Z",
            "textBody": [{ "partId": "1", "type": "text/plain" }],
            "bodyValues": { "1": { "value": "Sounds good" } },
        }))
        .unwrap();
        let raw = build("rob@example.com", &data, Some(&base), &[], &[]).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(
            parsed.body_text(0).as_deref().map(str::trim),
            Some("Sounds good")
        );
    }
}
