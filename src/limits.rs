//! Abuse limits: failed logins per address and per user, and caps on how
//! many requests run at once.
//!
//! After too many failed logins from one address (or for one user) within
//! the window, new logins from there are refused without asking the JMAP
//! server, so guessing is cheap to stop and the JMAP server's own ban does
//! not hit a whole household behind one address. Credentials that worked
//! before still get through: a password that was right is not being guessed.

use crate::LockExt;
use std::{
    collections::{HashMap, VecDeque},
    hash::BuildHasher,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::auth::Credentials;

const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Tracked addresses and users; beyond this the oldest are forgotten.
const MAX_TRACKED: usize = 10_000;

pub struct LoginLimiter {
    per_address: usize,
    per_user: usize,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// Per user, a keyed hash of the last password that worked.
    known_good: Mutex<HashMap<String, u64>>,
    hasher: std::hash::RandomState,
}

impl LoginLimiter {
    pub fn new(per_address: usize, per_user: usize) -> Self {
        Self {
            per_address,
            per_user,
            failures: Mutex::default(),
            known_good: Mutex::default(),
            hasher: std::hash::RandomState::new(),
        }
    }

    /// Whether a login with these credentials from `ip` must be refused.
    pub fn is_blocked(&self, creds: &Credentials, ip: IpAddr) -> bool {
        let user = creds.username.to_lowercase();
        if self.known_good.locked().get(&user) == Some(&self.hash(&creds.password)) {
            return false;
        }
        let mut failures = self.failures.locked();
        let mut count = |key: &str| {
            failures.get_mut(key).map_or(0, |times| {
                times.retain(|t| t.elapsed() < WINDOW);
                times.len()
            })
        };
        count(&ip_key(ip)) >= self.per_address || count(&user_key(&user)) >= self.per_user
    }

    /// A rejected login. Its password stops counting as known good (it was
    /// changed or revoked), so retries of it are limited again.
    pub fn record_failure(&self, creds: &Credentials, ip: IpAddr) {
        let user = creds.username.to_lowercase();
        {
            let mut known = self.known_good.locked();
            if known.get(&user) == Some(&self.hash(&creds.password)) {
                known.remove(&user);
            }
        }
        let mut failures = self.failures.locked();
        for key in [ip_key(ip), user_key(&user)] {
            if !failures.contains_key(&key) && failures.len() >= MAX_TRACKED {
                make_room(&mut failures);
            }
            failures.entry(key).or_default().push_back(Instant::now());
        }
    }

    /// A successful login: its password counts as known good. The address's
    /// failures stay until they expire: behind a shared address (a home or
    /// carrier NAT) one working phone must not reset the count for a
    /// device next to it that is guessing.
    pub fn record_success(&self, creds: &Credentials, _ip: IpAddr) {
        let mut known = self.known_good.locked();
        if known.len() >= MAX_TRACKED {
            known.clear();
        }
        known.insert(creds.username.to_lowercase(), self.hash(&creds.password));
    }

    fn hash(&self, password: &str) -> u64 {
        self.hasher.hash_one(password)
    }
}

/// Drops expired entries, or else the entry with the fewest failures (the
/// oldest among those). Clearing everything would let anyone who fills the
/// table with fresh keys reset the counts of the logins being guessed.
fn make_room(failures: &mut HashMap<String, VecDeque<Instant>>) {
    failures.retain(|_, times| times.back().is_some_and(|t| t.elapsed() < WINDOW));
    if failures.len() < MAX_TRACKED {
        return;
    }
    let weakest = failures
        .iter()
        .min_by_key(|(_, times)| (times.len(), times.back().copied()))
        .map(|(key, _)| key.clone());
    if let Some(key) = weakest {
        failures.remove(&key);
    }
}

/// IPv6 addresses count per /64: one host usually has a whole prefix.
fn ip_key(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let prefix = u128::from(v6) & !((1u128 << 64) - 1);
            format!("ip:{}/64", std::net::Ipv6Addr::from(prefix))
        }
        v4 => format!("ip:{v4}"),
    }
}

fn user_key(user: &str) -> String {
    format!("user:{user}")
}

/// How many requests may run at once: long-polling Pings, and everything
/// else. Over the cap a request gets 503 with Retry-After.
pub struct Concurrency {
    pub pings: tokio::sync::Semaphore,
    pub commands: tokio::sync::Semaphore,
}

impl Concurrency {
    pub fn new(pings: usize, commands: usize) -> Self {
        Self {
            pings: tokio::sync::Semaphore::new(pings),
            commands: tokio::sync::Semaphore::new(commands),
        }
    }
}

/// Bodies above this are refused, except for commands that carry messages.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Commands that may carry whole messages (with attachments) or drafts.
const LARGE_BODY_COMMANDS: &[&str] = &[
    "SendMail",
    "SmartReply",
    "SmartForward",
    "Sync",
    "ItemOperations",
];
const MAX_ID_LENGTH: usize = 128;

/// Checks a request line before anything is decoded.
pub fn check_request(device_id: &str, device_type: Option<&str>) -> Result<(), String> {
    let printable = |s: &str| s.len() <= MAX_ID_LENGTH && s.chars().all(|c| c.is_ascii_graphic());
    if !printable(device_id) {
        return Err("unusable DeviceId".to_owned());
    }
    if device_type.is_some_and(|t| !printable(t)) {
        return Err("unusable DeviceType".to_owned());
    }
    Ok(())
}

/// The largest body a command may send: `max` for commands that carry
/// messages, 4 MiB (or `max` if smaller) for the rest.
pub fn body_limit(cmd: &str, max: usize) -> usize {
    if LARGE_BODY_COMMANDS.contains(&cmd) {
        max
    } else {
        MAX_BODY.min(max)
    }
}

/// Whether a request may wait for changes: a Ping, or a Sync with `Wait`
/// or `HeartbeatInterval` (or an empty one, which repeats the last Sync).
pub fn is_long_poll(cmd: &str, body: Option<&crate::wbxml::Element>) -> bool {
    use crate::wbxml::cp::airsync;
    match (cmd, body) {
        ("Ping", _) | ("Sync", None) => true,
        ("Sync", Some(root)) => {
            root.find(airsync::Wait).is_some() || root.find(airsync::HeartbeatInterval).is_some()
        }
        _ => false,
    }
}

/// How long a command may run. Ping and Sync may wait for changes up to
/// the heartbeat limit.
pub fn time_limit(cmd: &str) -> Duration {
    let waiting = Duration::from_secs(crate::eas::ping::MAX_HEARTBEAT);
    match cmd {
        "Ping" | "Sync" => waiting + Duration::from_secs(300),
        _ => Duration::from_secs(300),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(user: &str, password: &str) -> Credentials {
        Credentials {
            username: user.to_owned(),
            password: password.to_owned(),
            bearer: false,
        }
    }

    #[test]
    fn blocks_guessing_but_not_known_passwords() {
        let limiter = LoginLimiter::new(10, 20);
        let home: IpAddr = "10.0.0.1".parse().unwrap();
        let right = creds("rob", "right");
        limiter.record_success(&right, home);
        for _ in 0..10 {
            limiter.record_failure(&creds("rob", "wrong"), home);
        }
        assert!(limiter.is_blocked(&creds("rob", "guess"), home));
        assert!(
            limiter.is_blocked(&creds("ann", "guess"), home),
            "per address"
        );
        assert!(
            !limiter.is_blocked(&right, home),
            "a password that worked passes"
        );
        // Another login working from the same address does not reset it.
        limiter.record_success(&creds("eve", "own"), home);
        assert!(limiter.is_blocked(&creds("rob", "guess"), home));
        let elsewhere: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(!limiter.is_blocked(&creds("ann", "pw"), elsewhere));

        // Many addresses guessing one user.
        for i in 0..20u8 {
            limiter.record_failure(&creds("Ann", "x"), IpAddr::from([10, 1, 0, i]));
        }
        assert!(limiter.is_blocked(&creds("ann", "pw"), elsewhere));
        limiter.record_success(&creds("ann", "pw"), elsewhere);
        assert!(!limiter.is_blocked(&creds("ann", "pw"), elsewhere));
        // Once that password is rejected (changed), it is limited again.
        limiter.record_failure(&creds("ann", "pw"), elsewhere);
        assert!(limiter.is_blocked(&creds("ann", "pw"), elsewhere));
    }

    #[test]
    fn counts_ipv6_per_prefix() {
        let limiter = LoginLimiter::new(10, 1000);
        for i in 0..10u16 {
            let ip = IpAddr::from([0x2001, 0xdb8, 0, 1, 0, 0, 0, i]);
            limiter.record_failure(&creds(&format!("u{i}"), "x"), ip);
        }
        let neighbour = IpAddr::from([0x2001, 0xdb8, 0, 1, 0xffff, 0, 0, 1]);
        assert!(limiter.is_blocked(&creds("new", "x"), neighbour));
        let other = IpAddr::from([0x2001, 0xdb8, 0, 2, 0, 0, 0, 1]);
        assert!(!limiter.is_blocked(&creds("new", "x"), other));
    }

    #[test]
    fn a_full_table_keeps_the_guessed_logins() {
        let limiter = LoginLimiter::new(1_000_000, 20);
        let home: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..20 {
            limiter.record_failure(&creds("rob", "guess"), home);
        }
        for i in 0..MAX_TRACKED as u32 {
            limiter.record_failure(&creds(&format!("x{i}"), "x"), home);
        }
        assert!(limiter.failures.locked().len() <= MAX_TRACKED);
        assert!(limiter.is_blocked(&creds("rob", "guess"), "10.9.9.9".parse().unwrap()));
    }

    #[test]
    fn refuses_odd_requests() {
        assert!(check_request("androidc123", Some("Android")).is_ok());
        assert!(check_request("bad id", None).is_err());
        assert!(check_request(&"x".repeat(200), None).is_err());
        assert_eq!(body_limit("FolderSync", 64 << 20), MAX_BODY);
        assert_eq!(body_limit("SendMail", 64 << 20), 64 << 20);
        assert_eq!(body_limit("Ping", 1024), 1024);

        use crate::wbxml::{Element, cp::airsync};
        let hanging = Element::new(airsync::Sync).child(Element::with_text(airsync::Wait, "5"));
        assert!(is_long_poll("Sync", Some(&hanging)));
        assert!(is_long_poll("Sync", None));
        assert!(!is_long_poll("Sync", Some(&Element::new(airsync::Sync))));
        assert!(is_long_poll("Ping", None));
        assert!(!is_long_poll("FolderSync", None));
    }
}
