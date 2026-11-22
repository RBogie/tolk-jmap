//! Change notifications from the JMAP server, shared per account.
//!
//! The first waiting Ping of an account starts a task that follows the
//! account's EventSource stream; every state change bumps a counter per kind
//! of data that waiters watch. The task stops when the last waiter is gone.

use crate::LockExt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use tokio::{sync::watch, task::JoinHandle};

use crate::jmap::JmapClient;

/// Pause before reconnecting a stream that ended or failed.
const RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// Pings answered at once that make a device count as looping.
const STORM_PINGS: usize = 10;
const STORM_WINDOW: Duration = Duration::from_secs(60);
/// How long the answer to a looping device is held back.
pub const STORM_DELAY: Duration = Duration::from_secs(30);

pub struct PushHub {
    accounts: Mutex<HashMap<String, Weak<AccountPush>>>,
    quick_answers: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// Bumped per device whenever it sends a request, ending its pending Ping.
    devices: Mutex<HashMap<String, watch::Sender<u64>>>,
    shutdown: watch::Sender<bool>,
}

impl Default for PushHub {
    fn default() -> Self {
        Self {
            accounts: Mutex::default(),
            quick_answers: Mutex::default(),
            devices: Mutex::default(),
            shutdown: watch::channel(false).0,
        }
    }
}

/// What a change notification is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mail,
    Contacts,
    Calendar,
    /// Mailboxes, address books or calendars themselves.
    Folders,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Mail, Kind::Contacts, Kind::Calendar, Kind::Folders];

    /// The kinds a JMAP data type's changes affect.
    fn of_type(data_type: &str) -> &'static [Kind] {
        match data_type {
            "Email" | "Thread" => &[Kind::Mail],
            // Mailbox counts change with every message; the hierarchy check
            // is cheap and tells real folder changes apart.
            "Mailbox" => &[Kind::Folders],
            "ContactCard" => &[Kind::Contacts],
            "CalendarEvent" => &[Kind::Calendar],
            "AddressBook" | "Calendar" => &[Kind::Folders],
            _ => &[],
        }
    }
}

/// Change counters per [`Kind`]; they only grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes([u64; 4]);

impl Changes {
    fn bump(&mut self, kind: Kind) {
        self.0[kind as usize] = self.0[kind as usize].wrapping_add(1);
    }

    /// Whether `kind` changed between `earlier` and these counters.
    pub fn changed_since(&self, earlier: &Changes, kind: Kind) -> bool {
        self.0[kind as usize] != earlier.0[kind as usize]
    }

    /// Records a state change of these JMAP data types (`None`: unknown, so
    /// everything may have changed).
    fn record(&mut self, types: Option<&[String]>) {
        match types {
            Some(types) => {
                for kind in Kind::ALL {
                    if types.iter().any(|t| Kind::of_type(t).contains(&kind)) {
                        self.bump(kind);
                    }
                }
            }
            None => Kind::ALL.into_iter().for_each(|k| self.bump(k)),
        }
    }
}

pub struct AccountPush {
    changes: watch::Sender<Changes>,
    task: JoinHandle<()>,
}

impl AccountPush {
    /// A receiver that wakes up on every change notified from now on.
    pub fn subscribe(&self) -> watch::Receiver<Changes> {
        self.changes.subscribe()
    }
}

/// Tells a request when its device sent another one.
#[derive(Clone, Debug)]
pub struct NextRequest {
    receiver: watch::Receiver<u64>,
    /// This request's number among the device's requests.
    arrival: u64,
    /// Keeps a detached one waiting forever.
    _sender: Option<std::sync::Arc<watch::Sender<u64>>>,
}

impl NextRequest {
    /// Resolves once the device has sent a request after this one (at
    /// once if it already has).
    pub async fn arrived(&mut self) {
        let arrival = self.arrival;
        let _ = self.receiver.wait_for(|n| *n != arrival).await;
    }

    /// One that never resolves, for requests outside the device's flow.
    pub fn detached() -> Self {
        let (sender, receiver) = watch::channel(0);
        Self {
            receiver,
            arrival: 0,
            _sender: Some(std::sync::Arc::new(sender)),
        }
    }
}

impl Drop for AccountPush {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PushHub {
    /// A new request from the device: ends its waiting Ping or Sync, if
    /// any, and returns what tells this request when the next one arrives.
    /// Numbering and subscribing happen under one lock, so a request that
    /// arrives before this one starts waiting still ends the wait.
    pub fn arrive(&self, user: &str, device_id: &str) -> NextRequest {
        let mut devices = self.devices.locked();
        devices.retain(|_, sender| sender.receiver_count() > 0);
        let sender = devices
            .entry(format!("{user}/{device_id}"))
            .or_insert_with(|| watch::channel(0).0);
        sender.send_modify(|n| *n = n.wrapping_add(1));
        NextRequest {
            arrival: *sender.borrow(),
            receiver: sender.subscribe(),
            _sender: None,
        }
    }

    /// Tells every waiting Ping to answer now (the server is stopping).
    pub fn shut_down(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Records a Ping that was answered without waiting and returns whether
    /// the device is pinging in a tight loop (e.g. ignoring the answers).
    pub fn is_storming(&self, device: &str) -> bool {
        let now = Instant::now();
        let mut answers = self.quick_answers.locked();
        answers.retain(|_, times| times.back().is_some_and(|t| now - *t < STORM_WINDOW));
        let times = answers.entry(device.to_owned()).or_default();
        times.push_back(now);
        while times.front().is_some_and(|t| now - *t >= STORM_WINDOW) {
            times.pop_front();
        }
        times.len() >= STORM_PINGS
    }

    /// The account's notifier; keep the `Arc` for as long as you listen.
    /// Shared by every login of the account (keyed like the store).
    pub fn account(&self, jmap: &JmapClient) -> Arc<AccountPush> {
        let key = jmap
            .user_key()
            .map_or_else(|_| jmap.username().to_owned(), |key| key.into_owned());
        let mut accounts = self.accounts.locked();
        accounts.retain(|_, weak| weak.strong_count() > 0);
        // A listener that stopped (its login was rejected) is replaced, so
        // new credentials get a fresh stream.
        if let Some(existing) = accounts.get(&key).and_then(Weak::upgrade)
            && !existing.task.is_finished()
        {
            return existing;
        }
        let (changes, _) = watch::channel(Changes::default());
        let task = tokio::spawn(listen(jmap.clone(), changes.clone()));
        let account = Arc::new(AccountPush { changes, task });
        accounts.insert(key, Arc::downgrade(&account));
        account
    }
}

async fn listen(jmap: JmapClient, changes: watch::Sender<Changes>) {
    // Changes while no stream was open are not reported: after the first
    // connection, every new one starts with a catch-up.
    let mut catch_up = false;
    loop {
        let result = jmap
            .watch_changes(catch_up, |types| {
                let mut next = *changes.borrow();
                next.record(types.as_deref());
                // Types nobody syncs (e.g. Identity) wake no one.
                changes.send_if_modified(|current| {
                    let modified = *current != next;
                    *current = next;
                    modified
                });
            })
            .await;
        catch_up = true;
        if let Err(e) = result {
            if crate::jmap::is_unauthorized(&e) {
                // Retrying an old password every few seconds would get the
                // user's address banned; the next Ping starts a new stream.
                tracing::info!(
                    user = jmap.username(),
                    "push stream login rejected, stopping it"
                );
                return;
            }
            tracing::warn!(
                user = jmap.username(),
                error = format!("{e:#}"),
                "push stream failed"
            );
            tokio::time::sleep(RECONNECT_DELAY).await;
        } else {
            // A planned end (or the server closing the stream).
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn resolves(next: &mut NextRequest) -> bool {
        tokio::time::timeout(Duration::from_millis(20), next.arrived())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn a_later_request_of_the_same_device_ends_a_wait() {
        let hub = PushHub::default();
        let mut d1 = hub.arrive("rob", "d1");
        let mut d2 = hub.arrive("rob", "d2");
        assert!(!resolves(&mut d1).await);
        // Before d1 starts waiting, as when a request arrives while a
        // hanging Sync is still being set up: the wait still ends.
        let mut later = hub.arrive("rob", "d1");
        assert!(resolves(&mut d1).await);
        assert!(resolves(&mut d1.clone()).await, "a clone too");
        assert!(!resolves(&mut d2).await, "only the same device");
        assert!(!resolves(&mut later).await);
        assert!(!resolves(&mut NextRequest::detached()).await);
        hub.shut_down();
        assert!(*hub.shutdown_signal().borrow());
    }

    #[test]
    fn counts_changes_per_kind() {
        let start = Changes::default();
        let mut now = start;
        now.record(Some(&["Email".to_owned(), "Thread".to_owned()]));
        assert!(now.changed_since(&start, Kind::Mail));
        assert!(!now.changed_since(&start, Kind::Contacts));
        assert!(!now.changed_since(&start, Kind::Folders));
        let before = now;
        now.record(Some(&["Identity".to_owned()]));
        assert_eq!(now, before, "irrelevant types change nothing");
        now.record(None);
        assert!(Kind::ALL.iter().all(|k| now.changed_since(&before, *k)));
    }

    #[test]
    fn detects_tight_ping_loops_per_device() {
        let hub = PushHub::default();
        let storming: Vec<bool> = (0..STORM_PINGS)
            .map(|_| hub.is_storming("rob/d1"))
            .collect();
        assert_eq!(storming.iter().filter(|s| **s).count(), 1);
        assert!(storming[STORM_PINGS - 1]);
        assert!(!hub.is_storming("rob/d2"));
    }
}
