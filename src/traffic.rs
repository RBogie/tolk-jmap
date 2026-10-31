//! Writes EAS request/response pairs to disk so real client sessions can be
//! inspected and later replayed as test fixtures.
//!
//! Capture is opt-in: only users or devices listed in `TRAFFIC_CAPTURE`
//! (`user`, `user/device` or `*`) are recorded, files are readable by the
//! service user only, and they are pruned after the retention period.
//!
//! Per exchange: `<prefix>.json` (metadata) plus `<prefix>.req.wbxml` and
//! `<prefix>.resp.wbxml` when the bodies are non-empty, each with a decoded
//! `.xml` next to it when the body is valid WBXML. The Authorization
//! header is reduced to the user name; bodies contain mail and stay private.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::{HeaderMap, Method, Uri, header::AUTHORIZATION};
use serde_json::{Map, Value, json};

use crate::{
    eas::{EasResponse, query::EasQuery},
    wbxml,
};

pub struct Exchange<'a> {
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub query: &'a EasQuery,
    pub user: Option<&'a str>,
    pub request_headers: &'a HeaderMap,
    pub request_body: &'a [u8],
    pub response: &'a EasResponse,
}

pub struct TrafficLog {
    dir: Option<PathBuf>,
    /// `user`, `user/device` or `*`; empty captures nothing.
    capture: Vec<String>,
    seq: AtomicU64,
}

impl TrafficLog {
    /// Starts logging to `dir` for the `capture` patterns and prunes files
    /// older than `retention` every hour (also when nothing is captured, so
    /// old files do not linger).
    pub async fn new(
        dir: Option<PathBuf>,
        capture: Vec<String>,
        retention: Duration,
    ) -> anyhow::Result<Self> {
        if let Some(dir) = &dir {
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .await?;
            if capture.is_empty() {
                tracing::info!(dir = %dir.display(), "traffic capture off (TRAFFIC_CAPTURE is empty)");
            } else {
                tracing::warn!(dir = %dir.display(), ?capture, retention_hours = retention.as_secs() / 3600, "capturing EAS traffic, which contains mail");
            }
            let dir = dir.clone();
            tokio::spawn(async move {
                let mut hourly = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    hourly.tick().await;
                    match prune(&dir, retention).await {
                        Ok(0) => {}
                        Ok(removed) => tracing::info!(removed, "pruned old traffic files"),
                        Err(e) => tracing::warn!(error = %e, "could not prune traffic files"),
                    }
                }
            });
        }
        Ok(Self {
            dir,
            capture,
            seq: AtomicU64::new(0),
        })
    }

    fn captures(&self, user: Option<&str>, device: Option<&str>) -> bool {
        let Some(user) = user else { return false };
        self.capture.iter().any(|pattern| {
            pattern == "*"
                || pattern.eq_ignore_ascii_case(user)
                || device.is_some_and(|d| pattern.eq_ignore_ascii_case(&format!("{user}/{d}")))
        })
    }

    pub async fn record(&self, exchange: &Exchange<'_>) {
        let Some(dir) = &self.dir else { return };
        if !self.captures(exchange.user, exchange.query.device_id.as_deref()) {
            return;
        }
        if let Err(e) = self.write(dir, exchange).await {
            tracing::warn!(error = %e, "failed to write traffic log");
        }
    }

    async fn write(&self, dir: &Path, ex: &Exchange<'_>) -> anyhow::Result<()> {
        let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let device = sanitize(ex.query.device_id.as_deref().unwrap_or("nodevice"));
        let cmd = match ex.query.cmd.as_deref() {
            Some(cmd) => sanitize(cmd),
            None => ex.method.as_str().to_owned(),
        };
        let prefix = format!("{millis}-{seq:06}-{device}-{cmd}");

        let meta = json!({
            "time_ms": millis,
            "user": ex.user,
            "method": ex.method.as_str(),
            "path": ex.uri.path(),
            "raw_query": ex.uri.query(),
            "query": ex.query,
            "request_headers": headers_json(ex.request_headers, ex.user),
            "request_body_len": ex.request_body.len(),
            "response_status": ex.response.status.as_u16(),
            "response_headers": headers_json(&ex.response.headers, None),
            "response_body_len": ex.response.body.len(),
        });
        write_private(
            &dir.join(format!("{prefix}.json")),
            &serde_json::to_vec_pretty(&meta)?,
        )
        .await?;
        write_body(dir, &format!("{prefix}.req"), ex.request_body).await?;
        write_body(dir, &format!("{prefix}.resp"), &ex.response.body).await?;
        Ok(())
    }
}

/// Removes files last modified longer than `max_age` ago; returns how many.
async fn prune(dir: &Path, max_age: Duration) -> std::io::Result<usize> {
    let mut removed = 0;
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let modified = entry.metadata().await?.modified()?;
        if modified.elapsed().is_ok_and(|age| age > max_age) {
            tokio::fs::remove_file(entry.path()).await?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Writes the raw body, plus an `.xml` rendering when it is valid WBXML.
async fn write_body(dir: &Path, stem: &str, body: &[u8]) -> anyhow::Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    write_private(&dir.join(format!("{stem}.wbxml")), body).await?;
    if let Ok(root) = wbxml::decode(body) {
        write_private(
            &dir.join(format!("{stem}.xml")),
            wbxml::to_xml(&root).as_bytes(),
        )
        .await?;
    }
    Ok(())
}

/// Writes a file only the service user can read (traffic contains mail).
async fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(data).await
}

fn headers_json(headers: &HeaderMap, user: Option<&str>) -> Value {
    let mut map = Map::new();
    for (name, value) in headers {
        let value = if name == AUTHORIZATION {
            format!("<redacted, user={}>", user.unwrap_or("?"))
        } else {
            String::from_utf8_lossy(value.as_bytes()).into_owned()
        };
        map.insert(name.as_str().to_owned(), Value::String(value));
    }
    Value::Object(map)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(48)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(capture: &[&str]) -> TrafficLog {
        TrafficLog {
            dir: None,
            capture: capture.iter().map(|c| c.to_string()).collect(),
            seq: AtomicU64::new(0),
        }
    }

    #[test]
    fn captures_only_listed_users_and_devices() {
        assert!(!log(&[]).captures(Some("rob"), Some("d1")));
        assert!(log(&["*"]).captures(Some("rob"), Some("d1")));
        assert!(!log(&["*"]).captures(None, None));
        assert!(log(&["Rob"]).captures(Some("rob"), Some("d1")));
        assert!(log(&["rob/d1"]).captures(Some("rob"), Some("d1")));
        assert!(!log(&["rob/d1"]).captures(Some("rob"), Some("d2")));
        assert!(!log(&["ann"]).captures(Some("rob"), None));
    }
}
