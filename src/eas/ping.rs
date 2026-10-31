//! Ping (MS-ASCMD 2.2.1.13): the long-poll that gives EAS clients push.
//!
//! The request stays open until one of the watched folders has something the
//! device does not have yet (status 2), or the heartbeat interval runs out
//! (status 1). Folders are re-checked whenever the JMAP push stream reports
//! a change, and at least every `FALLBACK_POLL` in case that stream is down.
//! Folders in shared accounts, which the stream does not cover, are checked
//! every `SHARED_POLL_SECS`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::{CommandResult, Request, folder_sync, sync};
use crate::{
    jmap::JmapClient,
    push::{Changes, Kind, PushHub, STORM_DELAY},
    wbxml::{Element, cp::ping},
};

mod status {
    pub const NO_CHANGES: &str = "1";
    pub const CHANGES: &str = "2";
    pub const MISSING_PARAMETERS: &str = "3";
    pub const BAD_HEARTBEAT: &str = "5";
    pub const TOO_MANY_FOLDERS: &str = "6";
    pub const FOLDER_SYNC_REQUIRED: &str = "7";
}

pub const MIN_HEARTBEAT: u64 = 60;
/// Exchange's limit; Gmail grows its heartbeat past 1500 s when Pings last.
pub const MAX_HEARTBEAT: u64 = 3540;
const MAX_FOLDERS: usize = 300;
const FALLBACK_POLL: Duration = Duration::from_secs(120);
/// Answers faster than this count towards ping loop detection.
const QUICK_ANSWER: Duration = Duration::from_secs(1);

/// Sticky per device: a Ping may omit the body or parts of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PingParams {
    heartbeat: u64,
    folders: Vec<String>,
}

impl PingParams {
    /// Combines the request with the stored parameters; `None` when either
    /// the heartbeat or the folders are known from neither.
    fn merge(body: Option<&Element>, stored: Option<PingParams>) -> Option<PingParams> {
        let Some(body) = body else { return stored };
        let heartbeat = body
            .child_text(ping::HeartbeatInterval)
            .and_then(|h| h.trim().parse().ok())
            .or(stored.as_ref().map(|s| s.heartbeat))?;
        let folders: Vec<String> = body
            .find(ping::Folders)
            .map(|folders| {
                folders
                    .find_all(ping::Folder)
                    .filter(|f| {
                        f.child_text(ping::Class)
                            .is_none_or(|c| matches!(c, "Email" | "Contacts" | "Calendar"))
                    })
                    .filter_map(|f| f.child_text(ping::Id).map(str::to_owned))
                    .collect()
            })
            .filter(|f: &Vec<String>| !f.is_empty())
            .or(stored.map(|s| s.folders))?;
        Some(PingParams { heartbeat, folders })
    }
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient, push: &PushHub) -> CommandResult {
    let started = Instant::now();
    let response = respond(req, jmap, push).await?;
    // A device that keeps re-pinging (for example because it ignores an
    // answer) gets its answers held back instead of hammering us and JMAP.
    if started.elapsed() < QUICK_ANSWER
        && push.is_storming(&format!("{}/{}", req.user, req.device_id))
    {
        tracing::warn!(
            user = req.user,
            device = req.device_id,
            "ping loop, delaying the answer"
        );
        // The device's next request or a shutdown ends the delay early.
        let mut next_request = push.device_requests(req.user, req.device_id);
        let mut shutdown = push.shutdown_signal();
        tokio::select! {
            _ = tokio::time::sleep(STORM_DELAY) => {}
            _ = next_request.changed() => {}
            _ = shutdown.changed() => {}
        }
    }
    Ok(response)
}

async fn respond(req: &Request<'_>, jmap: &JmapClient, push: &PushHub) -> CommandResult {
    let body = match &req.body {
        Some(_) => Some(req.body_with_root(ping::Ping)?),
        None => None,
    };
    let stored = req
        .store
        .ping_params(req.user, req.device_id)?
        .and_then(|json| serde_json::from_str(&json).ok());
    let Some(params) = PingParams::merge(body, stored) else {
        return Ok(reply(status::MISSING_PARAMETERS));
    };
    if !(MIN_HEARTBEAT..=MAX_HEARTBEAT).contains(&params.heartbeat) {
        let allowed = params.heartbeat.clamp(MIN_HEARTBEAT, MAX_HEARTBEAT);
        return Ok(reply(status::BAD_HEARTBEAT).child(Element::with_text(
            ping::HeartbeatInterval,
            allowed.to_string(),
        )));
    }
    if params.folders.len() > MAX_FOLDERS {
        return Ok(reply(status::TOO_MANY_FOLDERS).child(Element::with_text(
            ping::MaxFolders,
            MAX_FOLDERS.to_string(),
        )));
    }
    let json = serde_json::to_string(&params).map_err(anyhow::Error::from)?;
    req.store.save_ping_params(req.user, req.device_id, &json)?;

    // Which kind of data each folder holds; an unknown folder means the
    // device's hierarchy is out of date.
    let mut folders = Vec::with_capacity(params.folders.len());
    for id in &params.folders {
        let Some(folder) = sync::resolve(req, id)? else {
            return Ok(reply(status::FOLDER_SYNC_REQUIRED));
        };
        let kind = match folder.kind {
            sync::FolderKind::Mail => Kind::Mail,
            sync::FolderKind::Contacts => Kind::Contacts,
            sync::FolderKind::Calendar => Kind::Calendar,
        };
        folders.push((id, kind, folder.account.is_some()));
    }
    let any_shared = folders.iter().any(|(_, _, shared)| *shared);
    let poll = if any_shared {
        req.caps.shared_poll.min(FALLBACK_POLL)
    } else {
        FALLBACK_POLL
    };

    // Subscribe before the first check so no change slips in between.
    let account = push.account(jmap);
    let mut changes = account.subscribe();
    let mut next_request = push.device_requests(req.user, req.device_id);
    let mut shutdown = push.shutdown_signal();
    let deadline = Instant::now() + Duration::from_secs(params.heartbeat);
    let mut seen = *changes.borrow_and_update();
    // What to look at: everything at first and after a fallback poll, then
    // only the kinds the push stream reports changed.
    let mut changed_since: Option<Changes> = None;
    // A shared-folder poll between fallback polls checks only those folders.
    let mut shared_due = false;
    let mut last_full = Instant::now();
    loop {
        if changed_since.is_none() {
            last_full = Instant::now();
        }
        let check =
            |kind: Kind| changed_since.is_none_or(|before| seen.changed_since(&before, kind));
        if check(Kind::Folders) && folder_sync::hierarchy_changed(req, jmap).await? {
            tracing::debug!("ping: folder hierarchy changed");
            return Ok(reply(status::FOLDER_SYNC_REQUIRED));
        }
        let mut changed = Vec::new();
        for (folder, kind, shared) in &folders {
            if !(check(*kind) || (shared_due && *shared)) {
                continue;
            }
            match sync::has_pending_changes(req, jmap, folder).await? {
                None => return Ok(reply(status::FOLDER_SYNC_REQUIRED)),
                Some(true) => changed.push(*folder),
                Some(false) => {}
            }
        }
        shared_due = false;
        if !changed.is_empty() {
            tracing::debug!(folders = ?changed, "ping: changes");
            let folders = changed
                .into_iter()
                .fold(Element::new(ping::Folders), |list, id| {
                    list.child(Element::with_text(ping::Folder, id.as_str()))
                });
            return Ok(reply(status::CHANGES).child(folders));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(reply(status::NO_CHANGES));
        }
        tokio::select! {
            result = changes.changed() => {
                if result.is_err() {
                    tokio::time::sleep(remaining.min(poll)).await;
                    changed_since = None;
                } else {
                    changed_since = Some(seen);
                    seen = *changes.borrow_and_update();
                }
            }
            // The device sent another request: this Ping is superseded.
            _ = next_request.changed() => return Ok(reply(status::NO_CHANGES)),
            _ = shutdown.changed() => return Ok(reply(status::NO_CHANGES)),
            _ = tokio::time::sleep(remaining.min(poll)) => {
                if last_full.elapsed() + poll / 2 >= FALLBACK_POLL {
                    changed_since = None;
                } else {
                    shared_due = true;
                }
            }
        }
    }
}

fn reply(status: &str) -> Element {
    Element::new(ping::Ping).child(Element::with_text(ping::Status, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(heartbeat: Option<&str>, folders: &[(&str, &str)]) -> Element {
        let mut body = Element::new(ping::Ping);
        if let Some(h) = heartbeat {
            body.push(Element::with_text(ping::HeartbeatInterval, h));
        }
        if !folders.is_empty() {
            body.push(
                folders
                    .iter()
                    .fold(Element::new(ping::Folders), |list, (id, class)| {
                        list.child(
                            Element::new(ping::Folder)
                                .child(Element::with_text(ping::Id, *id))
                                .child(Element::with_text(ping::Class, *class)),
                        )
                    }),
            );
        }
        body
    }

    #[test]
    fn replays_gmail_ping() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gmail-android/14.1-ping.req.wbxml"
        );
        let body = crate::wbxml::decode(&std::fs::read(path).unwrap()).unwrap();
        let params = PingParams::merge(Some(&body), None).unwrap();
        assert_eq!(
            params,
            PingParams {
                heartbeat: 480,
                folders: vec!["23".to_owned(), "44".to_owned()]
            }
        );
        assert!((MIN_HEARTBEAT..=MAX_HEARTBEAT).contains(&params.heartbeat));
    }

    #[test]
    fn merges_with_stored_parameters() {
        let full = request(Some("480"), &[("23", "Email"), ("9", "Tasks")]);
        let params = PingParams::merge(Some(&full), None).unwrap();
        assert_eq!(
            params,
            PingParams {
                heartbeat: 480,
                folders: vec!["23".to_owned()]
            }
        );

        // An empty body or a partial one falls back to what was stored.
        assert_eq!(
            PingParams::merge(None, Some(params.clone())),
            Some(params.clone())
        );
        let only_heartbeat = request(Some("600"), &[]);
        assert_eq!(
            PingParams::merge(Some(&only_heartbeat), Some(params.clone())).unwrap(),
            PingParams {
                heartbeat: 600,
                folders: vec!["23".to_owned()]
            }
        );

        // Nothing stored and nothing sent.
        assert_eq!(PingParams::merge(None, None), None);
        assert_eq!(PingParams::merge(Some(&only_heartbeat), None), None);
    }
}
