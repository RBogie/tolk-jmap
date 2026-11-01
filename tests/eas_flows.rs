//! End-to-end EAS flows against the fake JMAP server.

mod support;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use support::{Harness, text};
use tolk::wbxml::{
    Element,
    cp::{
        airsync, calendar as cal, compose_mail as cm, contacts as ct, email as em,
        folder_hierarchy as fh, move_items as mv,
    },
};

#[tokio::test]
async fn data_commands_need_provisioning() {
    let h = Harness::start().await;
    let mut device = h.device("d1");
    let reply = device
        .send(
            "FolderSync",
            Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, "0"))),
        )
        .await;
    assert_eq!(reply.root().child_text(fh::Status), Some("142"));

    device.provision().await;
    let folders = device.folder_sync().await;
    for name in ["Inbox", "Sent", "Drafts", "Trash", "Contacts", "Calendar"] {
        folders.id(name);
    }
}

#[tokio::test]
async fn wrong_password_is_rejected() {
    let h = Harness::start().await;
    let device = h.device("d1");
    let reply = device.send_as("FolderSync", None, "wrong").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_password_mid_session_answers_401_and_recovers() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;

    h.jmap.fail_requests(&[StatusCode::UNAUTHORIZED]);
    let reply = device
        .send(
            "Sync",
            Some(
                Element::new(airsync::Sync).child(
                    Element::new(airsync::Collections).child(
                        Element::new(airsync::Collection)
                            .child(text(airsync::SyncKey, primed.key.clone()))
                            .child(text(airsync::CollectionId, inbox.clone())),
                    ),
                ),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // The session was evicted, so the next request verifies again and works.
    let result = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(result.status, "1");
}

#[tokio::test]
async fn mail_sync_initial_and_incremental() {
    let h = Harness::start().await;
    let ids: Vec<String> = (0..3)
        .map(|i| h.jmap.add_email("inbox", &format!("Message {i}"), "Hello"))
        .collect();
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");

    let first = device.initial_sync(&inbox).await;
    assert_eq!(first.adds.len(), 3);
    let subjects: Vec<&str> = first
        .adds
        .iter()
        .filter_map(|a| a.find(airsync::ApplicationData)?.child_text(em::Subject))
        .collect();
    assert!(subjects.contains(&"Message 0"));

    // Another client: one new message, one marked read, one deleted.
    let new = h.jmap.add_email("inbox", "Message 3", "New");
    h.jmap.change(|m| {
        m.patch(
            "Email",
            &ids[1],
            json!({ "keywords/$seen": true }).as_object().unwrap(),
        );
        m.remove("Email", &ids[2]);
    });
    let next = device.sync(&inbox, &first.key, None).await;
    assert_eq!(next.status, "1");
    assert_eq!(next.adds.len(), 1, "new message");
    assert_eq!(next.changes.len(), 1, "read flag");
    assert_eq!(next.deletes.len(), 1, "deleted message");
    let read = next.changes[0]
        .find(airsync::ApplicationData)
        .and_then(|d| d.child_text(em::Read));
    assert_eq!(read, Some("1"));
    assert_ne!(next.add_ids()[0], next.deletes[0]);
    let _ = new;

    // Nothing more to report.
    let idle = device.sync(&inbox, &next.key, None).await;
    assert!(idle.adds.is_empty() && idle.changes.is_empty() && idle.deletes.is_empty());
}

#[tokio::test]
async fn retry_with_previous_sync_key_repeats_the_batch() {
    let h = Harness::start().await;
    for i in 0..2 {
        h.jmap.add_email("inbox", &format!("Message {i}"), "Hello");
    }
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;

    let first = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(first.adds.len(), 2);
    // The response was lost: the device repeats the request with the same key.
    let retry = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(retry.status, "1");
    let mut a = first.add_ids();
    let mut b = retry.add_ids();
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same items under the same ids");

    // Continuing from the retried key does not add them again.
    let after = device.sync(&inbox, &retry.key, None).await;
    assert!(after.adds.is_empty());
    let store = &h.state.store;
    assert_eq!(
        store
            .sync_repeats(support::fake_jmap::ACCOUNT, "d1", &inbox)
            .unwrap(),
        None
    );
    assert!(
        store
            .quarantine_list(support::fake_jmap::ACCOUNT, "d1")
            .unwrap()
            .is_empty()
    );
}

fn subjects(result: &support::SyncResult) -> Vec<String> {
    result
        .adds
        .iter()
        .filter_map(|a| {
            a.find(airsync::ApplicationData)?
                .child_text(em::Subject)
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test]
async fn a_message_the_device_keeps_failing_on_is_quarantined() {
    let h = Harness::start().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(h.jmap.add_email("inbox", &format!("Message {i}"), "Hello"));
    }
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;

    // The device fails on something in the batch and sends the key again.
    let first = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(first.adds.len(), 3);
    let lost = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(lost.adds.len(), 3, "one repeat is answered as before");
    let probe = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(subjects(&probe), ["Message 2"], "one item at a time");
    assert!(probe.more);
    // It fails on that one too, again and again: quarantined, and the rest comes.
    let again = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(subjects(&again), ["Message 2"], "a lost probe is repeated");
    let rest = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(rest.status, "1");
    let mut got = subjects(&rest);
    got.sort();
    assert_eq!(got, ["Message 0", "Message 1"]);
    assert!(!rest.more);

    let quarantined = h
        .state
        .store
        .quarantine_list(support::fake_jmap::ACCOUNT, "d1")
        .unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].object_id, ids[2]);
    assert_eq!(quarantined[0].collection_id, inbox);

    // It stays away, also when it changes.
    let after = device.sync(&inbox, &rest.key, None).await;
    assert!(after.adds.is_empty() && after.changes.is_empty());
    h.jmap.change(|m| {
        m.patch(
            "Email",
            &ids[2],
            json!({ "keywords/$seen": true }).as_object().unwrap(),
        )
    });
    let after = device.sync(&inbox, &after.key, None).await;
    assert!(after.changes.is_empty(), "{:?}", after.changes);
}

#[tokio::test]
async fn probing_covers_only_the_rejected_response() {
    let h = Harness::start().await;
    for i in 0..5 {
        h.jmap.add_email("inbox", &format!("Message {i}"), "Hello");
    }
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;
    // Responses of two get lost three times (say, timeouts).
    for _ in 0..2 {
        let batch = device.sync_window(&inbox, &primed.key, None, 2).await;
        assert_eq!(batch.adds.len(), 2);
    }
    let probe = device.sync_window(&inbox, &primed.key, None, 2).await;
    assert_eq!(subjects(&probe), ["Message 4"]);
    let second = device.sync_window(&inbox, &probe.key, None, 2).await;
    assert_eq!(subjects(&second), ["Message 3"]);
    // Both suspects went through: normal windows again, nothing quarantined.
    let rest = device.sync_window(&inbox, &second.key, None, 2).await;
    assert_eq!(subjects(&rest), ["Message 2", "Message 1"]);
    assert!(
        h.state
            .store
            .quarantine_list(support::fake_jmap::ACCOUNT, "d1")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn probing_finds_the_broken_item_after_good_ones() {
    let h = Harness::start().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(h.jmap.add_email("inbox", &format!("Message {i}"), "Hello"));
    }
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;
    for _ in 0..2 {
        device.sync(&inbox, &primed.key, None).await;
    }
    let probe = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(subjects(&probe), ["Message 2"]);
    // The device takes that one, then fails on the next.
    let next = device.sync(&inbox, &probe.key, None).await;
    assert_eq!(subjects(&next), ["Message 1"]);
    let again = device.sync(&inbox, &probe.key, None).await;
    assert_eq!(subjects(&again), ["Message 1"], "a lost probe is repeated");
    let rest = device.sync(&inbox, &probe.key, None).await;
    assert_eq!(subjects(&rest), ["Message 0"]);
    let quarantined = h
        .state
        .store
        .quarantine_list(support::fake_jmap::ACCOUNT, "d1")
        .unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].object_id, ids[1]);
    // Caught up: back to normal windows.
    let done = device.sync(&inbox, &rest.key, None).await;
    assert!(done.adds.is_empty());
    assert_eq!(
        h.state
            .store
            .sync_repeats(support::fake_jmap::ACCOUNT, "d1", &inbox)
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn windowed_sync_reports_more_available() {
    let h = Harness::start().await;
    for i in 0..5 {
        h.jmap.add_email("inbox", &format!("Message {i}"), "Hello");
    }
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;
    let page1 = device.sync_window(&inbox, &primed.key, None, 2).await;
    assert_eq!(page1.adds.len(), 2);
    assert!(page1.more);
    let page2 = device.sync_window(&inbox, &page1.key, None, 2).await;
    let page3 = device.sync_window(&inbox, &page2.key, None, 2).await;
    assert_eq!(page2.adds.len() + page3.adds.len(), 3);
    assert!(!page3.more);
    let mut all: Vec<String> = [page1, page2, page3]
        .iter()
        .flat_map(|p| p.add_ids())
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn client_flag_and_delete_reach_jmap() {
    let h = Harness::start().await;
    let a = h.jmap.add_email("inbox", "Read me", "x");
    let b = h.jmap.add_email("inbox", "Delete me", "y");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    let server_id = |subject: &str| {
        initial
            .adds
            .iter()
            .find(|add| {
                add.find(airsync::ApplicationData)
                    .and_then(|d| d.child_text(em::Subject))
                    == Some(subject)
            })
            .and_then(|add| add.child_text(airsync::ServerId))
            .unwrap()
            .to_owned()
    };
    let commands = Element::new(airsync::Commands)
        .child(
            Element::new(airsync::Change)
                .child(text(airsync::ServerId, server_id("Read me")))
                .child(Element::new(airsync::ApplicationData).child(text(em::Read, "1"))),
        )
        .child(
            Element::new(airsync::Delete).child(text(airsync::ServerId, server_id("Delete me"))),
        );
    let result = device.sync(&inbox, &initial.key, Some(commands)).await;
    assert_eq!(result.status, "1");

    h.jmap.read(|m| {
        assert_eq!(m.get("Email", &a).unwrap()["keywords"]["$seen"], true);
        // Deletes move to Trash by default (DeletesAsMoves).
        let deleted = m.get("Email", &b).unwrap();
        assert_eq!(deleted["mailboxIds"], json!({ "trash": true }));
    });

    // The device's own changes are not echoed back.
    let echo = device.sync(&inbox, &result.key, None).await;
    assert!(echo.changes.is_empty(), "no echo: {:?}", echo.changes);
    assert!(echo.deletes.is_empty());
}

#[tokio::test]
async fn ping_wakes_on_new_mail() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    assert!(initial.adds.is_empty());

    let jmap = &h.jmap;
    let watched = [(inbox.as_str(), "Email")];
    let (ping, _) = tokio::join!(device.ping(60, &watched), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        jmap.add_email("inbox", "Wake up", "z");
    });
    assert_eq!(ping.0, "2", "changes found");
    assert_eq!(ping.1, vec![inbox.clone()]);

    let result = device.sync(&inbox, &initial.key, None).await;
    assert_eq!(result.adds.len(), 1);
}

#[tokio::test]
async fn ping_reports_changes_made_before_it_started() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    h.jmap.add_email("inbox", "Already there", "z");
    let (status, changed) = tokio::time::timeout(
        Duration::from_secs(10),
        device.ping(60, &[(&inbox, "Email")]),
    )
    .await
    .expect("Ping answers at once");
    assert_eq!(status, "2");
    assert_eq!(changed, vec![inbox.clone()]);
    let _ = initial;
}

#[tokio::test]
async fn move_items_changes_the_mailbox() {
    let h = Harness::start().await;
    let id = h.jmap.add_email("inbox", "Move me", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let trash = folders.id("Trash");
    let initial = device.initial_sync(&inbox).await;
    let server_id = initial.add_ids()[0].clone();

    let reply = device
        .send(
            "MoveItems",
            Some(
                Element::new(mv::MoveItems).child(
                    Element::new(mv::Move)
                        .child(text(mv::SrcMsgId, server_id.clone()))
                        .child(text(mv::SrcFldId, inbox.clone()))
                        .child(text(mv::DstFldId, trash.clone())),
                ),
            ),
        )
        .await;
    let response = reply.root().find(mv::Response).unwrap();
    assert_eq!(response.child_text(mv::Status), Some("3"), "success");
    h.jmap.read(|m| {
        assert_eq!(
            m.get("Email", &id).unwrap()["mailboxIds"],
            json!({ "trash": true })
        );
    });

    // The source folder reports it gone, the destination has it.
    let source = device.sync(&inbox, &initial.key, None).await;
    assert!(source.adds.is_empty());
    let target = device.initial_sync(&trash).await;
    assert_eq!(target.adds.len(), 1);
}

#[tokio::test]
async fn send_mail_submits_once() {
    let h = Harness::start().await;
    let (device, _) = h.ready_device("d1").await;
    let mime = "From: test@example.com\r\nTo: friend@example.org\r\nSubject: Hi\r\n\r\nHello\r\n";
    let send = || {
        Element::new(cm::SendMail)
            .child(text(cm::ClientId, "c-1"))
            .child(Element::new(cm::SaveInSentItems))
            .child(Element::with_opaque(cm::Mime, mime.as_bytes()))
    };
    let reply = device.send("SendMail", Some(send())).await;
    assert!(reply.status.is_success());
    assert!(reply.body.is_none(), "success has no body");

    h.jmap.read(|m| {
        assert_eq!(m.submissions.len(), 1);
        assert_eq!(
            m.submissions[0]["envelope"]["rcptTo"],
            json!([{ "email": "friend@example.org" }])
        );
        let sent: Vec<_> = m
            .all("Email")
            .into_iter()
            .filter(|e| e["mailboxIds"]["sent"] == true)
            .collect();
        assert_eq!(sent.len(), 1, "copy in Sent");
    });

    // A retry with the same ClientId is not sent again.
    let retry = device.send("SendMail", Some(send())).await;
    assert_eq!(retry.root().child_text(cm::Status), Some("118"));
    h.jmap.read(|m| assert_eq!(m.submissions.len(), 1));
}

#[tokio::test]
async fn contacts_sync_both_ways() {
    let h = Harness::start().await;
    h.jmap.change(|m| {
        m.insert(
            "ContactCard",
            json!({
                "@type": "Card",
                "version": "1.0",
                "uid": "urn:uuid:c1",
                "addressBookIds": { "ab1": true },
                "name": { "components": [
                    { "kind": "given", "value": "Ada" },
                    { "kind": "surname", "value": "Lovelace" },
                ] },
                "emails": { "e1": { "address": "ada@example.org" } },
            }),
        )
    });
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    let initial = device.initial_sync(&contacts).await;
    assert_eq!(initial.adds.len(), 1);
    let data = initial.adds[0].find(airsync::ApplicationData).unwrap();
    assert_eq!(data.child_text(ct::FirstName), Some("Ada"));
    assert_eq!(data.child_text(ct::LastName), Some("Lovelace"));
    assert_eq!(data.child_text(ct::Email1Address), Some("ada@example.org"));

    // A contact created on the device.
    let commands = Element::new(airsync::Commands).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "new-1"))
            .child(
                Element::new(airsync::ApplicationData)
                    .child(text(ct::FirstName, "Grace"))
                    .child(text(ct::LastName, "Hopper")),
            ),
    );
    let result = device.sync(&contacts, &initial.key, Some(commands)).await;
    assert_eq!(result.status, "1");
    let (kind, client_id, status, server_id) = &result.responses[0];
    assert_eq!(
        (kind.as_str(), client_id.as_str(), status.as_str()),
        ("Add", "new-1", "1")
    );
    assert!(!server_id.is_empty());
    h.jmap.read(|m| {
        let cards = m.all("ContactCard");
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().any(|c| c.to_string().contains("Hopper")));
    });

    // Not echoed back as an Add.
    let echo = device.sync(&contacts, &result.key, None).await;
    assert!(echo.adds.is_empty(), "no echo");
}

#[tokio::test]
async fn calendar_events_sync() {
    let h = Harness::start().await;
    h.jmap.change(|m| {
        m.insert(
            "CalendarEvent",
            json!({
                "@type": "Event",
                "uid": "ev1@example.org",
                "calendarIds": { "cal1": true },
                "title": "Standup",
                "start": "2026-11-02T09:00:00",
                "timeZone": "Europe/Amsterdam",
                "duration": "PT15M",
                "updated": "2026-10-30T10:00:00Z",
            }),
        )
    });
    let (device, folders) = h.ready_device("d1").await;
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&calendar).await;
    assert_eq!(initial.adds.len(), 1);
    let data = initial.adds[0].find(airsync::ApplicationData).unwrap();
    assert_eq!(data.child_text(cal::Subject), Some("Standup"));
    // 09:00 in Amsterdam is 08:00 UTC in November.
    assert_eq!(data.child_text(cal::StartTime), Some("20261102T080000Z"));

    h.jmap.change(|m| {
        let id = m.all("CalendarEvent")[0]["id"].as_str().unwrap().to_owned();
        m.patch(
            "CalendarEvent",
            &id,
            json!({ "title": "Daily standup" }).as_object().unwrap(),
        );
    });
    let next = device.sync(&calendar, &initial.key, None).await;
    assert_eq!(next.changes.len(), 1);
    let data = next.changes[0].find(airsync::ApplicationData).unwrap();
    assert_eq!(data.child_text(cal::Subject), Some("Daily standup"));
}

#[tokio::test]
async fn lost_change_history_resyncs_without_losing_items() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Old", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    h.jmap.add_email("inbox", "New", "y");
    h.jmap.change(|m| m.forget_changes("Email"));

    let next = device.sync(&inbox, &initial.key, None).await;
    assert_eq!(next.status, "1");
    assert_eq!(next.adds.len(), 1, "only the new message");
    assert!(next.deletes.is_empty());
}

#[tokio::test]
async fn jmap_failure_fails_only_that_collection() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Hello", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let contacts = folders.id("Contacts");
    let primed_inbox = device.sync(&inbox, "0", None).await;
    let primed_contacts = device.sync(&contacts, "0", None).await;

    h.jmap.fail_method("ContactCard/changes", "serverFail", 10);
    h.jmap.fail_method("ContactCard/get", "serverFail", 10);
    h.jmap.fail_method("ContactCard/query", "serverFail", 10);
    let collection = |id: &str, key: &str| {
        Element::new(airsync::Collection)
            .child(text(airsync::SyncKey, key))
            .child(text(airsync::CollectionId, id))
            .child(Element::new(airsync::GetChanges))
    };
    let reply = device
        .send(
            "Sync",
            Some(
                Element::new(airsync::Sync).child(
                    Element::new(airsync::Collections)
                        .child(collection(&inbox, &primed_inbox.key))
                        .child(collection(&contacts, &primed_contacts.key)),
                ),
            ),
        )
        .await;
    assert!(reply.status.is_success());
    let statuses: Vec<(String, String)> = reply
        .root()
        .find(airsync::Collections)
        .unwrap()
        .find_all(airsync::Collection)
        .map(|c| {
            (
                c.child_text(airsync::CollectionId).unwrap().to_owned(),
                c.child_text(airsync::Status).unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        statuses.contains(&(inbox.clone(), "1".to_owned())),
        "{statuses:?}"
    );
    assert!(
        statuses.iter().any(|(id, s)| id == &contacts && s != "1"),
        "{statuses:?}"
    );
    // Recorded for `devices show`.
    let devices = h.state.store.list_devices().unwrap();
    let (error, _) = devices[0].last_error.as_ref().expect("last error");
    assert!(error.contains(&contacts), "{error}");
}

/// A card with more entries than EAS has slots for, and a photo.
fn crowded_card(m: &mut support::fake_jmap::Model) -> String {
    m.insert(
        "ContactCard",
        json!({
            "@type": "Card",
            "version": "1.0",
            "addressBookIds": { "ab1": true },
            "name": { "components": [
                { "kind": "given", "value": "Ada" },
                { "kind": "surname", "value": "Lovelace" },
            ] },
            "emails": {
                "e1": { "address": "ada1@example.org", "pref": 1 },
                "e2": { "address": "ada2@example.org", "pref": 2 },
                "e3": { "address": "ada3@example.org", "pref": 3 },
                "e4": { "address": "ada4@example.org", "pref": 4 },
            },
            "phones": {
                "p1": { "number": "+44 1", "contexts": { "private": true } },
                "p2": { "number": "+44 2", "contexts": { "private": true } },
                "p3": { "number": "+44 3", "contexts": { "private": true } },
            },
            "addresses": {
                "a1": { "contexts": { "private": true }, "full": "1 Home St" },
                "a2": { "contexts": { "private": true }, "full": "2 Second Home St" },
                "a3": { "contexts": { "private": true }, "full": "3 Third Home St" },
                "a4": { "contexts": { "billing": true }, "full": "4 Billing St" },
            },
            "media": { "m1": { "kind": "photo", "mediaType": "image/png", "uri": "data:image/png;base64,iVBOR" } },
        }),
    )
}

/// A Change of `server_id` with the device's data minus Picture and with
/// `edit` applied.
fn contact_change(server_id: &str, data: &Element, edit: impl FnOnce(&mut Element)) -> Element {
    let mut data = data.clone();
    data.children
        .retain(|n| !matches!(n, tolk::wbxml::Node::Element(e) if e.tag == ct::Picture));
    edit(&mut data);
    Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id))
            .child(data),
    )
}

#[tokio::test]
async fn contact_edits_keep_what_the_device_cannot_show() {
    let h = Harness::start().await;
    let id = h.jmap.change(crowded_card);
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    let initial = device.initial_sync(&contacts).await;
    let add = &initial.adds[0];
    let server_id = add.child_text(airsync::ServerId).unwrap();
    let data = add.find(airsync::ApplicationData).unwrap();
    assert_eq!(data.child_text(ct::Email3Address), Some("ada3@example.org"));
    assert!(data.child_text(ct::Picture).is_some());

    // The phone edits one number and sends the card back without the photo.
    let change = contact_change(server_id, data, |d| {
        for child in &mut d.children {
            if let tolk::wbxml::Node::Element(e) = child
                && e.tag == ct::HomePhoneNumber
            {
                *e = text(ct::HomePhoneNumber, "+44 9");
            }
        }
    });
    let result = device.sync(&contacts, &initial.key, Some(change)).await;
    assert_eq!(result.status, "1");
    assert!(result.responses.is_empty(), "{:?}", result.responses);
    h.jmap.read(|m| {
        let card = m.get("ContactCard", &id).unwrap();
        assert_eq!(card["phones"]["p1"]["number"], "+44 9");
        assert_eq!(card["phones"]["p3"]["number"], "+44 3");
        assert_eq!(card["emails"]["e4"]["address"], "ada4@example.org");
        assert_eq!(card["addresses"]["a3"]["full"], "3 Third Home St");
        assert_eq!(card["addresses"]["a4"]["full"], "4 Billing St");
        assert_eq!(card["media"]["m1"]["uri"], "data:image/png;base64,iVBOR");
    });
    let echo = device.sync(&contacts, &result.key, None).await;
    assert!(echo.changes.is_empty(), "no echo");

    // An explicitly empty Picture removes the photo.
    let change = contact_change(server_id, data, |d| d.push(text(ct::Picture, "")));
    let result = device.sync(&contacts, &echo.key, Some(change)).await;
    assert_eq!(result.status, "1");
    h.jmap.read(|m| {
        let card = m.get("ContactCard", &id).unwrap();
        assert!(card["media"].get("m1").is_none(), "{card}");
        assert_eq!(card["emails"]["e4"]["address"], "ada4@example.org");
    });
}

#[tokio::test]
async fn contact_photos_kept_as_blobs_reach_the_device() {
    let h = Harness::start().await;
    let id = h.jmap.change(|m| {
        m.blobs.insert(
            "blob-photo".to_owned(),
            (b"JPEGDATA".to_vec(), "image/jpeg".to_owned()),
        );
        m.insert(
            "ContactCard",
            json!({
                "@type": "Card",
                "version": "1.0",
                "addressBookIds": { "ab1": true },
                "name": { "full": "Grace Hopper" },
                "media": { "p": { "@type": "Media", "kind": "photo", "mediaType": "image/jpeg", "blobId": "blob-photo" } },
            }),
        )
    });
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    let initial = device.initial_sync(&contacts).await;
    let add = &initial.adds[0];
    let server_id = add.child_text(airsync::ServerId).unwrap();
    let data = add.find(airsync::ApplicationData).unwrap();
    // base64 of JPEGDATA
    assert_eq!(data.child_text(ct::Picture), Some("SlBFR0RBVEE="));

    // Sending the same photo back leaves the stored blob alone.
    let change = contact_change(server_id, data, |d| {
        d.push(text(ct::Picture, "SlBFR0RBVEE="));
        d.push(text(ct::JobTitle, "Admiral"));
    });
    let result = device.sync(&contacts, &initial.key, Some(change)).await;
    assert_eq!(result.status, "1");
    h.jmap.read(|m| {
        let card = m.get("ContactCard", &id).unwrap();
        assert_eq!(card["media"]["p"]["blobId"], "blob-photo", "{card}");
        assert_eq!(card["titles"].as_object().unwrap().len(), 1, "{card}");
    });

    // A new photo replaces it.
    let change = contact_change(server_id, data, |d| d.push(text(ct::Picture, "TkVX")));
    let result = device.sync(&contacts, &result.key, Some(change)).await;
    assert_eq!(result.status, "1");
    h.jmap.read(|m| {
        let card = m.get("ContactCard", &id).unwrap();
        let media = card["media"].as_object().unwrap();
        assert_eq!(media.len(), 1, "{card}");
        assert_eq!(media["p"]["uri"], "data:image/jpeg;base64,TkVX");
        assert!(media["p"].get("blobId").is_none(), "{card}");
    });
}

#[tokio::test]
async fn contact_fields_outside_supported_are_left_alone() {
    let h = Harness::start().await;
    let id = h.jmap.change(crowded_card);
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    // The device says it manages names and the first email only.
    let request = Element::new(airsync::Sync).child(
        Element::new(airsync::Collections).child(
            Element::new(airsync::Collection)
                .child(text(airsync::SyncKey, "0"))
                .child(text(airsync::CollectionId, &contacts))
                .child(
                    Element::new(airsync::Supported)
                        .child(Element::new(ct::FirstName))
                        .child(Element::new(ct::LastName))
                        .child(Element::new(ct::Email1Address)),
                ),
        ),
    );
    let primed = device.send("Sync", Some(request)).await;
    let key = collection_result(&primed, &contacts)
        .child_text(airsync::SyncKey)
        .unwrap()
        .to_owned();
    let first = device.sync(&contacts, &key, None).await;
    let server_id = first.adds[0].child_text(airsync::ServerId).unwrap();

    // Only the first name, a mobile number and nothing else.
    let change = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id))
            .child(
                Element::new(airsync::ApplicationData)
                    .child(text(ct::FirstName, "Augusta"))
                    .child(text(ct::MobilePhoneNumber, "+44 7")),
            ),
    );
    let result = device.sync(&contacts, &first.key, Some(change)).await;
    assert_eq!(result.status, "1");
    h.jmap.read(|m| {
        let card = m.get("ContactCard", &id).unwrap();
        let name = card["name"]["components"].to_string();
        // Listed but not sent: cleared. Sent: written. Neither: kept.
        assert!(
            name.contains("Augusta") && !name.contains("Lovelace"),
            "{name}"
        );
        assert!(card["emails"].get("e1").is_none());
        assert_eq!(card["emails"]["e2"]["address"], "ada2@example.org");
        assert_eq!(card["phones"].as_object().unwrap().len(), 4);
        assert_eq!(card["addresses"]["a1"]["full"], "1 Home St");
        assert!(card["media"].get("m1").is_some());
    });
}

#[tokio::test]
async fn retried_add_creates_the_contact_once() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    let initial = device.initial_sync(&contacts).await;
    let add = || {
        Element::new(airsync::Commands).child(
            Element::new(airsync::Add)
                .child(text(airsync::ClientId, "new-1"))
                .child(Element::new(airsync::ApplicationData).child(text(ct::FirstName, "Grace"))),
        )
    };
    let first = device.sync(&contacts, &initial.key, Some(add())).await;
    // The response was lost; the device sends the same request again.
    let retry = device.sync(&contacts, &initial.key, Some(add())).await;
    assert_eq!(retry.status, "1");
    assert_eq!(first.responses, retry.responses, "same ServerId");
    h.jmap.read(|m| assert_eq!(m.all("ContactCard").len(), 1));

    let after = device.sync(&contacts, &retry.key, None).await;
    assert!(after.adds.is_empty() && after.changes.is_empty(), "no echo");
}

#[tokio::test]
async fn retried_add_creates_the_event_once() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&calendar).await;
    let add = || {
        Element::new(airsync::Commands).child(
            Element::new(airsync::Add)
                .child(text(airsync::ClientId, "ev-1"))
                .child(
                    Element::new(airsync::ApplicationData)
                        .child(text(cal::Subject, "Dentist"))
                        .child(text(cal::StartTime, "20261105T090000Z"))
                        .child(text(cal::EndTime, "20261105T100000Z")),
                ),
        )
    };
    let first = device.sync(&calendar, &initial.key, Some(add())).await;
    let retry = device.sync(&calendar, &initial.key, Some(add())).await;
    assert_eq!(first.responses, retry.responses);
    assert_eq!(first.responses[0].2, "1");
    h.jmap.read(|m| assert_eq!(m.all("CalendarEvent").len(), 1));
}

#[tokio::test]
async fn move_survives_a_retry_with_the_previous_key() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Move me", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let trash = folders.id("Trash");
    let inbox_sync = device.initial_sync(&inbox).await;
    let primed = device.sync(&trash, "0", None).await;
    // Trash is synced twice, so it has a previous key to retry with.
    let trash_first = device.sync(&trash, &primed.key, None).await;
    h.jmap.add_email("trash", "Old trash", "y");
    let trash_second = device.sync(&trash, &trash_first.key, None).await;
    assert_eq!(trash_second.adds.len(), 1);

    let reply = device
        .send(
            "MoveItems",
            Some(
                Element::new(mv::MoveItems).child(
                    Element::new(mv::Move)
                        .child(text(mv::SrcMsgId, inbox_sync.add_ids()[0].clone()))
                        .child(text(mv::SrcFldId, inbox.clone()))
                        .child(text(mv::DstFldId, trash.clone())),
                ),
            ),
        )
        .await;
    assert_eq!(
        reply
            .root()
            .find(mv::Response)
            .unwrap()
            .child_text(mv::Status),
        Some("3")
    );
    // The device retries its last Trash Sync with the previous key: the
    // moved email must not come back as an Add.
    let retry = device.sync(&trash, &trash_first.key, None).await;
    let subjects: Vec<&str> = retry
        .adds
        .iter()
        .filter_map(|a| a.find(airsync::ApplicationData)?.child_text(em::Subject))
        .collect();
    assert_eq!(subjects, vec!["Old trash"]);
}

#[tokio::test]
async fn login_forms_of_one_account_share_state() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Hello", "x");
    let (mut device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;

    // The same phone, now logging in with the short name.
    device.user = support::fake_jmap::ALIAS.to_owned();
    let next = device.sync(&inbox, &initial.key, None).await;
    assert_eq!(next.status, "1", "the SyncKey is still valid");
    assert!(next.adds.is_empty(), "nothing is sent twice");
    let again = device.folder_sync_from(&folders.key).await;
    assert!(again.by_name.is_empty(), "the hierarchy is unchanged");
}

#[tokio::test]
async fn ping_reports_a_new_folder_and_folder_sync_delivers_it() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    device.initial_sync(&inbox).await;

    let jmap = &h.jmap;
    let watched = [(inbox.as_str(), "Email")];
    let (ping, _) = tokio::join!(device.ping(60, &watched), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        jmap.change(|m| {
            m.insert(
                "Mailbox",
                json!({ "name": "Projects", "parentId": null, "role": null, "sortOrder": 9 }),
            )
        });
    });
    assert_eq!(ping.0, "7", "FolderSync required");

    let update = device.folder_sync_from(&folders.key).await;
    let names: Vec<&str> = update.by_name.iter().map(|(n, _, _)| n.as_str()).collect();
    assert_eq!(names, vec!["Projects"]);

    // In step again: the next Ping waits normally.
    let (status, _) = tokio::time::timeout(Duration::from_secs(3), device.ping(60, &watched))
        .await
        .unwrap_or(("timeout".to_owned(), vec![]));
    assert_eq!(status, "timeout", "no answer without changes");
}

#[tokio::test]
async fn contact_changes_wake_only_contact_pings() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let contacts = folders.id("Contacts");
    let inbox = folders.id("Inbox");
    device.initial_sync(&contacts).await;
    device.initial_sync(&inbox).await;

    let jmap = &h.jmap;
    let watched = [(inbox.as_str(), "Email"), (contacts.as_str(), "Contacts")];
    let (ping, _) = tokio::join!(device.ping(60, &watched), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        jmap.take_calls();
        jmap.change(|m| {
            m.insert(
                "ContactCard",
                json!({ "@type": "Card", "addressBookIds": { "ab1": true }, "name": { "full": "New" } }),
            )
        });
    });
    assert_eq!(ping.0, "2");
    assert_eq!(ping.1, vec![contacts.clone()]);
    let calls = h.jmap.take_calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("Email/") || c.starts_with("Mailbox/")),
        "mail was not re-checked: {calls:?}"
    );
}

fn sync_request(collections: &[(&str, &str)], extra: Vec<Element>) -> Element {
    let mut list = Element::new(airsync::Collections);
    for (id, key) in collections {
        list.push(
            Element::new(airsync::Collection)
                .child(text(airsync::SyncKey, *key))
                .child(text(airsync::CollectionId, *id)),
        );
    }
    let mut sync = Element::new(airsync::Sync);
    for element in extra {
        sync.push(element);
    }
    sync.child(list)
}

fn collection_result<'a>(reply: &'a support::Reply, id: &str) -> &'a Element {
    reply
        .root()
        .find(airsync::Collections)
        .unwrap()
        .find_all(airsync::Collection)
        .find(|c| c.child_text(airsync::CollectionId) == Some(id))
        .unwrap_or_else(|| panic!("no collection {id}"))
}

#[tokio::test]
async fn empty_sync_repeats_the_last_request_with_current_keys() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;

    h.jmap.add_email("inbox", "After", "x");
    let reply = device.send("Sync", None).await;
    let collection = collection_result(&reply, &inbox);
    assert_eq!(collection.child_text(airsync::Status), Some("1"));
    assert_ne!(
        collection.child_text(airsync::SyncKey),
        Some(initial.key.as_str())
    );
    let adds = collection
        .find(airsync::Commands)
        .unwrap()
        .find_all(airsync::Add)
        .count();
    assert_eq!(adds, 1);

    // Without anything to repeat, an empty Sync is status 13.
    let fresh = h.device("d2");
    let mut fresh = fresh;
    fresh.provision().await;
    let reply = fresh.send("Sync", None).await;
    assert_eq!(reply.root().child_text(airsync::Status), Some("13"));
}

#[tokio::test]
async fn partial_sync_merges_with_the_cached_collections() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Hello", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let contacts = folders.id("Contacts");
    let mail = device.sync(&inbox, "0", None).await;
    let cards = device.sync(&contacts, "0", None).await;
    let full = device
        .send(
            "Sync",
            Some(sync_request(
                &[(&inbox, &mail.key), (&contacts, &cards.key)],
                vec![],
            )),
        )
        .await;
    let mail_key = collection_result(&full, &inbox)
        .child_text(airsync::SyncKey)
        .unwrap()
        .to_owned();

    // Only the contacts collection, marked partial: the inbox comes along.
    let partial = device
        .send(
            "Sync",
            Some(sync_request(
                &[(
                    &contacts,
                    collection_result(&full, &contacts)
                        .child_text(airsync::SyncKey)
                        .unwrap(),
                )],
                vec![Element::new(airsync::Partial)],
            )),
        )
        .await;
    assert_eq!(
        collection_result(&partial, &contacts).child_text(airsync::Status),
        Some("1")
    );
    assert_eq!(
        collection_result(&partial, &inbox).child_text(airsync::SyncKey),
        Some(mail_key.as_str()),
        "the inbox is synced with its current key"
    );
}

#[tokio::test]
async fn sync_with_wait_returns_when_mail_arrives() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;

    let jmap = &h.jmap;
    let request = sync_request(
        &[(&inbox, &initial.key)],
        vec![text(airsync::HeartbeatInterval, "120")],
    );
    let started = std::time::Instant::now();
    let (reply, _) = tokio::join!(device.send("Sync", Some(request)), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        jmap.add_email("inbox", "Pushed", "x");
    });
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "answered on the change"
    );
    let collection = collection_result(&reply, &inbox);
    assert_eq!(
        collection
            .find(airsync::Commands)
            .unwrap()
            .find_all(airsync::Add)
            .count(),
        1
    );
}

#[tokio::test]
async fn waiting_sync_ends_when_the_device_sends_another_request() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    let request = sync_request(&[(&inbox, &initial.key)], vec![text(airsync::Wait, "5")]);
    let (waiting, _) = tokio::join!(device.send("Sync", Some(request)), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        device
            .send(
                "FolderSync",
                Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, &folders.key))),
            )
            .await
    });
    assert!(waiting.status.is_success());
    assert!(waiting.body.is_none(), "an empty response ends the wait");
}

#[tokio::test]
async fn out_of_range_wait_is_status_14() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    let reply = device
        .send(
            "Sync",
            Some(sync_request(
                &[(&inbox, &initial.key)],
                vec![text(airsync::HeartbeatInterval, "10")],
            )),
        )
        .await;
    assert_eq!(reply.root().child_text(airsync::Status), Some("14"));
    assert_eq!(reply.root().child_text(airsync::Limit), Some("60"));
}

#[tokio::test]
async fn email_fields_reach_the_device() {
    use tolk::wbxml::cp::{airsyncbase as base, email2};
    let h = Harness::start().await;
    let id = h.jmap.add_email("inbox", "Urgent", "Please call me back");
    h.jmap.change(|m| {
        m.patch(
            "Email",
            &id,
            json!({ "header:Importance:asText": "High", "keywords/$answered": true })
                .as_object()
                .unwrap(),
        );
    });
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let primed = device.sync(&inbox, "0", None).await;
    // Ask for a 10 character preview with the plain text body.
    let request = Element::new(airsync::Sync).child(
        Element::new(airsync::Collections).child(
            Element::new(airsync::Collection)
                .child(text(airsync::SyncKey, primed.key.clone()))
                .child(text(airsync::CollectionId, inbox.clone()))
                .child(Element::new(airsync::GetChanges))
                .child(
                    Element::new(airsync::Options).child(
                        Element::new(base::BodyPreference)
                            .child(text(base::Type, "1"))
                            .child(text(base::Preview, "10")),
                    ),
                ),
        ),
    );
    let reply = device.send("Sync", Some(request)).await;
    let data = reply
        .root()
        .find(airsync::Collections)
        .and_then(|c| c.find(airsync::Collection))
        .and_then(|c| c.find(airsync::Commands))
        .and_then(|c| c.find(airsync::Add))
        .and_then(|a| a.find(airsync::ApplicationData))
        .expect("an Add");
    assert_eq!(data.child_text(em::Importance), Some("2"));
    assert_eq!(data.child_text(email2::LastVerbExecuted), Some("1"));
    assert_eq!(
        data.find(email2::ConversationId)
            .and_then(|c| c.opaque())
            .map(<[u8]>::len),
        Some(16)
    );
    assert_eq!(
        data.find(base::Body)
            .and_then(|b| b.child_text(base::Preview)),
        Some("Please cal")
    );
    let key = reply
        .root()
        .find(airsync::Collections)
        .and_then(|c| c.find(airsync::Collection))
        .and_then(|c| c.child_text(airsync::SyncKey))
        .unwrap()
        .to_owned();

    // Forwarded from another client: a Change carries the new verb.
    h.jmap.change(|m| {
        m.patch(
            "Email",
            &id,
            json!({ "keywords/$answered": null, "keywords/$forwarded": true })
                .as_object()
                .unwrap(),
        );
    });
    let next = device.sync(&inbox, &key, None).await;
    assert_eq!(next.changes.len(), 1);
    let verb = next.changes[0]
        .find(airsync::ApplicationData)
        .and_then(|d| d.child_text(email2::LastVerbExecuted));
    assert_eq!(verb, Some("3"));
}

#[tokio::test]
async fn out_of_office_round_trips_through_the_vacation_response() {
    use tolk::wbxml::cp::settings as st;
    let h = Harness::start().await;
    let (device, _) = h.ready_device("d1").await;
    let set = Element::new(st::Settings).child(
        Element::new(st::Oof).child(
            Element::new(st::Set).child(text(st::OofState, "1")).child(
                Element::new(st::OofMessage)
                    .child(Element::new(st::AppliesToInternal))
                    .child(text(st::Enabled, "1"))
                    .child(text(st::ReplyMessage, "On holiday"))
                    .child(text(st::BodyType, "Text")),
            ),
        ),
    );
    let reply = device.send("Settings", Some(set)).await;
    assert_eq!(
        reply
            .root()
            .find(st::Oof)
            .and_then(|o| o.child_text(st::Status)),
        Some("1")
    );
    h.jmap.read(|m| {
        let vacation = m.get("VacationResponse", "singleton").unwrap();
        assert_eq!(vacation["isEnabled"], true);
        assert_eq!(vacation["textBody"], "On holiday");
    });

    let get = Element::new(st::Settings).child(
        Element::new(st::Oof).child(Element::new(st::Get).child(text(st::BodyType, "Text"))),
    );
    let reply = device.send("Settings", Some(get)).await;
    let get = reply
        .root()
        .find(st::Oof)
        .and_then(|o| o.find(st::Get))
        .unwrap();
    assert_eq!(get.child_text(st::OofState), Some("1"));
    assert_eq!(
        get.find(st::OofMessage)
            .and_then(|m| m.child_text(st::ReplyMessage)),
        Some("On holiday")
    );
}

async fn folder_op(device: &support::Device, cmd: &str, children: Vec<Element>) -> Element {
    let root = match cmd {
        "FolderCreate" => fh::FolderCreate,
        "FolderUpdate" => fh::FolderUpdate,
        _ => fh::FolderDelete,
    };
    let body = children
        .into_iter()
        .fold(Element::new(root), Element::child);
    let reply = device.send(cmd, Some(body)).await;
    let status = reply.status;
    reply
        .body
        .unwrap_or_else(|| panic!("{cmd}: HTTP {status} without a body"))
}

#[tokio::test]
async fn folders_can_be_created_renamed_and_deleted() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;

    let created = folder_op(
        &device,
        "FolderCreate",
        vec![
            text(fh::SyncKey, folders.key.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "Projects"),
            text(fh::Type, "12"),
        ],
    )
    .await;
    assert_eq!(created.child_text(fh::Status), Some("1"));
    let key = created.child_text(fh::SyncKey).unwrap().to_owned();
    let server_id = created.child_text(fh::ServerId).unwrap().to_owned();
    let mailbox_id = h.jmap.read(|m| {
        m.all("Mailbox")
            .into_iter()
            .find(|mb| mb["name"] == "Projects")
            .map(|mb| mb["id"].as_str().unwrap().to_owned())
            .expect("mailbox created")
    });
    // The device is not told again about the folder it made.
    assert!(device.folder_sync_from(&key).await.by_name.is_empty());

    let duplicate = folder_op(
        &device,
        "FolderCreate",
        vec![
            text(fh::SyncKey, key.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "projects"),
            text(fh::Type, "12"),
        ],
    )
    .await;
    assert_eq!(duplicate.child_text(fh::Status), Some("2"));

    let renamed = folder_op(
        &device,
        "FolderUpdate",
        vec![
            text(fh::SyncKey, key.clone()),
            text(fh::ServerId, server_id.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "Archive 2026"),
        ],
    )
    .await;
    assert_eq!(renamed.child_text(fh::Status), Some("1"));
    let key = renamed.child_text(fh::SyncKey).unwrap().to_owned();
    h.jmap.read(|m| {
        assert_eq!(
            m.get("Mailbox", &mailbox_id).unwrap()["name"],
            "Archive 2026"
        )
    });

    let inbox = folders.id("Inbox");
    let special = folder_op(
        &device,
        "FolderDelete",
        vec![text(fh::SyncKey, key.clone()), text(fh::ServerId, inbox)],
    )
    .await;
    assert_eq!(special.child_text(fh::Status), Some("3"));

    let stale = folder_op(
        &device,
        "FolderDelete",
        vec![
            text(fh::SyncKey, folders.key.clone()),
            text(fh::ServerId, server_id.clone()),
        ],
    )
    .await;
    assert_eq!(stale.child_text(fh::Status), Some("9"));

    let deleted = folder_op(
        &device,
        "FolderDelete",
        vec![
            text(fh::SyncKey, key.clone()),
            text(fh::ServerId, server_id.clone()),
        ],
    )
    .await;
    assert_eq!(deleted.child_text(fh::Status), Some("1"));
    let key = deleted.child_text(fh::SyncKey).unwrap().to_owned();
    // It moved into Trash, where the device finds it on the next FolderSync.
    h.jmap
        .read(|m| assert_eq!(m.get("Mailbox", &mailbox_id).unwrap()["parentId"], "trash"));
    let after = device.folder_sync_from(&key).await;
    assert_eq!(after.by_name.len(), 1);
    assert_eq!(after.by_name[0].0, "Archive 2026");
}

#[tokio::test]
async fn devices_can_add_calendars_but_not_delete_them() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let created = folder_op(
        &device,
        "FolderCreate",
        vec![
            text(fh::SyncKey, folders.key.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "Football"),
            text(fh::Type, "13"),
        ],
    )
    .await;
    assert_eq!(created.child_text(fh::Status), Some("1"));
    h.jmap
        .read(|m| assert!(m.all("Calendar").iter().any(|c| c["name"] == "Football")));
    let deleted = folder_op(
        &device,
        "FolderDelete",
        vec![
            text(fh::SyncKey, created.child_text(fh::SyncKey).unwrap()),
            text(fh::ServerId, created.child_text(fh::ServerId).unwrap()),
        ],
    )
    .await;
    assert_eq!(deleted.child_text(fh::Status), Some("3"));
}

#[tokio::test]
async fn empty_trash_deletes_messages_and_subfolders() {
    use tolk::wbxml::cp::item_operations as io;
    let h = Harness::start().await;
    let gone = h.jmap.add_email("trash", "Old", "x");
    let shared = h.jmap.add_email("trash", "Also in inbox", "y");
    h.jmap.change(|m| {
        m.patch("Email", &shared, json!({ "mailboxIds/inbox": true }).as_object().unwrap());
        m.insert(
            "Mailbox",
            json!({ "id": "old-project", "name": "Old project", "parentId": "trash", "role": null, "sortOrder": 5 }),
        );
    });
    let (device, folders) = h.ready_device("d1").await;
    let trash = folders.id("Trash");
    let initial = device.initial_sync(&trash).await;
    assert_eq!(initial.adds.len(), 2);

    let reply = device
        .send(
            "ItemOperations",
            Some(
                Element::new(io::ItemOperations).child(
                    Element::new(io::EmptyFolderContents)
                        .child(text(airsync::CollectionId, trash.clone()))
                        .child(Element::new(io::Options).child(Element::new(io::DeleteSubFolders))),
                ),
            ),
        )
        .await;
    let result = reply
        .root()
        .find(io::Response)
        .and_then(|r| r.find(io::EmptyFolderContents))
        .unwrap();
    assert_eq!(result.child_text(io::Status), Some("1"));
    h.jmap.read(|m| {
        assert!(m.get("Email", &gone).is_none(), "deleted for good");
        assert_eq!(
            m.get("Email", &shared).unwrap()["mailboxIds"],
            json!({ "inbox": true })
        );
        assert!(
            m.get("Mailbox", "old-project").is_none(),
            "subfolder deleted"
        );
    });
    // The device already emptied the folder: no Deletes follow.
    let next = device.sync(&trash, &initial.key, None).await;
    assert!(next.deletes.is_empty(), "{:?}", next.deletes);
}

#[tokio::test]
async fn resolve_recipients_finds_contacts() {
    use tolk::wbxml::cp::resolve_recipients as rr;
    let h = Harness::start().await;
    h.jmap.change(|m| {
        for (given, surname, address) in [("Ada", "Lovelace", "ada@example.org"), ("Adam", "Smith", "adam@example.org")] {
            m.insert(
                "ContactCard",
                json!({
                    "@type": "Card", "addressBookIds": { "ab1": true },
                    "name": { "components": [{ "kind": "given", "value": given }, { "kind": "surname", "value": surname }] },
                    "emails": { "e": { "address": address } },
                }),
            );
        }
    });
    let (device, _) = h.ready_device("d1").await;
    let request = Element::new(rr::ResolveRecipients)
        .child(text(rr::To, "ada@example.org"))
        .child(text(rr::To, "ada"))
        .child(text(rr::To, "nobody"))
        .child(Element::new(rr::Options).child(text(rr::CertificateRetrieval, "2")));
    let reply = device.send("ResolveRecipients", Some(request)).await;
    let root = reply.root();
    assert_eq!(root.child_text(rr::Status), Some("1"));
    let answers: Vec<(String, String, usize)> = root
        .find_all(rr::Response)
        .map(|r| {
            (
                r.child_text(rr::To).unwrap().to_owned(),
                r.child_text(rr::Status).unwrap().to_owned(),
                r.find_all(rr::Recipient).count(),
            )
        })
        .collect();
    assert_eq!(
        answers,
        vec![
            ("ada@example.org".to_owned(), "1".to_owned(), 1),
            ("ada".to_owned(), "2".to_owned(), 2),
            ("nobody".to_owned(), "4".to_owned(), 0),
        ]
    );
    let certificates = root
        .find(rr::Response)
        .and_then(|r| r.find(rr::Recipient))
        .and_then(|r| r.find(rr::Certificates))
        .and_then(|c| c.child_text(rr::Status));
    assert_eq!(certificates, Some("7"));
}

#[tokio::test]
async fn options_offer_the_configured_versions() {
    let h = Harness::start().await;
    let reply = tolk::http_client()
        .build()
        .unwrap()
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/Microsoft-Server-ActiveSync", h.url),
        )
        .basic_auth(support::fake_jmap::USER, Some(support::fake_jmap::PASSWORD))
        .send()
        .await
        .unwrap();
    assert_eq!(
        reply.headers()["ms-asprotocolversions"],
        "14.0,14.1,16.0,16.1"
    );
    assert_eq!(reply.headers()["ms-server-activesync"], "16.1");
}

fn weekly_meeting(m: &mut support::fake_jmap::Model) -> String {
    m.insert(
        "CalendarEvent",
        json!({
            "@type": "Event", "uid": "gym@example.org", "calendarIds": { "cal1": true },
            "title": "Gym", "start": "2026-11-02T18:00:00", "timeZone": "Europe/Amsterdam",
            "duration": "PT1H", "locations": { "l1": { "@type": "Location", "name": "Gym hall" } },
            "recurrenceRule": { "@type": "RecurrenceRule", "frequency": "weekly" },
            "recurrenceOverrides": { "2026-11-09T18:00:00": { "excluded": true } },
        }),
    )
}

#[tokio::test]
async fn eas16_calendar_uses_locations_and_instance_ids() {
    use tolk::wbxml::cp::airsyncbase as base;
    let h = Harness::start().await;
    let id = h.jmap.change(weekly_meeting);
    let (mut device, folders) = h.ready_device("d1").await;
    device.version = "16.1".to_owned();
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&calendar).await;
    let data = initial.adds[0].find(airsync::ApplicationData).unwrap();
    assert!(data.find(cal::Location).is_none());
    assert_eq!(
        data.find(base::Location)
            .and_then(|l| l.child_text(base::DisplayName)),
        Some("Gym hall")
    );
    let exception = data
        .find(cal::Exceptions)
        .and_then(|e| e.find(cal::Exception))
        .unwrap();
    // 18:00 in Amsterdam is 17:00 UTC in November.
    assert_eq!(
        exception.child_text(base::InstanceId),
        Some("20261109T170000Z")
    );
    let server_id = initial.add_ids()[0].clone();

    // Move one occurrence an hour later, delete another.
    let commands = Element::new(airsync::Commands)
        .child(
            Element::new(airsync::Change)
                .child(text(airsync::ServerId, server_id.clone()))
                .child(text(base::InstanceId, "20261116T170000Z"))
                .child(
                    Element::new(airsync::ApplicationData)
                        .child(text(cal::StartTime, "20261116T180000Z"))
                        .child(text(cal::EndTime, "20261116T190000Z")),
                ),
        )
        .child(
            Element::new(airsync::Delete)
                .child(text(airsync::ServerId, server_id))
                .child(text(base::InstanceId, "20261123T170000Z")),
        );
    let result = device.sync(&calendar, &initial.key, Some(commands)).await;
    assert_eq!(result.status, "1");
    assert!(result.responses.is_empty(), "{:?}", result.responses);
    h.jmap.read(|m| {
        let overrides = &m.get("CalendarEvent", &id).unwrap()["recurrenceOverrides"];
        assert_eq!(overrides["2026-11-09T18:00:00"]["excluded"], true, "kept");
        assert_eq!(
            overrides["2026-11-16T18:00:00"]["start"],
            "2026-11-16T19:00:00"
        );
        assert_eq!(overrides["2026-11-23T18:00:00"]["excluded"], true);
        assert_eq!(
            m.get("CalendarEvent", &id).unwrap()["title"],
            "Gym",
            "the series is untouched"
        );
    });
}

#[tokio::test]
async fn eas16_drafts_are_saved_edited_and_sent() {
    use tolk::wbxml::cp::{airsyncbase as base, email2};
    let h = Harness::start().await;
    let (mut device, folders) = h.ready_device("d1").await;
    device.version = "16.1".to_owned();
    let drafts = folders.id("Drafts");
    let initial = device.initial_sync(&drafts).await;

    let add = Element::new(airsync::Commands).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "draft-1"))
            .child(
                Element::new(airsync::ApplicationData)
                    .child(text(em::To, "friend@example.org"))
                    .child(text(em::Subject, "Plans"))
                    .child(
                        Element::new(base::Body)
                            .child(text(base::Type, "1"))
                            .child(text(base::Data, "First version")),
                    ),
            ),
    );
    let saved = device.sync(&drafts, &initial.key, Some(add)).await;
    let (_, _, status, server_id) = saved.responses[0].clone();
    assert_eq!(status, "1");
    let draft_subjects = |m: &support::fake_jmap::Model| -> Vec<String> {
        m.all("Email")
            .into_iter()
            .filter(|e| e["mailboxIds"]["drafts"] == true)
            .map(|e| e["subject"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    h.jmap
        .read(|m| assert_eq!(draft_subjects(m), vec!["Plans"]));

    let edit = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id.clone()))
            .child(
                Element::new(airsync::ApplicationData).child(text(em::Subject, "Plans for Friday")),
            ),
    );
    let edited = device.sync(&drafts, &saved.key, Some(edit)).await;
    assert!(edited.responses.is_empty(), "{:?}", edited.responses);
    h.jmap
        .read(|m| assert_eq!(draft_subjects(m), vec!["Plans for Friday"]));
    // Not echoed back: the device has this version under the same ServerId.
    let quiet = device.sync(&drafts, &edited.key, None).await;
    assert!(
        quiet.adds.is_empty() && quiet.deletes.is_empty(),
        "{:?} {:?}",
        quiet.add_ids(),
        quiet.deletes
    );

    let send = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id.clone()))
            .child(Element::new(airsync::ApplicationData).child(Element::new(email2::Send))),
    );
    let sent = device.sync(&drafts, &quiet.key, Some(send)).await;
    assert!(sent.responses.is_empty());
    h.jmap.read(|m| {
        assert!(draft_subjects(m).is_empty(), "draft gone");
        assert_eq!(m.submissions.len(), 1);
        assert_eq!(
            m.submissions[0]["envelope"]["rcptTo"],
            json!([{ "email": "friend@example.org" }])
        );
    });
    // The sent draft leaves the device's Drafts in the same response.
    assert_eq!(sent.deletes, vec![server_id]);
}

#[tokio::test]
async fn eas16_draft_attachments_are_added_and_deleted() {
    use tolk::wbxml::cp::airsyncbase as base;
    let h = Harness::start().await;
    let (mut device, folders) = h.ready_device("d1").await;
    device.version = "16.1".to_owned();
    let drafts = folders.id("Drafts");
    let initial = device.initial_sync(&drafts).await;
    let attach = |client_id: &str, name: &str, content: &str| {
        Element::new(base::Add)
            .child(text(base::ClientId, client_id))
            .child(text(base::Content, content))
            .child(text(base::DisplayName, name))
            .child(text(base::ContentType, "text/plain"))
            .child(text(base::Method, "1"))
    };
    let reference = |response: &Element, client_id: &str| -> String {
        response
            .find(airsync::ApplicationData)
            .and_then(|d| d.find(base::Attachments))
            .and_then(|a| {
                a.find_all(base::Attachment)
                    .find(|a| a.child_text(base::ClientId) == Some(client_id))
            })
            .and_then(|a| a.child_text(base::FileReference))
            .unwrap_or_else(|| panic!("no FileReference for {client_id}: {response:?}"))
            .to_owned()
    };
    let draft_attachments = |m: &support::fake_jmap::Model| -> Vec<String> {
        m.all("Email")
            .into_iter()
            .filter(|e| e["mailboxIds"]["drafts"] == true)
            .flat_map(|e| e["attachments"].as_array().cloned().unwrap_or_default())
            .map(|a| a["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    // base64 of "first file" and "second file"
    let add = Element::new(airsync::Commands).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "draft-1"))
            .child(
                Element::new(airsync::ApplicationData)
                    .child(text(em::Subject, "With files"))
                    .child(Element::new(base::Attachments).child(attach(
                        "att-1",
                        "one.txt",
                        "Zmlyc3QgZmlsZQ==",
                    ))),
            ),
    );
    let saved = device.sync(&drafts, &initial.key, Some(add)).await;
    let response = &saved.response_elements[0];
    assert_eq!(response.child_text(airsync::Status), Some("1"));
    let first = reference(response, "att-1");
    let server_id = response.child_text(airsync::ServerId).unwrap().to_owned();
    h.jmap
        .read(|m| assert_eq!(draft_attachments(m), vec!["one.txt"]));

    // Add a second file and delete the first.
    let edit = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id.clone()))
            .child(
                Element::new(airsync::ApplicationData).child(
                    Element::new(base::Attachments)
                        .child(attach("att-2", "two.txt", "c2Vjb25kIGZpbGU="))
                        .child(Element::new(base::Delete).child(text(base::FileReference, first))),
                ),
            ),
    );
    let edited = device.sync(&drafts, &saved.key, Some(edit)).await;
    let response = &edited.response_elements[0];
    assert_eq!(
        response.child_text(airsync::ServerId),
        Some(server_id.as_str())
    );
    let second = reference(response, "att-2");
    h.jmap.read(|m| {
        assert_eq!(draft_attachments(m), vec!["two.txt"]);
        let (blob, _, _) = tolk::eas::email::parse_file_reference(&second);
        assert_eq!(m.blobs[blob].0, b"second file");
    });

    // Keeping it through a subject edit.
    let edit = Element::new(airsync::Commands).child(
        Element::new(airsync::Change)
            .child(text(airsync::ServerId, server_id.clone()))
            .child(Element::new(airsync::ApplicationData).child(text(em::Subject, "Renamed"))),
    );
    device.sync(&drafts, &edited.key, Some(edit)).await;
    h.jmap
        .read(|m| assert_eq!(draft_attachments(m), vec!["two.txt"]));
}

#[tokio::test]
async fn attachments_stream_into_item_operations() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tolk::wbxml::cp::{airsyncbase as base, item_operations as io};
    let h = Harness::start().await;
    // An odd size, so the base64 carry across download chunks is exercised.
    let file: Vec<u8> = (0..300_001u32).map(|i| (i * 7 % 251) as u8).collect();
    h.jmap.change(|m| {
        m.blobs.insert(
            "big".to_owned(),
            (file.clone(), "application/octet-stream".to_owned()),
        )
    });
    let (device, _) = h.ready_device("d1").await;
    let fetch = |range: Option<&str>| {
        let mut fetch = Element::new(io::Fetch)
            .child(text(io::Store, "Mailbox"))
            .child(text(base::FileReference, "big|application/octet-stream"));
        if let Some(range) = range {
            fetch.push(Element::new(io::Options).child(text(io::Range, range)));
        }
        Element::new(io::ItemOperations).child(fetch)
    };
    let data = |reply: &support::Reply| -> Vec<u8> {
        let properties = reply
            .root()
            .find(io::Response)
            .and_then(|r| r.find(io::Fetch))
            .and_then(|f| f.find(io::Properties))
            .unwrap()
            .clone();
        STANDARD
            .decode(properties.child_text(io::Data).unwrap())
            .unwrap()
    };

    let reply = device.send("ItemOperations", Some(fetch(None))).await;
    assert!(reply.status.is_success());
    assert_eq!(data(&reply), file);

    // Ranges come from the server, or are cut from the whole blob when the
    // server ignores the Range header.
    let range = |reply: &support::Reply| -> (String, String, String) {
        let fetch = reply
            .root()
            .find(io::Response)
            .and_then(|r| r.find(io::Fetch))
            .unwrap();
        let properties = fetch.find(io::Properties);
        let field = |tag| {
            properties
                .and_then(|p| p.child_text(tag))
                .unwrap_or_default()
                .to_owned()
        };
        (
            fetch.child_text(io::Status).unwrap_or_default().to_owned(),
            field(io::Range),
            field(io::Total),
        )
    };
    for ignore in [false, true] {
        h.jmap.change(|m| m.ignore_ranges = ignore);
        let reply = device
            .send("ItemOperations", Some(fetch(Some("10-19"))))
            .await;
        assert_eq!(data(&reply), file[10..20], "ignoring ranges: {ignore}");
        assert_eq!(
            range(&reply),
            ("1".to_owned(), "10-19".to_owned(), "300001".to_owned())
        );
        let reply = device
            .send("ItemOperations", Some(fetch(Some("300000-400000"))))
            .await;
        assert_eq!(data(&reply), file[300_000..]);
        assert_eq!(range(&reply).1, "300000-300000");
        let reply = device
            .send("ItemOperations", Some(fetch(Some("300001-400000"))))
            .await;
        assert_eq!(range(&reply).0, "8", "past the end");
    }
    h.jmap.read(|m| assert_eq!(m.ranges[0], "bytes=10-19"));
    let reply = device
        .send("ItemOperations", Some(fetch(Some("9-1"))))
        .await;
    assert_eq!(range(&reply).0, "8", "backwards");

    // A missing blob is a status, not a broken stream.
    let missing = Element::new(io::ItemOperations).child(
        Element::new(io::Fetch)
            .child(text(io::Store, "Mailbox"))
            .child(text(base::FileReference, "nope|text/plain")),
    );
    let reply = device.send("ItemOperations", Some(missing)).await;
    let status = reply
        .root()
        .find(io::Response)
        .and_then(|r| r.find(io::Fetch))
        .and_then(|f| f.child_text(io::Status))
        .map(str::to_owned);
    assert_eq!(status.as_deref(), Some("15"));
}

#[tokio::test]
async fn oauth_tokens_work_like_passwords() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Token mail", "x");
    // A device with a password, and the same user's device with a token:
    // both land on the same account.
    let (_, folders) = h.ready_device("d1").await;
    let mut token_device = h.device("d2");
    token_device.0.bearer = true;
    token_device.0.password = support::fake_jmap::TOKEN.to_owned();
    token_device.provision().await;
    let token_folders = token_device.folder_sync().await;
    assert_eq!(token_folders.id("Inbox"), folders.id("Inbox"));
    let initial = token_device.initial_sync(&token_folders.id("Inbox")).await;
    assert_eq!(initial.adds.len(), 1);
    let devices = h.state.store.list_devices().unwrap();
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().all(|d| d.user == devices[0].user));

    // A wrong token is refused.
    let reply = token_device
        .send_as("FolderSync", None, "wrong-token")
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn failed_logins_block_guessing_but_not_working_passwords() {
    let h = Harness::start().await;
    let (device, folders) = h.ready_device("d1").await;
    let folder_sync =
        || Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, folders.key.clone())));
    for _ in 0..10 {
        let reply = device.send_as("FolderSync", folder_sync(), "guess").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    // The other login name has the right password but never logged in
    // from here: the address is blocked for it.
    let mut alias = h.device("d2");
    alias.user = support::fake_jmap::ALIAS.to_owned();
    let blocked = alias.send("FolderSync", folder_sync()).await;
    assert_eq!(blocked.status, StatusCode::UNAUTHORIZED);
    // The password that already worked keeps working.
    let reply = device.send("FolderSync", folder_sync()).await;
    assert!(reply.status.is_success());
}

#[tokio::test]
async fn metrics_count_requests() {
    let h = Harness::start().await;
    let (_device, _) = h.ready_device("d1").await;
    let text = tolk::metrics::render(&h.state, 1000, 256);
    assert!(
        text.contains("tolk_requests_total{cmd=\"FolderSync\",status=\"200\"} 1"),
        "{text}"
    );
    assert!(text.contains("tolk_pings_waiting 0"));
    assert!(text.contains("tolk_state_rows{table=\"devices\"} 1"));
}

#[tokio::test]
async fn restoring_an_older_backup_makes_devices_resync() {
    use tolk::maintenance::{Backups, backup_if_due};
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Before the backup", "x");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let initial = device.initial_sync(&inbox).await;
    let backups = Backups {
        dir: h.dir.path().join("backups"),
        keep: 7,
    };
    backup_if_due(&h.state, &backups).unwrap();
    backup_if_due(&h.state, &backups).unwrap();
    let files: Vec<_> = std::fs::read_dir(&backups.dir).unwrap().collect();
    assert_eq!(files.len(), 1, "one backup a day");
    let backup = files[0].as_ref().unwrap().path();

    h.jmap.add_email("inbox", "After the backup", "y");
    let later = device.sync(&inbox, &initial.key, None).await;
    assert_eq!(later.adds.len(), 1);

    // The server comes back from the backup; the device is a step ahead.
    let restored = Harness::start_on(&backup).await;
    restored.jmap.add_email("inbox", "Hello", "x");
    let mut same_phone = restored.device("d1");
    same_phone.policy_key = device.policy_key.clone();
    let stale = same_phone.sync(&inbox, &later.key, None).await;
    assert_eq!(stale.status, "3", "invalid SyncKey: the device starts over");
    let again = same_phone.initial_sync(&inbox).await;
    assert_eq!(again.status, "1");
    assert_eq!(again.adds.len(), 1);
}

#[tokio::test]
async fn blocked_and_wiped_devices() {
    use tolk::wbxml::cp::provision as pv;
    let h = Harness::start().await;
    let (mut device, folders) = h.ready_device("d1").await;
    device.version = "16.1".to_owned();
    let account = support::fake_jmap::ACCOUNT;
    let folder_sync =
        || Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, folders.key.clone())));

    assert!(
        h.state
            .store
            .set_device_status(account, "d1", "blocked")
            .unwrap()
    );
    assert_eq!(
        device.send("FolderSync", folder_sync()).await.status,
        StatusCode::FORBIDDEN
    );
    h.state
        .store
        .set_device_status(account, "d1", "allowed")
        .unwrap();
    assert!(
        device
            .send("FolderSync", folder_sync())
            .await
            .status
            .is_success()
    );

    // An account-only wipe: every command says "provision", Provision says
    // "wipe the account", and the confirmation ends the device's access.
    h.state
        .store
        .set_device_status(account, "d1", "account-wipe")
        .unwrap();
    let reply = device.send("FolderSync", folder_sync()).await;
    assert_eq!(reply.root().child_text(fh::Status), Some("140"));
    let provision =
        Element::new(pv::Provision).child(Element::new(pv::Policies).child(
            Element::new(pv::Policy).child(text(pv::PolicyType, "MS-EAS-Provisioning-WBXML")),
        ));
    let reply = device.send("Provision", Some(provision)).await;
    assert!(reply.root().find(pv::AccountOnlyRemoteWipe).is_some());
    let ack = Element::new(pv::Provision)
        .child(Element::new(pv::AccountOnlyRemoteWipe).child(text(pv::Status, "1")));
    device.send("Provision", Some(ack)).await;
    assert_eq!(h.state.store.device_status(account, "d1").unwrap(), "wiped");
    assert_eq!(
        device.send("FolderSync", folder_sync()).await.status,
        StatusCode::FORBIDDEN
    );

    let listed = h.state.store.list_devices().unwrap();
    assert_eq!(listed[0].login.as_deref(), Some(support::fake_jmap::USER));
    assert_eq!(listed[0].version, Some(161));
}

#[tokio::test]
async fn invitations_to_an_alias_are_the_users_own() {
    use tolk::wbxml::cp::settings as st;
    let h = Harness::start().await;
    h.jmap.change(|m| {
        m.insert(
            "Identity",
            json!({ "name": "Test", "email": "alias@example.com" }),
        );
        m.insert(
            "CalendarEvent",
            json!({
                "@type": "Event", "uid": "invite@example.org", "calendarIds": { "cal1": true },
                "title": "Planning", "start": "2026-11-12T10:00:00", "timeZone": "Europe/Amsterdam",
                "duration": "PT1H",
                "organizerCalendarAddress": "mailto:boss@example.org",
                "participants": {
                    "o": { "@type": "Participant", "calendarAddress": "mailto:boss@example.org",
                           "roles": { "owner": true } },
                    "me": { "@type": "Participant", "calendarAddress": "mailto:alias@example.com",
                            "roles": { "attendee": true }, "participationStatus": "accepted" },
                },
            }),
        )
    });
    let (device, folders) = h.ready_device("d1").await;
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&calendar).await;
    let data = initial.adds[0].find(airsync::ApplicationData).unwrap();
    assert_eq!(
        data.child_text(cal::ResponseType),
        Some("3"),
        "accepted, as the alias"
    );

    let get = Element::new(st::Settings)
        .child(Element::new(st::UserInformation).child(Element::new(st::Get)));
    let reply = device.send("Settings", Some(get)).await;
    let addresses: Vec<String> = reply
        .root()
        .find(st::UserInformation)
        .and_then(|u| u.find(st::Get))
        .and_then(|g| g.find(st::Accounts))
        .and_then(|a| a.find(st::Account))
        .and_then(|a| a.find(st::EmailAddresses))
        .map(|e| {
            e.elements()
                .filter_map(|x| x.text().map(str::to_owned))
                .collect()
        })
        .unwrap();
    assert_eq!(addresses[0], support::fake_jmap::USER, "primary first");
    assert!(addresses.contains(&"alias@example.com".to_owned()));
}

#[tokio::test]
async fn accounts_have_a_device_limit() {
    let h = Harness::start().await;
    for i in 0..20 {
        let mut device = h.device(&format!("d{i}"));
        device.provision().await;
    }
    let mut extra = h.device("one-too-many");
    let reply = extra
        .send(
            "Provision",
            Some(Element::new(tolk::wbxml::cp::provision::Provision)),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Forgetting an old device makes room.
    h.state
        .store
        .forget_device(support::fake_jmap::ACCOUNT, "d0")
        .unwrap();
    extra.provision().await;
    assert_ne!(extra.policy_key, "0");
}

#[tokio::test]
async fn bodies_are_not_read_before_the_login_is_checked() {
    let h = Harness::start().await;
    let http = tolk::http_client().build().unwrap();
    let url = format!(
        "{}/Microsoft-Server-ActiveSync?Cmd=FolderSync&User=x&DeviceId=d1&DeviceType=T",
        h.url
    );
    let big = vec![0u8; 8 * 1024 * 1024];
    // The server answers without reading the body and may close the
    // connection while the client is still sending: both mean refused.
    let refused =
        |result: Result<reqwest::Response, reqwest::Error>, expected: StatusCode| match result {
            Ok(response) => assert_eq!(response.status(), expected),
            Err(e) => assert!(e.is_request(), "{e}"),
        };
    // No credentials.
    refused(
        http.post(&url).body(big.clone()).send().await,
        StatusCode::UNAUTHORIZED,
    );
    // Credentials, but a body over the command's limit.
    let too_big = http
        .post(&url)
        .basic_auth(support::fake_jmap::USER, Some(support::fake_jmap::PASSWORD))
        .body(big)
        .send()
        .await;
    refused(too_big, StatusCode::PAYLOAD_TOO_LARGE);
}

/// `tolk devices ...` on the harness's state database.
fn devices_cli(h: &Harness, args: &[&str]) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tolk"))
        .arg("devices")
        .args(args)
        .env("STATE_DB", h.dir.path().join("state.sqlite"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "devices {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn admin_resync_makes_the_device_start_over() {
    let h = Harness::start().await;
    h.jmap.add_email("inbox", "Hello", "Hi");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let contacts = folders.id("Contacts");
    let mail = device.initial_sync(&inbox).await;
    let cards = device.initial_sync(&contacts).await;

    let shown = devices_cli(&h, &["show", support::fake_jmap::USER, "d1"]);
    assert!(shown.contains("FOLDERSYNC   SyncKey"), "{shown}");
    assert!(
        shown
            .lines()
            .any(|l| l.starts_with(&format!("{inbox} ")) && l.contains(" 1 ")),
        "{shown}"
    );
    assert!(shown.contains("No quarantined items."), "{shown}");

    // One collection: its next Sync is told the key is invalid.
    let out = devices_cli(&h, &["resync", support::fake_jmap::USER, "d1", &inbox]);
    assert!(out.contains("starts over"), "{out}");
    assert_eq!(device.sync(&inbox, &mail.key, None).await.status, "3");
    assert_eq!(device.sync(&contacts, &cards.key, None).await.status, "1");
    let again = device.initial_sync(&inbox).await;
    assert_eq!(again.adds.len(), 1);

    // Everything: FolderSync gets status 9, then the device starts over.
    devices_cli(&h, &["resync", support::fake_jmap::USER, "d1"]);
    let reply = device
        .send(
            "FolderSync",
            Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, folders.key.clone()))),
        )
        .await;
    assert_eq!(reply.root().child_text(fh::Status), Some("9"));
    assert_eq!(device.sync(&contacts, &cards.key, None).await.status, "3");
    let refreshed = device.folder_sync().await;
    assert_eq!(refreshed.id("Inbox"), inbox, "folder ids stay");
    assert_eq!(device.initial_sync(&inbox).await.adds.len(), 1);
    // The device itself is kept.
    assert!(devices_cli(&h, &["list"]).contains("d1"));
}

#[tokio::test]
async fn admin_clears_the_quarantine() {
    let h = Harness::start().await;
    let id = h.jmap.add_email("inbox", "Broken", "Hi");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    h.state
        .store
        .quarantine(support::fake_jmap::ACCOUNT, "d1", &inbox, &id)
        .unwrap();
    h.jmap.add_email("inbox", "Fine", "Hi");
    let primed = device.sync(&inbox, "0", None).await;
    let first = device.sync(&inbox, &primed.key, None).await;
    assert_eq!(subjects(&first), ["Fine"]);

    let shown = devices_cli(&h, &["show", support::fake_jmap::ACCOUNT, "d1"]);
    assert!(shown.contains(&id), "{shown}");
    let out = devices_cli(&h, &["clear-quarantine", support::fake_jmap::USER, "d1"]);
    assert!(out.contains("1 quarantined"), "{out}");
    // Sent at the next Sync.
    let next = device.sync(&inbox, &first.key, None).await;
    assert_eq!(subjects(&next), ["Broken"]);
}

fn caps(caps: tolk::config::Caps) -> tolk::config::Tuning {
    tolk::config::Tuning {
        caps,
        ..Default::default()
    }
}

#[tokio::test]
async fn sync_max_age_limits_even_unfiltered_syncs() {
    let h = Harness::start_tuned(caps(tolk::config::Caps {
        sync_max_age_days: Some(30),
        ..Default::default()
    }))
    .await;
    let recent = h.jmap.add_email("inbox", "Recent", "Hi");
    let old = h.jmap.add_email("inbox", "Old", "Hi");
    let now = chrono::Utc::now();
    h.jmap.change(|m| {
        for (id, age) in [(&recent, 1), (&old, 60)] {
            let at = (now - chrono::Duration::days(age))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            m.patch(
                "Email",
                id,
                json!({ "receivedAt": at }).as_object().unwrap(),
            );
        }
    });
    let (device, folders) = h.ready_device("d1").await;
    // No FilterType: everything, as far as the cap allows.
    let synced = device.initial_sync(&folders.id("Inbox")).await;
    assert_eq!(subjects(&synced), ["Recent"]);
}

#[tokio::test]
async fn large_contact_photos_stay_on_the_server() {
    let h = Harness::start_tuned(caps(tolk::config::Caps {
        contact_picture_max_bytes: 1024,
        ..Default::default()
    }))
    .await;
    let photo = |bytes: usize| format!("data:image/jpeg;base64,{}", "A".repeat(bytes / 3 * 4));
    h.jmap.change(|m| {
        for (name, bytes) in [("Small", 900), ("Large", 4000)] {
            m.insert(
                "ContactCard",
                json!({
                    "@type": "Card",
                    "addressBookIds": { "ab1": true },
                    "name": { "full": name },
                    "media": { "m1": { "kind": "photo", "uri": photo(bytes) } },
                }),
            );
        }
    });
    let (device, folders) = h.ready_device("d1").await;
    let synced = device.initial_sync(&folders.id("Contacts")).await;
    for add in &synced.adds {
        let data = add.find(airsync::ApplicationData).unwrap();
        let has_photo = data.child_text(ct::Picture).is_some();
        match data.child_text(ct::FileAs) {
            Some("Small") => assert!(has_photo),
            Some("Large") => assert!(!has_photo),
            other => panic!("{other:?}"),
        }
    }
    // Edits from the device leave the large photo alone.
    let large = synced
        .adds
        .iter()
        .find(|a| {
            a.find(airsync::ApplicationData)
                .and_then(|d| d.child_text(ct::FileAs))
                == Some("Large")
        })
        .unwrap();
    let server_id = large.child_text(airsync::ServerId).unwrap();
    let change = contact_change(
        server_id,
        large.find(airsync::ApplicationData).unwrap(),
        |d| {
            d.push(text(ct::Picture, ""));
            d.push(text(ct::JobTitle, "Engineer"));
        },
    );
    assert_eq!(
        device
            .sync(&folders.id("Contacts"), &synced.key, Some(change))
            .await
            .status,
        "1"
    );
    h.jmap.read(|m| {
        let card = m
            .all("ContactCard")
            .into_iter()
            .find(|c| c["name"]["full"] == "Large")
            .unwrap();
        assert_eq!(card["media"]["m1"]["uri"], photo(4000));
        assert_eq!(card["titles"].as_object().unwrap().len(), 1);
    });
}

#[tokio::test]
async fn search_results_follow_the_configured_cap() {
    use tolk::wbxml::cp::search as sp;
    let h = Harness::start_tuned(caps(tolk::config::Caps {
        search_max_results: 2,
        ..Default::default()
    }))
    .await;
    for i in 0..5 {
        h.jmap.add_email("inbox", &format!("Invoice {i}"), "Pay");
    }
    let (device, _) = h.ready_device("d1").await;
    let body = Element::new(sp::Search).child(
        Element::new(sp::Store)
            .child(text(sp::Name, "Mailbox"))
            .child(
                Element::new(sp::Query).child(
                    Element::new(sp::And)
                        .child(text(airsync::Class, "Email"))
                        .child(text(sp::FreeText, "invoice")),
                ),
            )
            .child(Element::new(sp::Options).child(text(sp::Range, "0-9"))),
    );
    let reply = device.send("Search", Some(body)).await;
    let store = reply
        .root()
        .find(sp::Response)
        .and_then(|r| r.find(sp::Store))
        .unwrap();
    assert_eq!(store.find_all(sp::Result).count(), 2);
    assert_eq!(store.child_text(sp::Range), Some("0-1"));
    assert_eq!(store.child_text(sp::Total), Some("5"));
}

#[tokio::test]
async fn find_searches_mail_with_subfolders_and_the_gal() {
    use tolk::wbxml::cp::{find as fp, item_operations as io};
    let h = Harness::start().await;
    h.jmap.change(|m| {
        m.insert(
            "Mailbox",
            json!({ "id": "projects", "name": "Projects", "parentId": "inbox", "role": null, "sortOrder": 5 }),
        )
    });
    h.jmap.add_email("inbox", "Invoice one", "Pay");
    h.jmap.add_email("projects", "Invoice two", "Pay");
    h.jmap.add_email("sent", "Invoice three", "Pay");
    h.jmap.add_email("inbox", "Lunch", "Eat");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let find = |criterion: Element| {
        Element::new(fp::Find)
            .child(text(fp::SearchId, "6e0f6d0a-1d2b-4c3d-8e4f-5a6b7c8d9e0f"))
            .child(Element::new(fp::ExecuteSearch).child(criterion))
    };
    let mail = |deep: bool, range: &str| {
        let mut options = Element::new(fp::Options).child(text(fp::Range, range));
        if deep {
            options.push(Element::new(fp::DeepTraversal));
        }
        find(
            Element::new(fp::MailBoxSearchCriterion)
                .child(
                    Element::new(fp::Query)
                        .child(text(airsync::Class, "Email"))
                        .child(text(airsync::CollectionId, &inbox))
                        .child(text(fp::FreeText, "invoice")),
                )
                .child(options),
        )
    };

    let reply = device.send("Find", Some(mail(true, "0-9"))).await;
    let root = reply.root();
    assert_eq!(root.child_text(fp::Status), Some("1"));
    let response = root.find(fp::Response).unwrap();
    assert_eq!(response.child_text(io::Store), Some("Mailbox"));
    assert_eq!(response.child_text(fp::Total), Some("2"));
    assert_eq!(response.child_text(fp::Range), Some("0-1"));
    let result = response.find(fp::Result).unwrap();
    assert!(result.child_text(airsync::ServerId).unwrap().contains(':'));
    let properties = result.find(fp::Properties).unwrap();
    assert!(
        properties
            .child_text(em::Subject)
            .unwrap()
            .starts_with("Invoice")
    );
    assert_eq!(properties.child_text(fp::HasAttachments), Some("0"));

    let reply = device.send("Find", Some(mail(false, "0-9"))).await;
    let response = reply.root().find(fp::Response).unwrap();
    assert_eq!(response.child_text(fp::Total), Some("1"));

    // Ranges must start at 0.
    let reply = device.send("Find", Some(mail(false, "5-9"))).await;
    let response = reply.root().find(fp::Response).unwrap();
    assert_eq!(response.child_text(fp::Status), Some("4"));

    // GAL queries need four characters.
    let gal = |q: &str| find(Element::new(fp::GalSearchCriterion).child(text(fp::Query, q)));
    let reply = device.send("Find", Some(gal("te"))).await;
    let response = reply.root().find(fp::Response).unwrap();
    assert_eq!(response.child_text(fp::Status), Some("2"));
    let reply = device.send("Find", Some(gal("test"))).await;
    let response = reply.root().find(fp::Response).unwrap();
    assert_eq!(response.child_text(io::Store), Some("GAL"));
    assert_eq!(response.child_text(fp::Status), Some("1"));
}

/// The folder of a shared account's address book (type 14) or calendar (13).
fn shared_folder(folders: &support::Folders, folder_type: &str) -> String {
    let name = format!("Team ({})", support::fake_jmap::SHARED_OWNER);
    folders
        .by_name
        .iter()
        .find(|(n, _, t)| *n == name && t == folder_type)
        .map(|(_, id, _)| id.clone())
        .unwrap_or_else(|| {
            panic!(
                "no shared folder of type {folder_type}: {:?}",
                folders.by_name
            )
        })
}

fn shared_card(m: &mut support::fake_jmap::Model) -> String {
    m.insert(
        "ContactCard",
        json!({
            "@type": "Card",
            "addressBookIds": { "sab1": true },
            "name": { "components": [{ "kind": "given", "value": "Colleague" }] },
        }),
    )
}

#[tokio::test]
async fn shared_calendars_and_address_books_reach_the_device() {
    let h = Harness::start().await;
    h.jmap.share(false);
    let card = h.jmap.change_shared(shared_card);
    h.jmap.change_shared(|m| {
        m.insert(
            "CalendarEvent",
            json!({
                "@type": "Event",
                "uid": "team1@example.org",
                "calendarIds": { "scal1": true },
                "title": "Team meeting",
                "start": "2026-11-03T10:00:00",
                "timeZone": "Europe/Amsterdam",
                "duration": "PT1H",
            }),
        )
    });
    let (device, folders) = h.ready_device("d1").await;
    // The user's own folders keep their main types.
    assert_eq!(folders.of_type("9"), folders.id("Contacts"));
    assert_eq!(folders.of_type("8"), folders.id("Calendar"));
    let team_contacts = shared_folder(&folders, "14");
    let team_calendar = shared_folder(&folders, "13");

    let cards = device.initial_sync(&team_contacts).await;
    assert_eq!(cards.adds.len(), 1);
    let events = device.initial_sync(&team_calendar).await;
    assert_eq!(events.adds.len(), 1);
    let own = device.initial_sync(&folders.id("Contacts")).await;
    assert!(own.adds.is_empty(), "the shared card stays in its folder");

    // Changes from the device go to the shared account.
    let server_id = cards.adds[0].child_text(airsync::ServerId).unwrap();
    let change = contact_change(
        server_id,
        cards.adds[0].find(airsync::ApplicationData).unwrap(),
        |d| d.push(text(ct::JobTitle, "Engineer")),
    );
    let changed = device.sync(&team_contacts, &cards.key, Some(change)).await;
    assert!(changed.responses.is_empty(), "{:?}", changed.responses);
    h.jmap.read_shared(|m| {
        let card = m.get("ContactCard", &card).unwrap();
        assert_eq!(card["titles"].as_object().unwrap().len(), 1, "{card}");
    });
    let add = Element::new(airsync::Commands).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "ev-1"))
            .child(
                Element::new(airsync::ApplicationData)
                    .child(text(cal::Subject, "Planning"))
                    .child(text(cal::StartTime, "20261104T090000Z"))
                    .child(text(cal::EndTime, "20261104T100000Z")),
            ),
    );
    let added = device.sync(&team_calendar, &events.key, Some(add)).await;
    assert_eq!(added.responses[0].2, "1", "{:?}", added.responses);
    h.jmap
        .read_shared(|m| assert_eq!(m.all("CalendarEvent").len(), 2));
    h.jmap.read(|m| assert!(m.all("CalendarEvent").is_empty()));

    // A change in the shared account wakes a Ping on its folder.
    let jmap = &h.jmap;
    let watched = [(team_contacts.as_str(), "Contacts")];
    let (ping, _) = tokio::join!(device.ping(60, &watched), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        jmap.change_shared(shared_card);
    });
    assert_eq!(ping, ("2".to_owned(), vec![team_contacts.clone()]));
    let next = device.sync(&team_contacts, &changed.key, None).await;
    assert_eq!(next.adds.len(), 1);

    // Shared folders cannot be renamed from the phone.
    let reply = folder_op(
        &device,
        "FolderUpdate",
        vec![
            text(fh::SyncKey, folders.key.clone()),
            text(fh::ServerId, team_calendar.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "Mine now"),
        ],
    )
    .await;
    assert_eq!(reply.child_text(fh::Status), Some("3"));
}

#[tokio::test]
async fn shared_mailboxes_sync_fetch_and_move() {
    use tolk::wbxml::cp::item_operations as io;
    let h = Harness::start().await;
    h.jmap.share(false);
    h.jmap
        .add_shared_email("inbox", "Team news", "For everyone");
    h.jmap.add_shared_email("sproj", "Project plan", "Deep");
    let (device, folders) = h.ready_device("d1").await;
    let shared_inbox = folders.id(&format!("Inbox ({})", support::fake_jmap::SHARED_OWNER));
    let projects = folders.id("Projects");
    // The user's own Inbox stays the device's Inbox; shared ones are plain.
    assert_eq!(folders.of_type("2"), folders.id("Inbox"));
    assert!(
        folders
            .by_name
            .iter()
            .any(|(_, id, t)| *id == shared_inbox && t == "12")
    );

    // The user's own Inbox has the same JMAP id; its view must stay apart.
    h.jmap.add_email("inbox", "Mine", "Own mail");
    let own_first = device.initial_sync(&folders.id("Inbox")).await;
    assert_eq!(own_first.adds.len(), 1);
    let initial = device.initial_sync(&shared_inbox).await;
    assert_eq!(initial.adds.len(), 1);
    let data = initial.adds[0].find(airsync::ApplicationData).unwrap();
    assert_eq!(data.child_text(em::Subject), Some("Team news"));
    let server_id = initial.add_ids()[0].clone();
    assert_eq!(device.initial_sync(&projects).await.adds.len(), 1);

    // A Ping on the shared folder waits while nothing changes.
    let synced = device.sync(&shared_inbox, &initial.key, None).await;
    assert!(synced.adds.is_empty());
    let watched = [(shared_inbox.as_str(), "Email")];
    let waiting = tokio::time::timeout(Duration::from_secs(2), device.ping(60, &watched)).await;
    assert!(waiting.is_err(), "answered at once: {waiting:?}");

    // Fetched from the shared account.
    let fetch = Element::new(io::ItemOperations).child(
        Element::new(io::Fetch)
            .child(text(io::Store, "Mailbox"))
            .child(text(airsync::CollectionId, &shared_inbox))
            .child(text(airsync::ServerId, &server_id)),
    );
    let reply = device.send("ItemOperations", Some(fetch)).await;
    let status = reply
        .root()
        .find(io::Response)
        .and_then(|r| r.find(io::Fetch))
        .and_then(|f| f.child_text(io::Status))
        .map(str::to_owned);
    assert_eq!(status.as_deref(), Some("1"));

    // Moved into the user's own Inbox: copied over, gone from the share.
    let inbox = folders.id("Inbox");
    let reply = device
        .send(
            "MoveItems",
            Some(
                Element::new(mv::MoveItems).child(
                    Element::new(mv::Move)
                        .child(text(mv::SrcMsgId, server_id.clone()))
                        .child(text(mv::SrcFldId, shared_inbox.clone()))
                        .child(text(mv::DstFldId, inbox.clone())),
                ),
            ),
        )
        .await;
    let response = reply.root().find(mv::Response).unwrap();
    assert_eq!(response.child_text(mv::Status), Some("3"));
    assert!(
        response
            .child_text(mv::DstMsgId)
            .unwrap()
            .starts_with(&format!("{inbox}:"))
    );
    h.jmap.read(|m| {
        let mut subjects: Vec<String> = m
            .all("Email")
            .iter()
            .map(|e| e["subject"].as_str().unwrap_or_default().to_owned())
            .collect();
        subjects.sort();
        assert_eq!(subjects, vec!["Mine", "Team news"]);
    });
    h.jmap.read_shared(|m| assert_eq!(m.all("Email").len(), 1));
    let source = device.sync(&shared_inbox, &initial.key, None).await;
    assert!(
        source.adds.is_empty() && source.deletes.is_empty(),
        "already gone"
    );
    let own = device.sync(&inbox, &own_first.key, None).await;
    assert!(own.adds.is_empty(), "the move already recorded it");
    assert_eq!(device.initial_sync(&inbox).await.adds.len(), 2);

    // Someone else's folders are not renamed or deleted from the phone.
    let rename = folder_op(
        &device,
        "FolderUpdate",
        vec![
            text(fh::SyncKey, folders.key.clone()),
            text(fh::ServerId, projects.clone()),
            text(fh::ParentId, "0"),
            text(fh::DisplayName, "Mine now"),
        ],
    )
    .await;
    assert_eq!(rename.child_text(fh::Status), Some("3"));
}

#[tokio::test]
async fn read_only_shared_folders_refuse_device_changes() {
    let h = Harness::start().await;
    h.jmap.share(true);
    let card = h.jmap.change_shared(shared_card);
    let (device, folders) = h.ready_device("d1").await;
    let team_contacts = shared_folder(&folders, "14");
    let cards = device.initial_sync(&team_contacts).await;
    let server_id = cards.adds[0].child_text(airsync::ServerId).unwrap();
    let data = cards.adds[0].find(airsync::ApplicationData).unwrap();

    let commands = contact_change(server_id, data, |d| d.push(text(ct::JobTitle, "Boss"))).child(
        Element::new(airsync::Add)
            .child(text(airsync::ClientId, "c-1"))
            .child(Element::new(airsync::ApplicationData).child(text(ct::FirstName, "New"))),
    );
    let result = device
        .sync(&team_contacts, &cards.key, Some(commands))
        .await;
    assert_eq!(result.status, "1");
    let statuses: Vec<(&str, &str)> = result
        .responses
        .iter()
        .map(|(kind, _, status, _)| (kind.as_str(), status.as_str()))
        .collect();
    assert_eq!(statuses, [("Change", "7"), ("Add", "6")]);
    // The device gets the server's version back.
    assert_eq!(result.changes.len(), 1);

    let delete = Element::new(airsync::Commands)
        .child(Element::new(airsync::Delete).child(text(airsync::ServerId, server_id)));
    let deleted = device.sync(&team_contacts, &result.key, Some(delete)).await;
    assert_eq!(deleted.responses[0].2, "7");
    assert_eq!(deleted.add_ids(), [server_id.to_owned()], "sent again");
    h.jmap.read_shared(|m| {
        assert_eq!(m.all("ContactCard").len(), 1);
        assert!(m.get("ContactCard", &card).unwrap()["titles"].is_null());
    });
}

#[tokio::test]
async fn shared_accounts_may_offer_only_some_kinds() {
    let h = Harness::start().await;
    h.jmap.share(true);
    h.jmap
        .change_shared(|m| m.unsupported.push("AddressBook".to_owned()));
    let (_, folders) = h.ready_device("d1").await;
    shared_folder(&folders, "13");
    let name = format!("Team ({})", support::fake_jmap::SHARED_OWNER);
    assert!(
        !folders
            .by_name
            .iter()
            .any(|(n, _, t)| *n == name && t == "14"),
        "{:?}",
        folders.by_name
    );
}

fn estimate_request(collection: &str, key: &str) -> Element {
    use tolk::wbxml::cp::item_estimate as ie;
    Element::new(ie::GetItemEstimate).child(
        Element::new(ie::Collections).child(
            Element::new(ie::Collection)
                .child(text(airsync::SyncKey, key))
                .child(text(ie::CollectionId, collection)),
        ),
    )
}

/// (Status, Estimate) of each collection in a GetItemEstimate reply.
fn estimates(reply: &support::Reply) -> Vec<(String, Option<String>)> {
    use tolk::wbxml::cp::item_estimate as ie;
    reply
        .root()
        .find_all(ie::Response)
        .map(|r| {
            (
                r.child_text(ie::Status).unwrap_or_default().to_owned(),
                r.find(ie::Collection)
                    .and_then(|c| c.child_text(ie::Estimate))
                    .map(str::to_owned),
            )
        })
        .collect()
}

#[tokio::test]
async fn item_estimate_counts_pending_changes() {
    let h = Harness::start().await;
    let ids: Vec<String> = (0..3)
        .map(|i| h.jmap.add_email("inbox", &format!("Message {i}"), "Hello"))
        .collect();
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");

    // Before priming, and with an unknown folder.
    let reply = device
        .send("GetItemEstimate", Some(estimate_request(&inbox, "0")))
        .await;
    assert_eq!(estimates(&reply), [("3".to_owned(), None)], "not primed");
    let reply = device
        .send("GetItemEstimate", Some(estimate_request("999", "1")))
        .await;
    assert_eq!(
        estimates(&reply),
        [("2".to_owned(), None)],
        "unknown folder"
    );

    let primed = device.sync(&inbox, "0", None).await;
    let reply = device
        .send(
            "GetItemEstimate",
            Some(estimate_request(&inbox, &primed.key)),
        )
        .await;
    assert_eq!(estimates(&reply), [("1".to_owned(), Some("3".to_owned()))]);

    // After a full sync: one new message and one deleted.
    let synced = device.sync(&inbox, &primed.key, None).await;
    h.jmap.add_email("inbox", "Message 3", "New");
    h.jmap.change(|m| m.remove("Email", &ids[0]));
    let reply = device
        .send(
            "GetItemEstimate",
            Some(estimate_request(&inbox, &synced.key)),
        )
        .await;
    assert_eq!(estimates(&reply), [("1".to_owned(), Some("2".to_owned()))]);

    // The estimate changes nothing: the Sync still delivers both.
    let next = device.sync(&inbox, &synced.key, None).await;
    assert_eq!((next.adds.len(), next.deletes.len()), (1, 1));

    let reply = device
        .send("GetItemEstimate", Some(estimate_request(&inbox, "77")))
        .await;
    assert_eq!(estimates(&reply), [("4".to_owned(), None)], "unknown key");
}

fn meeting_response(collection: &str, request_id: &str, answer: &str) -> Element {
    use tolk::wbxml::cp::meeting_response as mr;
    Element::new(mr::MeetingResponse).child(
        Element::new(mr::Request)
            .child(text(mr::UserResponse, answer))
            .child(text(mr::CollectionId, collection))
            .child(text(mr::RequestId, request_id)),
    )
}

/// (Status, CalendarId) of the first result of a MeetingResponse reply.
fn meeting_result(reply: &support::Reply) -> (String, Option<String>) {
    use tolk::wbxml::cp::meeting_response as mr;
    let result = reply.root().find(mr::Result).expect("Result");
    (
        result.child_text(mr::Status).unwrap_or_default().to_owned(),
        result.child_text(mr::CalendarId).map(str::to_owned),
    )
}

fn invitation(uid: &str, status: &str) -> serde_json::Value {
    json!({
        "@type": "Event", "uid": uid, "title": "Planning",
        "start": "2026-11-12T10:00:00", "timeZone": "Europe/Amsterdam", "duration": "PT1H",
        "organizerCalendarAddress": "mailto:boss@example.org",
        "participants": {
            "o": { "@type": "Participant", "calendarAddress": "mailto:boss@example.org",
                   "roles": { "owner": true } },
            "me": { "@type": "Participant",
                    "calendarAddress": format!("mailto:{}", support::fake_jmap::USER),
                    "roles": { "attendee": true }, "participationStatus": status },
        },
    })
}

#[tokio::test]
async fn meeting_responses_from_the_calendar_set_the_own_status() {
    let h = Harness::start().await;
    let event = h.jmap.change(|m| {
        let mut event = invitation("plan@example.org", "needs-action");
        event["calendarIds"] = json!({ "cal1": true });
        m.insert("CalendarEvent", event)
    });
    let (device, folders) = h.ready_device("d1").await;
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&calendar).await;
    let server_id = initial.add_ids()[0].clone();
    let status = |h: &Harness| {
        h.jmap.read(|m| {
            m.get("CalendarEvent", &event).unwrap()["participants"]["me"]["participationStatus"]
                .clone()
        })
    };

    let reply = device
        .send(
            "MeetingResponse",
            Some(meeting_response(&calendar, &server_id, "2")),
        )
        .await;
    assert_eq!(
        meeting_result(&reply),
        ("1".to_owned(), Some(server_id.clone()))
    );
    assert_eq!(status(&h), "tentative");

    // Declining answers without a calendar item.
    let reply = device
        .send(
            "MeetingResponse",
            Some(meeting_response(&calendar, &server_id, "3")),
        )
        .await;
    assert_eq!(meeting_result(&reply), ("1".to_owned(), None));
    assert_eq!(status(&h), "declined");

    // Unknown answers and items are invalid requests.
    for (id, answer) in [(server_id.as_str(), "9"), ("999:1", "1")] {
        let reply = device
            .send(
                "MeetingResponse",
                Some(meeting_response(&calendar, id, answer)),
            )
            .await;
        assert_eq!(meeting_result(&reply).0, "2", "{id} {answer}");
    }
}

#[tokio::test]
async fn meeting_responses_to_an_invitation_email_place_the_event() {
    let h = Harness::start().await;
    let email = h.jmap.add_email("inbox", "Invitation: Planning", "Join us");
    h.jmap.change(|m| {
        m.patch(
            "Email",
            &email,
            json!({ "attachments": [{
                "partId": "2", "blobId": "blob-ics", "size": 400, "name": "invite.ics",
                "type": "text/calendar", "cid": null, "disposition": "attachment",
            }] })
            .as_object()
            .unwrap(),
        );
        m.parsed.insert(
            "blob-ics".to_owned(),
            invitation("new@example.org", "needs-action"),
        );
    });
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let calendar = folders.id("Calendar");
    let initial = device.initial_sync(&inbox).await;
    let mail_id = initial.add_ids()[0].clone();

    // The server has not put the invitation in a calendar: accepting adds it.
    let reply = device
        .send(
            "MeetingResponse",
            Some(meeting_response(&inbox, &mail_id, "1")),
        )
        .await;
    let (status, calendar_id) = meeting_result(&reply);
    assert_eq!(status, "1");
    assert!(calendar_id.unwrap().starts_with(&format!("{calendar}:")));
    let placed = h.jmap.read(|m| {
        let events = m.all("CalendarEvent");
        assert_eq!(events.len(), 1);
        events[0].clone()
    });
    assert_eq!(placed["calendarIds"], json!({ "cal1": true }));
    assert_eq!(
        placed["participants"]["me"]["participationStatus"],
        "accepted"
    );

    // Answering again finds the placed event by its UID.
    let reply = device
        .send(
            "MeetingResponse",
            Some(meeting_response(&inbox, &mail_id, "3")),
        )
        .await;
    assert_eq!(meeting_result(&reply), ("1".to_owned(), None));
    h.jmap.read(|m| {
        let events = m.all("CalendarEvent");
        assert_eq!(events.len(), 1, "not placed twice");
        assert_eq!(
            events[0]["participants"]["me"]["participationStatus"],
            "declined"
        );
    });

    // A mail without an invitation is an invalid request.
    h.jmap.add_email("inbox", "Lunch?", "No invitation");
    let plain_id = device.sync(&inbox, &initial.key, None).await.add_ids()[0].clone();
    let reply = device
        .send(
            "MeetingResponse",
            Some(meeting_response(&inbox, &plain_id, "1")),
        )
        .await;
    assert_eq!(meeting_result(&reply).0, "2");
}

fn smart(
    root: tolk::wbxml::Tag,
    client_id: &str,
    source: Option<(&str, &str)>,
    mime: &str,
) -> Element {
    let mut request = Element::new(root).child(text(cm::ClientId, client_id));
    if let Some((folder, item)) = source {
        request.push(
            Element::new(cm::Source)
                .child(text(cm::FolderId, folder))
                .child(text(cm::ItemId, item)),
        );
    }
    request
        .child(Element::new(cm::SaveInSentItems))
        .child(Element::with_opaque(cm::Mime, mime.as_bytes()))
}

/// The status of a failed compose command; `None` for success.
fn compose_status(reply: &support::Reply) -> Option<String> {
    assert!(reply.status.is_success());
    reply.body.as_ref().map(|_| {
        reply
            .root()
            .child_text(cm::Status)
            .unwrap_or_default()
            .to_owned()
    })
}

/// The raw message of the only email in Sent.
fn sent_message(h: &Harness) -> Vec<u8> {
    h.jmap.read(|m| {
        let sent: Vec<_> = m
            .all("Email")
            .into_iter()
            .filter(|e| e["mailboxIds"]["sent"] == true)
            .collect();
        assert_eq!(sent.len(), 1, "one message in Sent");
        m.blobs[sent[0]["blobId"].as_str().unwrap()].0.clone()
    })
}

#[tokio::test]
async fn smart_reply_quotes_the_original_and_marks_it_answered() {
    let h = Harness::start().await;
    let original = h
        .jmap
        .add_email("inbox", "Plans", "Shall we meet on Friday?");
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let item = device.initial_sync(&inbox).await.add_ids()[0].clone();
    let mime = "From: test@example.com\r\nTo: sender@example.org\r\nSubject: Re: Plans\r\n\r\nFriday works.\r\n";

    let reply = device
        .send(
            "SmartReply",
            Some(smart(cm::SmartReply, "r-1", Some((&inbox, &item)), mime)),
        )
        .await;
    assert_eq!(compose_status(&reply), None);

    let raw = sent_message(&h);
    let sent = mail_parser::MessageParser::default().parse(&raw).unwrap();
    let body = sent.body_text(0).unwrap();
    assert!(body.starts_with("Friday works."), "{body}");
    assert!(body.contains("> Shall we meet on Friday?"), "{body}");
    let original_id = h.jmap.read(|m| {
        let email = m.get("Email", &original).unwrap();
        assert_eq!(email["keywords"]["$answered"], true);
        email["messageId"][0].as_str().unwrap().to_owned()
    });
    let headers = String::from_utf8_lossy(&raw);
    assert!(
        headers.contains(&format!("In-Reply-To: <{original_id}>")),
        "{headers}"
    );
    h.jmap.read(|m| {
        assert_eq!(
            m.submissions[0]["envelope"]["rcptTo"],
            json!([{ "email": "sender@example.org" }])
        );
    });

    // A reply to a message that does not exist is not sent.
    let reply = device
        .send(
            "SmartReply",
            Some(smart(cm::SmartReply, "r-2", Some((&inbox, "999")), mime)),
        )
        .await;
    assert_eq!(compose_status(&reply).as_deref(), Some("150"));
    h.jmap.read(|m| assert_eq!(m.submissions.len(), 1));
}

#[tokio::test]
async fn smart_forward_adds_the_original_attachments() {
    let h = Harness::start().await;
    let original = h.jmap.add_email("inbox", "Report", "See the attachment.");
    h.jmap.change(|m| {
        m.blobs.insert(
            "blob-report".to_owned(),
            (b"%PDF-1.7 report".to_vec(), "application/pdf".to_owned()),
        );
        m.patch(
            "Email",
            &original,
            json!({ "attachments": [{
                "partId": "2", "blobId": "blob-report", "size": 15, "name": "report.pdf",
                "type": "application/pdf", "cid": null, "disposition": "attachment",
            }] })
            .as_object()
            .unwrap(),
        );
    });
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let item = device.initial_sync(&inbox).await.add_ids()[0].clone();
    let mime = "From: test@example.com\r\nTo: colleague@example.org\r\nSubject: Fwd: Report\r\n\r\nFor you.\r\n";

    let reply = device
        .send(
            "SmartForward",
            Some(smart(cm::SmartForward, "f-1", Some((&inbox, &item)), mime)),
        )
        .await;
    assert_eq!(compose_status(&reply), None);

    let raw = sent_message(&h);
    let sent = mail_parser::MessageParser::default().parse(&raw).unwrap();
    let body = sent.body_text(0).unwrap();
    assert!(
        body.contains("---------- Forwarded message ---------"),
        "{body}"
    );
    assert!(body.contains("See the attachment."), "{body}");
    let attachment = sent
        .attachments()
        .next()
        .expect("the original's attachment");
    assert_eq!(attachment.attachment_name(), Some("report.pdf"));
    assert_eq!(&*attachment.decoded(), b"%PDF-1.7 report");
    h.jmap.read(|m| {
        assert_eq!(
            m.get("Email", &original).unwrap()["keywords"]["$forwarded"],
            true
        );
    });
}

#[tokio::test]
async fn compose_rejects_unusable_messages() {
    let h = Harness::start().await;
    let (device, _) = h.ready_device("d1").await;
    let cases = [
        (
            "no recipient",
            "From: test@example.com\r\nSubject: Hi\r\n\r\nHello\r\n",
            "119",
        ),
        ("no MIME", "", "107"),
    ];
    for (name, mime, expected) in cases {
        let mut request = Element::new(cm::SendMail).child(text(cm::ClientId, name));
        if !mime.is_empty() {
            request.push(Element::with_opaque(cm::Mime, mime.as_bytes()));
        }
        let reply = device.send("SendMail", Some(request)).await;
        assert_eq!(compose_status(&reply).as_deref(), Some(expected), "{name}");
    }

    // A failed message can be sent again with the same ClientId.
    let mime = "From: test@example.com\r\nTo: friend@example.org\r\nSubject: Hi\r\n\r\nHello\r\n";
    let reply = device
        .send(
            "SendMail",
            Some(smart(cm::SendMail, "no recipient", None, mime)),
        )
        .await;
    assert_eq!(compose_status(&reply), None);

    // A failed submission is reported.
    h.jmap
        .fail_method("EmailSubmission/set", "forbiddenFrom", 1);
    let reply = device
        .send("SendMail", Some(smart(cm::SendMail, "c-2", None, mime)))
        .await;
    assert_eq!(compose_status(&reply).as_deref(), Some("120"));
}

#[tokio::test]
async fn whole_messages_stream_into_item_operations() {
    use tolk::wbxml::cp::{airsyncbase as base, item_operations as io};
    let h = Harness::start().await;
    let email = h.jmap.add_email("inbox", "Raw", "Body text");
    // A raw message with a non-UTF-8 byte and a NUL, which a WBXML string
    // cannot carry.
    let raw = h.jmap.change(|m| {
        let blob = m.get("Email", &email).unwrap()["blobId"]
            .as_str()
            .unwrap()
            .to_owned();
        let entry = m.blobs.get_mut(&blob).unwrap();
        entry.0.extend_from_slice(b"caf\xE9 \0end\r\n");
        String::from_utf8_lossy(&entry.0).replace('\0', "")
    });
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let server_id = device.initial_sync(&inbox).await.add_ids()[0].clone();
    let fetch = |truncation: Option<&str>| {
        let mut preference = Element::new(base::BodyPreference).child(text(base::Type, "4"));
        if let Some(size) = truncation {
            preference.push(text(base::TruncationSize, size));
        }
        Element::new(io::ItemOperations).child(
            Element::new(io::Fetch)
                .child(text(io::Store, "Mailbox"))
                .child(text(airsync::CollectionId, inbox.clone()))
                .child(text(airsync::ServerId, server_id.clone()))
                .child(Element::new(io::Options).child(preference)),
        )
    };
    let body = |reply: &support::Reply| {
        reply
            .root()
            .find(io::Response)
            .and_then(|r| r.find(io::Fetch))
            .and_then(|f| f.find(io::Properties))
            .and_then(|p| p.find(base::Body))
            .unwrap()
            .clone()
    };

    let reply = device.send("ItemOperations", Some(fetch(None))).await;
    let streamed = body(&reply);
    assert_eq!(streamed.child_text(base::Data), Some(raw.as_str()));
    assert_eq!(streamed.find(base::Truncated), None);

    // A truncated body is cut from the downloaded message instead.
    let reply = device.send("ItemOperations", Some(fetch(Some("10")))).await;
    let cut = body(&reply);
    assert_eq!(cut.child_text(base::Data), Some(&raw[..10]));
    assert_eq!(cut.child_text(base::Truncated), Some("1"));
}

#[tokio::test]
async fn mail_domains_can_have_their_own_jmap_server() {
    let (h, mapped) = Harness::start_mapped().await;
    h.jmap.add_email("inbox", "On the default server", "x");
    mapped.add_email("inbox", "On the mapped server", "x");
    let subjects = |result: &support::SyncResult| -> Vec<String> {
        result
            .adds
            .iter()
            .filter_map(|a| a.find(airsync::ApplicationData)?.child_text(em::Subject))
            .map(str::to_owned)
            .collect()
    };

    // The full address goes to the server of its domain; both servers call
    // their account a1, but the devices' state stays apart.
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");
    let on_mapped = device.initial_sync(&inbox).await;
    assert_eq!(subjects(&on_mapped), ["On the mapped server"]);

    let mut other = h.device("d1");
    other.user = support::fake_jmap::ALIAS.to_owned();
    other.provision().await;
    let folders = other.folder_sync().await;
    let on_default = other.initial_sync(&folders.id("Inbox")).await;
    assert_eq!(subjects(&on_default), ["On the default server"]);

    // Changes on one server do not reach the other's device.
    mapped.add_email("inbox", "Second", "x");
    let next = device.sync(&inbox, &on_mapped.key, None).await;
    assert_eq!(subjects(&next), ["Second"]);
    let idle = other
        .sync(&folders.id("Inbox"), &on_default.key, None)
        .await;
    assert!(idle.adds.is_empty());

    // State of the mapped server's accounts is stored under its host.
    let devices = h.state.store.list_devices().unwrap();
    let mut users: Vec<&str> = devices.iter().map(|d| d.user.as_str()).collect();
    users.sort_unstable();
    let host = mapped.url.trim_start_matches("http://");
    let scoped = format!("{host}/{}", support::fake_jmap::ACCOUNT);
    assert_eq!(users, [scoped.as_str(), support::fake_jmap::ACCOUNT]);
}
