//! Configuration from environment variables (see docs/configuration.md);
//! invalid values stop the server at startup.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;

pub struct Config {
    pub listen_addr: SocketAddr,
    /// Base URL of the JMAP server, e.g. `https://mail.example.com`.
    pub jmap_url: String,
    /// Other JMAP servers by mail domain (`JMAP_SERVERS`: comma separated
    /// `domain=url`); logins for other domains go to `jmap_url`.
    pub jmap_servers: Vec<(String, String)>,
    /// SQLite database with device and sync state.
    pub state_db: PathBuf,
    /// Where raw EAS traffic is written. Unset disables traffic logging.
    pub traffic_dir: Option<PathBuf>,
    /// Whose traffic is captured (`TRAFFIC_CAPTURE`: comma separated `user`,
    /// `user/device` or `*`); nothing by default.
    pub traffic_capture: Vec<String>,
    /// How long traffic files are kept; they contain mail.
    pub traffic_retention: Duration,
    /// How long a verified JMAP session is reused.
    pub auth_cache_ttl: Duration,
    /// Comma separated `TRUSTED_PROXIES` (addresses or CIDR ranges), e.g.
    /// the Traefik address.
    pub trusted_proxies: Vec<Cidr>,
    /// The address devices reach us at (`PUBLIC_URL`, e.g.
    /// `https://mail.example.com`), handed out by Autodiscover. Unset: the
    /// host of each Autodiscover request.
    pub public_url: Option<String>,
    /// EAS versions offered to devices (`EAS_VERSIONS`), oldest first.
    pub eas_versions: Vec<String>,
    pub tuning: Tuning,
    /// Where Prometheus metrics are served (`METRICS_ADDR`); off when unset.
    pub metrics_addr: Option<SocketAddr>,
    /// Daily copies of the state database (`BACKUP_DIR`, default `backups`
    /// next to it) and how many are kept (`BACKUP_KEEP`, 7; 0 turns them off).
    pub backup_dir: PathBuf,
    pub backup_keep: usize,
}

/// Limits with sensible defaults, each overridable by an environment
/// variable (in parentheses).
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Largest request body (`MAX_REQUEST_MB`, 64).
    pub max_request_bytes: usize,
    /// How long requests may finish after SIGTERM (`SHUTDOWN_DEADLINE_SECS`,
    /// 8); keep it below the container's stop timeout.
    pub shutdown_deadline: Duration,
    /// Pings and waiting Syncs at once (`MAX_PINGS`, 1000).
    pub max_pings: usize,
    /// Other requests running at once (`MAX_COMMANDS`, 256).
    pub max_commands: usize,
    /// Failed logins from one address within 15 minutes before logins from
    /// there are refused (`LOGIN_FAILURES_PER_ADDRESS`, 10).
    pub login_failures_per_address: usize,
    /// The same for one user from any address (`LOGIN_FAILURES_PER_USER`, 20).
    pub login_failures_per_user: usize,
    /// Devices one account may register (`MAX_DEVICES_PER_USER`, 20); a
    /// new device beyond it is refused until an old one is forgotten.
    pub max_devices_per_user: usize,
    pub caps: Caps,
}

/// Limits on what devices are sent, each overridable by an environment
/// variable (in parentheses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// How far back, in days, any device may sync mail and calendars
    /// (`SYNC_MAX_AGE_DAYS`); unset means no limit.
    pub sync_max_age_days: Option<u32>,
    /// Contact photos larger than this are not sent
    /// (`CONTACT_PICTURE_MAX_KB`, 48, like Exchange).
    pub contact_picture_max_bytes: usize,
    /// Search results per request (`SEARCH_MAX_RESULTS`, 100).
    pub search_max_results: usize,
    /// How often a waiting Ping or Sync re-checks folders in shared accounts
    /// (`SHARED_POLL_SECS`, 120): Stalwart pushes no changes for them.
    pub shared_poll: Duration,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            sync_max_age_days: None,
            contact_picture_max_bytes: 48 * 1024,
            search_max_results: 100,
            shared_poll: Duration::from_secs(120),
        }
    }
}

impl Caps {
    fn from_env() -> anyhow::Result<Self> {
        let d = Self::default();
        let max_age = match std::env::var("SYNC_MAX_AGE_DAYS") {
            Ok(_) => Some(number("SYNC_MAX_AGE_DAYS", 1)? as u32),
            Err(_) => None,
        };
        Ok(Self {
            sync_max_age_days: max_age,
            contact_picture_max_bytes: number(
                "CONTACT_PICTURE_MAX_KB",
                d.contact_picture_max_bytes / 1024,
            )? * 1024,
            search_max_results: number("SEARCH_MAX_RESULTS", d.search_max_results)?,
            shared_poll: Duration::from_secs(
                number("SHARED_POLL_SECS", d.shared_poll.as_secs() as usize)?.max(5) as u64,
            ),
        })
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            max_request_bytes: 64 * 1024 * 1024,
            shutdown_deadline: Duration::from_secs(8),
            max_pings: 1000,
            max_commands: 256,
            login_failures_per_address: 10,
            login_failures_per_user: 20,
            max_devices_per_user: 20,
            caps: Caps::default(),
        }
    }
}

impl Tuning {
    fn from_env() -> anyhow::Result<Self> {
        let d = Self::default();
        Ok(Self {
            max_request_bytes: number("MAX_REQUEST_MB", d.max_request_bytes / (1024 * 1024))?
                * 1024
                * 1024,
            shutdown_deadline: Duration::from_secs(number(
                "SHUTDOWN_DEADLINE_SECS",
                d.shutdown_deadline.as_secs() as usize,
            )? as u64),
            max_pings: number("MAX_PINGS", d.max_pings)?,
            max_commands: number("MAX_COMMANDS", d.max_commands)?,
            login_failures_per_address: number(
                "LOGIN_FAILURES_PER_ADDRESS",
                d.login_failures_per_address,
            )?,
            login_failures_per_user: number("LOGIN_FAILURES_PER_USER", d.login_failures_per_user)?,
            max_devices_per_user: number("MAX_DEVICES_PER_USER", d.max_devices_per_user)?,
            caps: Caps::from_env()?,
        })
    }
}

/// A positive whole number from the environment, or `default`.
fn number(key: &str, default: usize) -> anyhow::Result<usize> {
    match std::env::var(key) {
        Ok(value) => {
            let n: usize = value
                .trim()
                .parse()
                .with_context(|| format!("{key}: {value}"))?;
            anyhow::ensure!(n > 0, "{key} must be at least 1");
            Ok(n)
        }
        Err(_) => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let listen_addr = env_or("LISTEN_ADDR", "0.0.0.0:8080")
            .parse()
            .context("LISTEN_ADDR")?;
        let jmap_url = std::env::var("JMAP_URL")
            .context("JMAP_URL must be set")?
            .trim_end_matches('/')
            .to_owned();
        let jmap_servers = parse_servers(&list("JMAP_SERVERS"))?;
        let state_db = PathBuf::from(env_or("STATE_DB", "state.sqlite"));
        let traffic_dir = std::env::var_os("TRAFFIC_DIR").map(PathBuf::from);
        let traffic_capture = list("TRAFFIC_CAPTURE");
        let traffic_retention = match std::env::var("TRAFFIC_RETENTION_HOURS") {
            Ok(hours) => {
                Duration::from_secs(3600 * hours.parse::<u64>().context("TRAFFIC_RETENTION_HOURS")?)
            }
            Err(_) => Duration::from_secs(
                86_400
                    * env_or("TRAFFIC_RETENTION_DAYS", "1")
                        .parse::<u64>()
                        .context("TRAFFIC_RETENTION_DAYS")?,
            ),
        };
        let auth_cache_ttl = Duration::from_secs(
            env_or("AUTH_CACHE_TTL_SECS", "300")
                .parse()
                .context("AUTH_CACHE_TTL_SECS")?,
        );
        let trusted_proxies = list("TRUSTED_PROXIES")
            .iter()
            .map(|s| s.parse().with_context(|| format!("TRUSTED_PROXIES: {s}")))
            .collect::<anyhow::Result<_>>()?;
        let public_url = std::env::var("PUBLIC_URL")
            .ok()
            .map(|url| url.trim().trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty());
        if let Some(url) = &public_url {
            anyhow::ensure!(
                url.starts_with("https://") || url.starts_with("http://"),
                "PUBLIC_URL must start with https://"
            );
        }
        let eas_versions = match list("EAS_VERSIONS") {
            versions if versions.is_empty() => DEFAULT_EAS_VERSIONS
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            versions => versions,
        };
        for version in &eas_versions {
            anyhow::ensure!(
                SUPPORTED_EAS_VERSIONS.contains(&version.as_str()),
                "EAS_VERSIONS: {version} is not one of {}",
                SUPPORTED_EAS_VERSIONS.join(", ")
            );
        }
        for url in std::iter::once(&jmap_url).chain(jmap_servers.iter().map(|(_, url)| url)) {
            if url.starts_with("http://") {
                tracing::warn!(
                    url,
                    "JMAP server URL is not HTTPS: passwords travel unencrypted to it"
                );
            }
        }
        let backup_dir = std::env::var_os("BACKUP_DIR").map_or_else(
            || {
                state_db
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("backups")
            },
            PathBuf::from,
        );
        Ok(Self {
            listen_addr,
            jmap_url,
            jmap_servers,
            state_db,
            traffic_dir,
            traffic_capture,
            traffic_retention,
            auth_cache_ttl,
            trusted_proxies,
            public_url,
            eas_versions,
            tuning: Tuning::from_env()?,
            backup_dir,
            backup_keep: match std::env::var("BACKUP_KEEP") {
                Ok(n) => n.trim().parse().context("BACKUP_KEEP")?,
                Err(_) => 7,
            },
            metrics_addr: std::env::var("METRICS_ADDR")
                .ok()
                .filter(|a| !a.trim().is_empty())
                .map(|a| a.trim().parse().context("METRICS_ADDR"))
                .transpose()?,
        })
    }
}

/// EAS versions Tolk can speak.
pub const SUPPORTED_EAS_VERSIONS: &[&str] = &["14.0", "14.1", "16.0", "16.1"];
/// Offered when `EAS_VERSIONS` is unset.
pub const DEFAULT_EAS_VERSIONS: &[&str] = SUPPORTED_EAS_VERSIONS;

/// An address range such as `192.0.2.0/24`; a bare address is one host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl std::str::FromStr for Cidr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (address, prefix) = match s.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix.parse::<u8>()?)),
            None => (s, None),
        };
        let network: IpAddr = address.parse()?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        anyhow::ensure!(prefix <= max, "prefix /{prefix} is too long");
        Ok(Self { network, prefix })
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = |prefix: u8, width: u32| {
            if prefix == 0 {
                0
            } else {
                u128::MAX << (width - u32::from(prefix))
            }
        };
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = bits(self.prefix, 32) as u32;
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = bits(self.prefix, 128);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// A comma separated environment variable, empty entries dropped.
/// `JMAP_SERVERS` entries (`domain=url`) as (domain, url).
fn parse_servers(entries: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    entries
        .iter()
        .map(|entry| {
            let (domain, url) = entry
                .split_once('=')
                .with_context(|| format!("JMAP_SERVERS: {entry} is not domain=url"))?;
            let (domain, url) = (domain.trim(), url.trim().trim_end_matches('/'));
            anyhow::ensure!(
                !domain.is_empty() && !domain.contains('@'),
                "JMAP_SERVERS: {domain:?} is not a mail domain"
            );
            anyhow::ensure!(
                url.starts_with("https://") || url.starts_with("http://"),
                "JMAP_SERVERS: {url} must start with https://"
            );
            Ok((domain.to_ascii_lowercase(), url.to_owned()))
        })
        .collect()
}

fn list(key: &str) -> Vec<String> {
    env_or(key, "")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jmap_servers() {
        let entries = ["Example.org=https://jmap.example.org/".to_owned()];
        assert_eq!(
            parse_servers(&entries).unwrap(),
            [(
                "example.org".to_owned(),
                "https://jmap.example.org".to_owned()
            )]
        );
        for bad in ["example.org", "=https://x", "a@b=https://x", "b=ftp://x"] {
            assert!(parse_servers(&[bad.to_owned()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn matches_cidr_ranges() {
        let range: Cidr = "192.0.2.0/24".parse().unwrap();
        assert!(range.contains("192.0.2.68".parse().unwrap()));
        assert!(!range.contains("198.51.100.68".parse().unwrap()));
        assert!(range.contains("::ffff:192.0.2.1".parse().unwrap()));
        let host: Cidr = "192.0.2.68".parse().unwrap();
        assert!(host.contains("192.0.2.68".parse().unwrap()));
        assert!(!host.contains("192.0.2.69".parse().unwrap()));
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(
            "0.0.0.0/0"
                .parse::<Cidr>()
                .unwrap()
                .contains("1.2.3.4".parse().unwrap())
        );
        assert!("10.0.0.0/33".parse::<Cidr>().is_err());
    }
}
