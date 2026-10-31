//! Turns captured EAS sessions (see [`crate::traffic`]) into shareable test
//! fixtures, and reads them back for replay.
//!
//! Sanitizing keeps the protocol skeleton: element structure and the values
//! of an allowlist of structural tags (keys, ids, statuses, options, times).
//! Every other text or blob, which is where names, addresses, subjects,
//! bodies and attachments live, becomes `redacted`.
//!
//! A fixture session is a directory of numbered exchanges:
//! `NNN-<Cmd>.json` (command, query parameters, response status) plus
//! `NNN-<Cmd>.req.wbxml` and `NNN-<Cmd>.resp.wbxml` when there were bodies,
//! each with an `.xml` rendering for review.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::wbxml::{
    self, Element, Node, Tag,
    cp::{
        airsync, airsyncbase, calendar, compose_mail, email, folder_hierarchy, item_estimate,
        item_operations, meeting_response, move_items, ping, provision, search, settings,
    },
};

const REDACTED: &str = "redacted";

/// Stands in for a sent message, so a replayed SendMail still parses.
const SYNTHETIC_MIME: &str = "From: user@example.com\r\nTo: someone@example.com\r\n\
                              Subject: redacted\r\n\r\nredacted\r\n";

/// Tags whose text says nothing about the user's data.
fn kept_tags() -> &'static HashSet<Tag> {
    static KEPT: OnceLock<HashSet<Tag>> = OnceLock::new();
    KEPT.get_or_init(|| {
        HashSet::from([
            airsync::SyncKey,
            airsync::ClientId,
            airsync::ServerId,
            airsync::Status,
            airsync::Class,
            airsync::CollectionId,
            airsync::GetChanges,
            airsync::WindowSize,
            airsync::FilterType,
            airsync::Conflict,
            airsync::DeletesAsMoves,
            airsync::MIMESupport,
            airsync::MIMETruncation,
            airsync::ConversationMode,
            airsync::MaxItems,
            airsync::HeartbeatInterval,
            airsyncbase::Type,
            airsyncbase::TruncationSize,
            airsyncbase::AllOrNone,
            airsyncbase::EstimatedDataSize,
            airsyncbase::Truncated,
            airsyncbase::Method,
            airsyncbase::IsInline,
            airsyncbase::NativeBodyType,
            airsyncbase::Preview,
            email::Read,
            email::Importance,
            email::DateReceived,
            email::MessageClass,
            email::InternetCPID,
            email::ContentClass,
            email::FlagType,
            email::Status,
            calendar::AllDayEvent,
            calendar::BusyStatus,
            calendar::DtStamp,
            calendar::StartTime,
            calendar::EndTime,
            calendar::TimeZone,
            calendar::MeetingStatus,
            calendar::Sensitivity,
            calendar::Reminder,
            calendar::AttendeeStatus,
            calendar::AttendeeType,
            calendar::ResponseType,
            calendar::ResponseRequested,
            calendar::Type,
            calendar::Interval,
            calendar::DayOfWeek,
            calendar::DayOfMonth,
            calendar::WeekOfMonth,
            calendar::MonthOfYear,
            calendar::Occurrences,
            calendar::Until,
            calendar::ExceptionStartTime,
            calendar::Deleted,
            folder_hierarchy::SyncKey,
            folder_hierarchy::ServerId,
            folder_hierarchy::ParentId,
            folder_hierarchy::Type,
            folder_hierarchy::Status,
            folder_hierarchy::Count,
            item_estimate::CollectionId,
            item_estimate::Status,
            item_estimate::Estimate,
            item_estimate::Class,
            ping::Status,
            ping::HeartbeatInterval,
            ping::Id,
            ping::Class,
            ping::Folder,
            ping::MaxFolders,
            provision::PolicyType,
            provision::PolicyKey,
            provision::Status,
            move_items::SrcMsgId,
            move_items::SrcFldId,
            move_items::DstFldId,
            move_items::DstMsgId,
            move_items::Status,
            compose_mail::ClientId,
            compose_mail::SaveInSentItems,
            compose_mail::FolderId,
            compose_mail::ItemId,
            compose_mail::Status,
            item_operations::Store,
            item_operations::Status,
            item_operations::Range,
            item_operations::Total,
            meeting_response::UserResponse,
            meeting_response::CollectionId,
            meeting_response::RequestId,
            meeting_response::Status,
            search::Name,
            search::Range,
            search::Status,
            search::Total,
            settings::BodyType,
            settings::Status,
        ])
    })
}

/// A copy of `root` with every text and blob outside the allowlist replaced
/// (a sent message by a synthetic one).
pub fn sanitize(root: &Element) -> Element {
    if root.tag == compose_mail::Mime {
        return Element::with_opaque(compose_mail::Mime, SYNTHETIC_MIME.as_bytes().to_vec());
    }
    let keep = kept_tags().contains(&root.tag);
    Element {
        tag: root.tag,
        children: root
            .children
            .iter()
            .map(|node| match node {
                Node::Element(child) => Node::Element(sanitize(child)),
                Node::Text(text) if keep => Node::Text(text.clone()),
                Node::Text(_) => Node::Text(REDACTED.to_owned()),
                Node::Opaque(_) => Node::Opaque(REDACTED.as_bytes().to_vec()),
                Node::Stream(n) => Node::Stream(*n),
            })
            .collect(),
    }
}

/// One request/response pair of a fixture session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exchange {
    pub cmd: String,
    /// Query parameters other than `Cmd`, `User` and `DeviceId`.
    #[serde(default)]
    pub params: Vec<(String, String)>,
    /// Whether the device sent a policy key (its value is session specific).
    #[serde(default)]
    pub sent_policy_key: bool,
    /// `MS-ASProtocolVersion`, e.g. `16.1`.
    #[serde(default)]
    pub protocol_version: Option<String>,
    pub response_status: u16,
    #[serde(skip)]
    pub request: Option<Element>,
    #[serde(skip)]
    pub response: Option<Element>,
}

/// Reads one device's exchanges from a traffic capture directory, oldest
/// first, sanitized.
pub fn read_capture(dir: &Path, device_id: &str) -> anyhow::Result<Vec<Exchange>> {
    let mut metas: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    // Names start with the time in milliseconds and a sequence number.
    metas.sort();
    let mut exchanges = Vec::new();
    for meta_path in metas {
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path)?)
            .with_context(|| format!("read {}", meta_path.display()))?;
        if meta["query"]["device_id"] != device_id {
            continue;
        }
        let stem = meta_path.with_extension("");
        let body = |suffix: &str| -> anyhow::Result<Option<Element>> {
            let path = PathBuf::from(format!("{}.{suffix}.wbxml", stem.display()));
            match std::fs::read(&path) {
                Ok(bytes) => Ok(wbxml::decode(&bytes).ok().map(|root| sanitize(&root))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        exchanges.push(Exchange {
            cmd: meta["query"]["cmd"].as_str().unwrap_or_default().to_owned(),
            params: serde_json::from_value(meta["query"]["params"].clone()).unwrap_or_default(),
            sent_policy_key: meta["request_headers"]["x-ms-policykey"].is_string(),
            protocol_version: meta["request_headers"]["ms-asprotocolversion"]
                .as_str()
                .map(str::to_owned),
            response_status: meta["response_status"].as_u64().unwrap_or(0) as u16,
            request: body("req")?,
            response: body("resp")?,
        });
    }
    Ok(exchanges)
}

/// Writes a fixture session (see the module docs).
pub fn write_session(dir: &Path, exchanges: &[Exchange]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (n, exchange) in exchanges.iter().enumerate() {
        let stem = dir.join(format!("{n:03}-{}", exchange.cmd));
        std::fs::write(
            stem.with_extension("json"),
            serde_json::to_vec_pretty(exchange)?,
        )?;
        for (suffix, body) in [("req", &exchange.request), ("resp", &exchange.response)] {
            if let Some(body) = body {
                let base = format!("{}.{suffix}", stem.display());
                std::fs::write(format!("{base}.wbxml"), wbxml::encode(body))?;
                std::fs::write(format!("{base}.xml"), wbxml::to_xml(body))?;
            }
        }
    }
    Ok(())
}

/// Reads a fixture session written by [`write_session`].
pub fn read_session(dir: &Path) -> anyhow::Result<Vec<Exchange>> {
    let mut metas: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    metas.sort();
    metas
        .into_iter()
        .map(|meta_path| {
            let mut exchange: Exchange = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
            let stem = meta_path.with_extension("");
            let body = |suffix: &str| -> anyhow::Result<Option<Element>> {
                match std::fs::read(format!("{}.{suffix}.wbxml", stem.display())) {
                    Ok(bytes) => Ok(Some(wbxml::decode(&bytes)?)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                }
            };
            exchange.request = body("req")?;
            exchange.response = body("resp")?;
            Ok(exchange)
        })
        .collect()
}

/// The texts of every element with tag `tag`, in document order.
pub fn texts(root: &Element, tag: Tag) -> Vec<String> {
    let mut out = Vec::new();
    collect(root, tag, &mut out);
    out
}

fn collect(root: &Element, tag: Tag, out: &mut Vec<String>) {
    if root.tag == tag
        && let Some(text) = root.text()
    {
        out.push(text.to_owned());
    }
    for child in root.elements() {
        collect(child, tag, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::cp::contacts;

    #[test]
    fn keeps_structure_and_drops_personal_data() {
        let root = Element::new(airsync::Sync).child(
            Element::new(airsync::Collections).child(
                Element::new(airsync::Collection)
                    .child(Element::with_text(airsync::SyncKey, "4"))
                    .child(Element::with_text(airsync::CollectionId, "23"))
                    .child(
                        Element::new(airsync::Commands).child(
                            Element::new(airsync::Add)
                                .child(Element::with_text(airsync::ServerId, "23:5"))
                                .child(
                                    Element::new(airsync::ApplicationData)
                                        .child(Element::with_text(email::Subject, "Payslip"))
                                        .child(Element::with_text(email::From, "boss@example.com"))
                                        .child(Element::with_text(email::Read, "1"))
                                        .child(Element::with_text(contacts::FirstName, "Ann"))
                                        .child(Element::with_opaque(
                                            airsyncbase::Data,
                                            b"secret".to_vec(),
                                        )),
                                ),
                        ),
                    ),
            ),
        );
        let clean = wbxml::to_xml(&sanitize(&root));
        for kept in [
            "<SyncKey>4</SyncKey>",
            "<ServerId>23:5</ServerId>",
            "Read>1</",
        ] {
            assert!(clean.contains(kept), "{kept} in {clean}");
        }
        for gone in ["Payslip", "boss@", "Ann", "secret"] {
            assert!(!clean.contains(gone), "{gone} in {clean}");
        }
    }
}
