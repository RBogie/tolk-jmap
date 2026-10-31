//! Timing of Sync and Ping on a 50k-message folder. Ignored by
//! default; run with `cargo test --release --test large_mailbox -- --ignored --nocapture`.

mod support;

use std::time::Instant;

use support::Harness;

const MESSAGES: usize = 50_000;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn incremental_sync_of_a_large_folder() {
    let h = Harness::start().await;
    h.jmap.change(|m| {
        for i in 0..MESSAGES {
            m.insert(
                "Email",
                serde_json::json!({
                    "blobId": format!("b{i}"),
                    "mailboxIds": { "inbox": true },
                    "keywords": {},
                    "size": 1000,
                    "receivedAt": format!("2026-{:02}-{:02}T{:02}:{:02}:00Z", 1 + i % 12, 1 + i % 28, i % 24, i % 60),
                    "subject": format!("Message {i}"),
                    "from": [{ "name": "S", "email": "s@example.org" }],
                    "to": null, "cc": null, "replyTo": null, "messageId": null, "references": null,
                    "textBody": [], "htmlBody": [], "attachments": [], "bodyValues": {},
                }),
            );
        }
    });
    let (device, folders) = h.ready_device("d1").await;
    let inbox = folders.id("Inbox");

    let started = Instant::now();
    let primed = device.sync(&inbox, "0", None).await;
    println!("prime: {:?}", started.elapsed());
    let started = Instant::now();
    let mut key = primed.key;
    let mut pages = 0;
    loop {
        let result = device.sync_window(&inbox, &key, None, 512).await;
        key = result.key;
        pages += 1;
        if !result.more {
            break;
        }
    }
    println!(
        "initial download: {pages} pages in {:?} ({:?} per page)",
        started.elapsed(),
        started.elapsed() / pages
    );

    for round in 0..3 {
        h.jmap.add_email("inbox", &format!("New {round}"), "x");
        let started = Instant::now();
        let result = device.sync(&inbox, &key, None).await;
        println!(
            "incremental sync with one new message: {:?}",
            started.elapsed()
        );
        assert_eq!(result.adds.len(), 1);
        key = result.key;
    }
    let started = Instant::now();
    let idle = device.sync(&inbox, &key, None).await;
    println!("incremental sync without changes: {:?}", started.elapsed());
    assert!(idle.adds.is_empty());
}
