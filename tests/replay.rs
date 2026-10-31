//! Replays sanitized sessions: a session captured through the real
//! router is sanitized with `tolk::fixtures`, then replayed against a
//! fresh server with the same data, and every status must match.

mod support;

use std::{collections::HashMap, path::Path, time::Duration};

use support::{Harness, fake_jmap, text};
use tolk::{
    fixtures::{self, Exchange},
    wbxml::{
        Element, Node, Tag,
        cp::{
            airsync, compose_mail as cm, contacts as ct, email as em, folder_hierarchy as fh,
            move_items as mv, ping, provision as pv,
        },
    },
};

/// The same mailbox for recording and replay.
fn seed(h: &Harness) {
    for i in 0..3 {
        h.jmap.add_email("inbox", &format!("Message {i}"), "Body");
    }
}

/// A session touching most commands, recorded as a phone would make it.
async fn record(h: &Harness) {
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let trash = folders.id("Trash");
    let contacts = folders.id("Contacts");
    let initial = device.initial_sync(&inbox).await;
    let first = initial.adds[0]
        .child_text(airsync::ServerId)
        .unwrap()
        .to_owned();
    let second = initial.adds[1]
        .child_text(airsync::ServerId)
        .unwrap()
        .to_owned();

    let read = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, first))
            .child(Element::new(airsync::ApplicationData).child(text(em::Read, "1"))),
    );
    let after_read = device.sync(&inbox, &initial.key, Some(read)).await;
    device
        .send(
            "MoveItems",
            Some(
                Element::new(mv::MoveItems).child(
                    Element::new(mv::Move)
                        .child(text(mv::SrcMsgId, second))
                        .child(text(mv::SrcFldId, inbox.clone()))
                        .child(text(mv::DstFldId, trash)),
                ),
            ),
        )
        .await;
    device.sync(&inbox, &after_read.key, None).await;

    let contacts_initial = device.initial_sync(&contacts).await;
    let add = Element::new(airsync::Commands).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "c1"))
            .child(Element::new(airsync::ApplicationData).child(text(ct::FirstName, "Grace"))),
    );
    device
        .sync(&contacts, &contacts_initial.key, Some(add))
        .await;

    let mime = "From: test@example.com\r\nTo: friend@example.org\r\nSubject: Hi\r\n\r\nHello\r\n";
    device
        .send(
            "SendMail",
            Some(
                Element::new(cm::SendMail)
                    .child(text(cm::ClientId, "m1"))
                    .child(Element::new(cm::SaveInSentItems))
                    .child(Element::with_opaque(cm::Mime, mime.as_bytes())),
            ),
        )
        .await;
    // Sent mail changed nothing in the inbox, but a new message did.
    h.jmap.add_email("inbox", "New", "Body");
    let (status, _) = device.ping(60, &[(inbox.as_str(), "Email")]).await;
    assert_eq!(status, "2");
}

/// Session specific values (policy and sync keys) seen in recorded
/// responses, and what the live server answered in their place.
#[derive(Default)]
struct Keys(HashMap<(Tag, String), String>);

impl Keys {
    fn learn(&mut self, recorded: &Element, live: &Element) {
        for tag in [pv::PolicyKey, airsync::SyncKey, fh::SyncKey] {
            for (r, l) in fixtures::texts(recorded, tag)
                .into_iter()
                .zip(fixtures::texts(live, tag))
            {
                self.0.insert((tag, r), l);
            }
        }
    }

    fn rewrite(&self, element: &Element) -> Element {
        Element {
            tag: element.tag,
            children: element
                .children
                .iter()
                .map(|node| match node {
                    Node::Element(child) => Node::Element(self.rewrite(child)),
                    Node::Text(value) => Node::Text(
                        self.0
                            .get(&(element.tag, value.clone()))
                            .cloned()
                            .unwrap_or_else(|| value.clone()),
                    ),
                    other => other.clone(),
                })
                .collect(),
        }
    }
}

/// Every status in a response, in document order.
fn statuses(root: &Element) -> Vec<String> {
    let mut out = Vec::new();
    for tag in [
        airsync::Status,
        fh::Status,
        pv::Status,
        mv::Status,
        cm::Status,
        ping::Status,
    ] {
        out.extend(fixtures::texts(root, tag));
    }
    out
}

async fn replay(session: &[Exchange]) {
    let h = Harness::start().await;
    seed(&h);
    let mut device = h.device("d1");
    let mut keys = Keys::default();
    for (n, exchange) in session.iter().enumerate() {
        if exchange.cmd == "Ping" {
            // The recording's Ping found a message that arrived meanwhile.
            h.jmap.add_email("inbox", "New", "Body");
        }
        let request = exchange.request.as_ref().map(|r| keys.rewrite(r));
        // Policy keys are random per session: send the live one.
        device.policy_key = match exchange.sent_policy_key {
            true => last_policy_key(&session[..n], &keys).unwrap_or_else(|| "0".to_owned()),
            false => String::new(),
        };
        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            device.send_full(
                &exchange.cmd,
                &exchange.params,
                request,
                fake_jmap::PASSWORD,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("exchange {n} ({}) hangs", exchange.cmd));
        assert_eq!(
            reply.status.as_u16(),
            exchange.response_status,
            "HTTP status of exchange {n} ({})",
            exchange.cmd
        );
        match (&exchange.response, &reply.body) {
            (Some(recorded), Some(live)) => {
                assert_eq!(
                    statuses(live),
                    statuses(recorded),
                    "statuses of exchange {n} ({})",
                    exchange.cmd
                );
                keys.learn(recorded, live);
            }
            (None, None) => {}
            (recorded, live) => panic!(
                "exchange {n} ({}): recorded body {}, live body {}",
                exchange.cmd,
                recorded.is_some(),
                live.is_some()
            ),
        }
    }
}

/// The live final policy key: from the last Provision exchange so far.
fn last_policy_key(before: &[Exchange], keys: &Keys) -> Option<String> {
    let recorded = before
        .iter()
        .rev()
        .find(|e| e.cmd == "Provision")
        .and_then(|e| e.response.as_ref())
        .and_then(|r| fixtures::texts(r, pv::PolicyKey).pop())?;
    keys.0.get(&(pv::PolicyKey, recorded)).cloned()
}

fn assert_sanitized(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "xml") {
            let xml = std::fs::read_to_string(&path).unwrap();
            for secret in [
                "Message 0",
                "Grace",
                "friend@example.org",
                "Hello",
                "sender@example.org",
            ] {
                assert!(!xml.contains(secret), "{secret} in {}", path.display());
            }
        }
    }
}

#[tokio::test]
async fn sanitized_session_replays_with_the_same_statuses() {
    let traffic = tempfile::tempdir().unwrap();
    let recording = Harness::start_capturing(Some(traffic.path())).await;
    seed(&recording);
    record(&recording).await;
    // Captures are written after the response is sent.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let captured = fixtures::read_capture(traffic.path(), "d1").unwrap();
    let commands: Vec<&str> = captured.iter().map(|e| e.cmd.as_str()).collect();
    for cmd in [
        "Provision",
        "FolderSync",
        "Sync",
        "MoveItems",
        "SendMail",
        "Ping",
    ] {
        assert!(commands.contains(&cmd), "{cmd} captured: {commands:?}");
    }
    let fixture = tempfile::tempdir().unwrap();
    fixtures::write_session(fixture.path(), &captured).unwrap();
    assert_sanitized(fixture.path());

    let session = fixtures::read_session(fixture.path()).unwrap();
    assert_eq!(session.len(), captured.len());
    replay(&session).await;
}

/// A real Gmail 16.1 session (test account, sanitized): every request it
/// sent must be understood. Its data cannot be rebuilt, so items it names
/// may be missing; that is fine, protocol and server errors are not.
#[tokio::test]
async fn real_gmail_session_is_understood() {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sessions/gmail-android-16.1");
    let session = fixtures::read_session(&dir).unwrap();
    assert!(session.len() > 20);
    let h = Harness::start().await;
    seed(&h);
    let mut device = h.device("gmail");
    let mut keys = Keys::default();
    let mut folders = FolderMap::default();
    for (n, exchange) in session.iter().enumerate() {
        // Pings wait for changes that will not come.
        if exchange.cmd == "Ping" {
            continue;
        }
        device.version = exchange
            .protocol_version
            .clone()
            .unwrap_or_else(|| "14.1".to_owned());
        device.policy_key = last_policy_key(&session[..n], &keys).unwrap_or_else(|| "0".to_owned());
        let request = exchange
            .request
            .as_ref()
            .map(|r| folders.rewrite(&keys.rewrite(r)));
        let reply = device
            .send_full(
                &exchange.cmd,
                &exchange.params,
                request,
                fake_jmap::PASSWORD,
            )
            .await;
        assert!(
            reply.status.is_success(),
            "exchange {n} ({}): HTTP {}",
            exchange.cmd,
            reply.status
        );
        let Some(live) = &reply.body else { continue };
        let bad: &[&str] = match exchange.cmd.as_str() {
            // Protocol error, server error.
            "Sync" => &["4", "5"],
            // Server error, malformed request.
            "FolderSync" => &["6", "10"],
            "Provision" => &["2"],
            _ => &[],
        };
        let statuses = statuses(live);
        assert!(
            !statuses.iter().any(|s| bad.contains(&s.as_str())),
            "exchange {n} ({}): statuses {statuses:?}",
            exchange.cmd
        );
        if let Some(recorded) = &exchange.response {
            keys.learn(recorded, live);
            folders.learn(recorded, live);
        }
    }
}

/// Recorded folder ids to live ones, matched on the folder type of the
/// default folders (Inbox, Drafts, Trash, Sent, Calendar, Contacts).
#[derive(Default)]
struct FolderMap(HashMap<String, String>);

impl FolderMap {
    fn learn(&mut self, recorded: &Element, live: &Element) {
        let by_type = |root: &Element| -> HashMap<String, String> {
            root.find(fh::Changes)
                .into_iter()
                .flat_map(|c| c.find_all(fh::Add))
                .filter_map(|add| {
                    let kind = add.child_text(fh::Type)?;
                    let id = add.child_text(fh::ServerId)?;
                    matches!(kind, "2" | "3" | "4" | "5" | "8" | "9")
                        .then(|| (kind.to_owned(), id.to_owned()))
                })
                .collect()
        };
        let live = by_type(live);
        for (kind, recorded_id) in by_type(recorded) {
            if let Some(live_id) = live.get(&kind) {
                self.0.insert(recorded_id, live_id.clone());
            }
        }
    }

    /// Rewrites CollectionIds and the folder part of `folder:item` ServerIds.
    fn rewrite(&self, element: &Element) -> Element {
        Element {
            tag: element.tag,
            children: element
                .children
                .iter()
                .map(|node| match node {
                    Node::Element(child) => Node::Element(self.rewrite(child)),
                    Node::Text(value) if element.tag == airsync::CollectionId => {
                        Node::Text(self.0.get(value).cloned().unwrap_or_else(|| value.clone()))
                    }
                    Node::Text(value) if element.tag == airsync::ServerId => {
                        let mapped = value.split_once(':').and_then(|(folder, item)| {
                            Some(format!("{}:{item}", self.0.get(folder)?))
                        });
                        Node::Text(mapped.unwrap_or_else(|| value.clone()))
                    }
                    other => other.clone(),
                })
                .collect(),
        }
    }
}
