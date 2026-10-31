//! Drafts through Sync (EAS 16): the device adds and changes messages in a
//! mail folder (normally Drafts) field by field, and sends one with
//! `<Send>`. A draft is a JMAP email with `$draft`; an edit replaces it by a
//! new email that keeps the device's ServerId. Attachments come as
//! `<Attachments>` with `<Add>` (the file, under a ClientId) and `<Delete>`
//! (a FileReference); the reply maps each ClientId to its FileReference.

use mail_builder::MessageBuilder;
use mail_parser::MessageParser;

use super::{
    Request,
    compose::{builder_addresses, recipients, strip_bcc},
    sync::Folder,
};
use crate::{
    jmap::{BodyValues, Email, EmailAddress, JmapClient},
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
    let files: Vec<&File> = added.iter().map(|(_, f)| f).collect();
    let raw = build(req.email, data, None, &files)?;
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
    let files: Vec<&File> = kept.iter().chain(added.iter().map(|(_, f)| f)).collect();
    let raw = build(req.email, data, base.as_ref(), &files)?;
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

/// Sends a stored draft and deletes it (the copy goes to Sent).
pub(super) async fn send(req: &Request<'_>, jmap: &JmapClient, id: &str) -> anyhow::Result<()> {
    let draft = jmap
        .get_emails(&[id], BodyValues::None, 0)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("draft {id} is gone"))?;
    let raw = jmap
        .download(&draft.blob_id, "draft.eml", "message/rfc822")
        .await?;
    let parsed = MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| anyhow::anyhow!("draft {id} does not parse"))?;
    let to = recipients(&parsed);
    anyhow::ensure!(!to.is_empty(), "draft {id} has no recipients");
    jmap.send_message(strip_bcc(&raw), req.email, &to, true)
        .await?;
    jmap.set(
        crate::jmap::EMAIL,
        crate::jmap::SetRequest::default().destroy(vec![id.to_owned()]),
    )
    .await?;
    tracing::info!(recipients = to.len(), "device sent a draft");
    Ok(())
}

/// The message for the device's fields, on top of an earlier version.
fn build(
    from: &str,
    data: &Element,
    base: Option<&Email>,
    files: &[&File],
) -> anyhow::Result<Vec<u8>> {
    let field = |tag| data.child_text(tag).map(str::to_owned);
    let joined = |list: &Option<Vec<EmailAddress>>| {
        list.as_ref().map(|list| {
            list.iter()
                .filter_map(|a| {
                    let address = a.email.as_deref()?;
                    Some(match a.name.as_deref().filter(|n| !n.is_empty()) {
                        Some(name) => format!("\"{}\" <{address}>", name.replace('"', "")),
                        None => address.to_owned(),
                    })
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    };
    let to = field(email::To).or_else(|| base.and_then(|b| joined(&b.to)));
    let cc = field(email::Cc).or_else(|| base.and_then(|b| joined(&b.cc)));
    let bcc = field(email2::Bcc);
    let subject = field(email::Subject).or_else(|| base.and_then(|b| b.subject.clone()));
    let (text, html) = match data.find(base::Body) {
        Some(body) => {
            let content = body.child_text(base::Data).unwrap_or_default().to_owned();
            match body.child_text(base::Type) {
                Some("2") => (super::email::html_to_text(&content), Some(content)),
                _ => (content, None),
            }
        }
        None => base.map(base_bodies).unwrap_or_default(),
    };

    // The address fields arrive as header text; let the parser read them.
    let headers: String = [("To", &to), ("Cc", &cc), ("Bcc", &bcc)]
        .iter()
        .filter_map(|(name, value)| Some(format!("{name}: {}\r\n", value.as_deref()?)))
        .collect();
    let headers = format!("{headers}\r\n");
    let parsed = MessageParser::default().parse(headers.as_bytes());
    let mut builder = MessageBuilder::new().from(from.to_owned());
    if let Some(parsed) = &parsed {
        if let Some(a) = builder_addresses(parsed.to()) {
            builder = builder.to(a);
        }
        if let Some(a) = builder_addresses(parsed.cc()) {
            builder = builder.cc(a);
        }
        if let Some(a) = builder_addresses(parsed.bcc()) {
            builder = builder.bcc(a);
        }
    }
    if let Some(subject) = subject {
        builder = builder.subject(subject);
    }
    builder = builder.text_body(text);
    if let Some(html) = html {
        builder = builder.html_body(html);
    }
    for file in files {
        let (content_type, bytes) = (file.content_type.as_str(), file.bytes.as_slice());
        builder = match &file.cid {
            Some(cid) => builder.inline(content_type, cid.as_str(), bytes),
            None => builder.attachment(content_type, file.name.as_str(), bytes),
        };
    }
    Ok(builder.write_to_vec()?)
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
        let raw = build("rob@example.com", &data, None, &[]).unwrap();
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
}
