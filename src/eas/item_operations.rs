//! ItemOperations (MS-ASCMD 2.2.1.10): fetching attachments and full items.
//!
//! `<Fetch>`: an attachment is addressed by the FileReference sent in Sync
//! and returned inline as base64 `<Data>`; an item by CollectionId plus
//! ServerId, with its body as requested.
//!
//! `<EmptyFolderContents>` ("Empty Trash") deletes every message of a mail
//! folder for good; one that is also in another folder only leaves this
//! one. With `<DeleteSubFolders>` the folders below it go too.

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;

use serde_json::{Map, json};

use super::{
    CommandError, CommandResult, Request,
    email::{self, BodyPreference},
    search, stream, sync,
};
use crate::{
    jmap::{self, JmapClient},
    wbxml::{
        Element,
        cp::{airsync, airsyncbase, item_operations as io, search as sp},
    },
};

mod status {
    pub const OK: &str = "1";
    pub const PROTOCOL_ERROR: &str = "2";
    pub const SERVER_ERROR: &str = "3";
    pub const NOT_FOUND: &str = "6";
    pub const BAD_RANGE: &str = "8";
    pub const INVALID_ATTACHMENT: &str = "15";
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(io::ItemOperations)?;
    let mut response = Element::new(io::Response);
    for operation in body.elements() {
        let reply = match operation.tag {
            io::Fetch => match fetch(req, jmap, operation).await {
                Ok(reply) => reply,
                Err(CommandError::Backend(e)) if !jmap::is_unauthorized(&e) => {
                    tracing::warn!(error = format!("{e:#}"), "fetch failed");
                    fetch_status(status::SERVER_ERROR)
                }
                Err(e) => return Err(e),
            },
            io::EmptyFolderContents => match empty_folder(req, jmap, operation).await {
                Ok(reply) => reply,
                Err(CommandError::Backend(e)) if !jmap::is_unauthorized(&e) => {
                    tracing::warn!(error = format!("{e:#}"), "emptying a folder failed");
                    empty_status(operation, status::SERVER_ERROR)
                }
                Err(e) => return Err(e),
            },
            other => {
                Element::new(other).child(Element::with_text(io::Status, status::PROTOCOL_ERROR))
            }
        };
        response.push(reply);
    }
    Ok(Element::new(io::ItemOperations)
        .child(Element::with_text(io::Status, status::OK))
        .child(response))
}

async fn fetch(req: &Request<'_>, jmap: &JmapClient, fetch: &Element) -> CommandResult {
    let options = fetch.find(io::Options);
    if let Some(reference) = fetch.child_text(airsyncbase::FileReference) {
        let range = options.and_then(|o| o.child_text(io::Range));
        return fetch_attachment(req, jmap, reference, range).await;
    }
    let body = BodyPreference::choose(&options.map(sync::body_preferences).unwrap_or_default());
    // A Search result is addressed by its LongId, which is `folder:item`.
    if let Some(long_id) = fetch.child_text(sp::LongId) {
        let Some(collection_id) = search::long_id_folder(long_id) else {
            return Ok(fetch_status(status::NOT_FOUND));
        };
        let reply = fetch_item(req, jmap, collection_id, long_id, &body).await?;
        return Ok(reply.child(Element::with_text(sp::LongId, long_id)));
    }
    let (Some(collection_id), Some(server_id)) = (
        fetch.child_text(airsync::CollectionId),
        fetch.child_text(airsync::ServerId),
    ) else {
        return Ok(fetch_status(status::PROTOCOL_ERROR));
    };
    fetch_item(req, jmap, collection_id, server_id, &body).await
}

/// Messages changed per `Email/set`, below common server limits.
const SET_BATCH: usize = 256;

async fn empty_folder(req: &Request<'_>, jmap: &JmapClient, request: &Element) -> CommandResult {
    let Some(collection_id) = request.child_text(airsync::CollectionId) else {
        return Ok(empty_status(request, status::PROTOCOL_ERROR));
    };
    let Some(folder) =
        sync::resolve(req, collection_id)?.filter(|f| f.kind == sync::FolderKind::Mail)
    else {
        return Ok(empty_status(request, status::PROTOCOL_ERROR));
    };
    let jmap = &*folder.client(jmap);
    let ids = jmap
        .query_email_ids(json!({ "inMailbox": folder.jmap_id }))
        .await?;
    let mut destroyed = std::collections::HashSet::new();
    for batch in ids.chunks(SET_BATCH) {
        let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
        let mut update = Map::new();
        let mut destroy = Vec::new();
        for meta in jmap.email_meta(&refs).await? {
            let elsewhere = meta
                .mailbox_ids
                .iter()
                .any(|(m, on)| *on && *m != folder.jmap_id);
            if elsewhere {
                update.insert(
                    meta.id.clone(),
                    json!({ format!("mailboxIds/{}", folder.jmap_id): null }),
                );
            } else {
                destroy.push(meta.id.clone());
            }
            destroyed.insert(meta.id);
        }
        let result = jmap
            .set(
                crate::jmap::EMAIL,
                crate::jmap::SetRequest::default()
                    .update(update)
                    .destroy(destroy),
            )
            .await?;
        if let Some(error) = refs
            .iter()
            .find_map(|id| result.error(id).filter(|e| *e != "notFound"))
        {
            return Err(anyhow::anyhow!("Email/set refused: {error}").into());
        }
    }
    if request
        .find(io::Options)
        .is_some_and(|o| o.find(io::DeleteSubFolders).is_some())
    {
        delete_subfolders(jmap, &folder.jmap_id).await?;
    }
    // The device emptied the folder itself: no Deletes for it later.
    req.store
        .update_collection_syncs(req.user, req.device_id, &[collection_id], |_, state| {
            let snapshots =
                std::iter::once(&mut state.snapshot).chain(state.previous.as_mut().map(|(_, s)| s));
            for json in snapshots {
                let mut snapshot: sync::Snapshot = serde_json::from_str(json)?;
                snapshot.retain(|id, _| !destroyed.contains(id));
                *json = serde_json::to_string(&snapshot)?;
            }
            Ok(())
        })?;
    tracing::info!(
        collection_id,
        messages = ids.len(),
        "device emptied a folder"
    );
    Ok(empty_status(request, status::OK))
}

/// Destroys every mailbox below `parent` with its messages, deepest first.
async fn delete_subfolders(jmap: &JmapClient, parent: &str) -> anyhow::Result<()> {
    let mailboxes = jmap.mailboxes().await?;
    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut current = vec![parent.to_owned()];
    while !current.is_empty() && levels.len() < mailboxes.len() {
        let children: Vec<String> = mailboxes
            .iter()
            .filter(|m| m.parent_id.as_ref().is_some_and(|p| current.contains(p)))
            .map(|m| m.id.clone())
            .collect();
        if !children.is_empty() {
            levels.push(children.clone());
        }
        current = children;
    }
    for level in levels.into_iter().rev() {
        let result = jmap
            .set(
                crate::jmap::MAILBOX,
                crate::jmap::SetRequest::default()
                    .destroy(level.clone())
                    .with("onDestroyRemoveEmails", true),
            )
            .await?;
        if let Some(error) = level.iter().find_map(|id| result.error(id)) {
            anyhow::bail!("Mailbox/set refused: {error}");
        }
    }
    Ok(())
}

fn empty_status(request: &Element, status: &str) -> Element {
    let mut reply =
        Element::new(io::EmptyFolderContents).child(Element::with_text(io::Status, status));
    if let Some(id) = request.child_text(airsync::CollectionId) {
        reply.push(Element::with_text(airsync::CollectionId, id));
    }
    reply
}

fn fetch_status(status: &str) -> Element {
    Element::new(io::Fetch).child(Element::with_text(io::Status, status))
}

/// An attachment: streamed into the response as it downloads, or for a
/// Range request, only that range read (the server is asked for it, and cut
/// from the whole blob if it ignores that).
async fn fetch_attachment(
    req: &Request<'_>,
    jmap: &JmapClient,
    reference: &str,
    range: Option<&str>,
) -> CommandResult {
    let (blob_id, content_type, account) = email::parse_file_reference(reference);
    let jmap = &*match account {
        Some(account) => std::borrow::Cow::Owned(jmap.in_account(account)),
        None => std::borrow::Cow::Borrowed(jmap),
    };
    let requested = match range.map(parse_range) {
        Some(None) => return Ok(fetch_status(status::BAD_RANGE)),
        Some(range) => range,
        None => None,
    };
    let download = match jmap
        .download_range(blob_id, "attachment", content_type, requested)
        .await
    {
        Ok(download) => download,
        Err(e) => {
            tracing::warn!(
                blob_id,
                error = format!("{e:#}"),
                "attachment download failed"
            );
            let status = match e.downcast_ref::<reqwest::Error>().and_then(|e| e.status()) {
                Some(reqwest::StatusCode::NOT_FOUND) => status::INVALID_ATTACHMENT,
                Some(reqwest::StatusCode::RANGE_NOT_SATISFIABLE) => status::BAD_RANGE,
                _ => status::SERVER_ERROR,
            };
            return Ok(fetch_status(status)
                .child(Element::with_text(airsyncbase::FileReference, reference)));
        }
    };

    let mut properties = Element::new(io::Properties)
        .child(Element::with_text(airsyncbase::ContentType, content_type));
    match requested {
        Some((start, end)) => {
            let Some(part) = read_range(download, start, end).await? else {
                return Ok(fetch_status(status::BAD_RANGE));
            };
            properties.push(Element::with_text(
                io::Range,
                format!("{}-{}", part.start, part.end),
            ));
            properties.push(Element::with_text(io::Total, part.total.to_string()));
            properties.push(Element::with_text(io::Data, STANDARD.encode(&part.data)));
        }
        None => properties.push(Element {
            tag: io::Data,
            children: vec![req.downloads.add(download, stream::Encoding::Base64)],
        }),
    }
    Ok(fetch_status(status::OK)
        .child(Element::with_text(airsyncbase::FileReference, reference))
        .child(properties))
}

/// `start-end` (inclusive); `None` when unusable.
fn parse_range(range: &str) -> Option<(u64, u64)> {
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    (start <= end).then_some((start, end))
}

/// The bytes of an attachment a Range fetch returns.
#[derive(Debug, PartialEq, Eq)]
struct RangePart {
    start: u64,
    end: u64,
    total: u64,
    data: Vec<u8>,
}

/// Reads `start..=end` of a download answered with 206 (the range itself)
/// or 200 (the whole blob, read only as far as needed when its length is
/// known). `None` when the range starts past the end.
async fn read_range(
    download: reqwest::Response,
    start: u64,
    end: u64,
) -> anyhow::Result<Option<RangePart>> {
    if download.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = download
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);
        let Some((start, total)) = content_range else {
            anyhow::bail!("206 answer without a usable Content-Range");
        };
        let data = download.bytes().await?.to_vec();
        let Some(last) = (data.len() as u64).checked_sub(1) else {
            return Ok(None);
        };
        return Ok(Some(RangePart {
            start,
            end: start + last,
            total,
            data,
        }));
    }
    let known_total = download.content_length();
    let mut bytes = download.bytes_stream();
    let mut data = Vec::new();
    let mut offset = 0u64;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        let chunk_start = offset;
        offset += chunk.len() as u64;
        if offset > start && chunk_start <= end {
            let from = start.saturating_sub(chunk_start) as usize;
            let to = (end + 1 - chunk_start).min(chunk.len() as u64) as usize;
            data.extend_from_slice(&chunk[from..to]);
        }
        if offset > end && known_total.is_some() {
            break;
        }
    }
    let total = known_total.unwrap_or(offset);
    if start >= total {
        return Ok(None);
    }
    Ok(Some(RangePart {
        start,
        end: end.min(total - 1),
        total,
        data,
    }))
}

/// `bytes 10-19/100` as (10, 100).
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

async fn fetch_item(
    req: &Request<'_>,
    jmap: &JmapClient,
    collection_id: &str,
    server_id: &str,
    body: &BodyPreference,
) -> CommandResult {
    let Some(folder) =
        sync::resolve(req, collection_id)?.filter(|f| f.kind == sync::FolderKind::Mail)
    else {
        return Ok(fetch_status(status::NOT_FOUND));
    };
    let Some(email_id) = folder.email_for(req, server_id)? else {
        return Ok(fetch_status(status::NOT_FOUND));
    };
    let jmap = &*folder.client(jmap);
    let emails = jmap
        .get_emails(
            &[email_id.as_str()],
            body.body_values(),
            body.truncation_size.unwrap_or(0),
        )
        .await?;
    let Some(msg) = emails.first() else {
        return Ok(fetch_status(status::NOT_FOUND));
    };
    let data = email::application_data(msg, body, jmap, Some(&req.downloads)).await?;
    Ok(fetch_status(status::OK)
        .child(Element::with_text(airsync::CollectionId, collection_id))
        .child(Element::with_text(airsync::ServerId, server_id))
        .child(Element::with_text(airsync::Class, "Email"))
        .child(Element {
            tag: io::Properties,
            children: data.children,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("0-9"), Some((0, 9)));
        assert_eq!(parse_range(" 90 - 200 "), Some((90, 200)));
        assert_eq!(parse_range("5-1"), None);
        assert_eq!(parse_range("x"), None);
        assert_eq!(parse_content_range("bytes 10-19/100"), Some((10, 100)));
        assert_eq!(parse_content_range("bytes 10-19/*"), None);
    }

    #[test]
    fn gmail_attachment_fetch_has_a_file_reference() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gmail-android/14.1-itemoperations-fetch-attachment.req.wbxml"
        ))
        .unwrap();
        let root = crate::wbxml::decode(&bytes).unwrap();
        let fetch = root.find(io::Fetch).unwrap();
        assert_eq!(fetch.child_text(io::Store), Some("Mailbox"));
        assert!(fetch.child_text(airsyncbase::FileReference).is_some());
    }
}
