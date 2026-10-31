//! Tolk: an Exchange ActiveSync server in front of a JMAP mail server.
//!
//! [`build_app`] builds the HTTP router: [`eas`] answers `/Microsoft-Server-ActiveSync`
//! and [`autodiscover`] the Autodiscover paths, both talking to the JMAP
//! server through [`jmap`] with the device's own credentials ([`auth`]).
//! [`store`] keeps what each device was sent, [`push`] relays the server's
//! change notifications to waiting Pings, and [`limits`], [`metrics`],
//! [`maintenance`] and [`traffic`] cover operations. [`client`] and
//! [`fixtures`] serve the test tools.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::get,
};

pub mod auth;
pub mod autodiscover;
pub mod client;
pub mod config;
pub mod eas;
pub mod fixtures;
pub mod jmap;
pub mod limits;
pub mod maintenance;
pub mod metrics;
pub mod push;
pub mod store;
pub mod traffic;
pub mod wbxml;

pub struct AppState {
    pub jmap: jmap::JmapPool,
    pub push: push::PushHub,
    pub store: store::Store,
    pub traffic: traffic::TrafficLog,
    /// Proxies whose forwarding headers are believed.
    pub trusted_proxies: Vec<config::Cidr>,
    /// See [`config::Config::public_url`].
    pub public_url: Option<String>,
    /// Comma separated EAS versions offered (`MS-ASProtocolVersions`).
    pub eas_versions: String,
    pub device_locks: DeviceLocks,
    pub logins: limits::LoginLimiter,
    pub concurrency: limits::Concurrency,
    pub metrics: metrics::Metrics,
    /// See [`config::Tuning::max_devices_per_user`].
    pub max_devices_per_user: usize,
    pub caps: config::Caps,
    /// See [`config::Tuning::max_request_bytes`].
    pub max_request_bytes: usize,
}

/// The router and its shared state, as the binary serves them; integration
/// tests drive the same router. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`.
pub async fn build_app(config: &config::Config) -> anyhow::Result<(Router, Arc<AppState>)> {
    let state = Arc::new(AppState {
        jmap: jmap::JmapPool::new(config)?,
        push: push::PushHub::default(),
        store: store::Store::open(&config.state_db)?,
        traffic: traffic::TrafficLog::new(
            config.traffic_dir.clone(),
            config.traffic_capture.clone(),
            config.traffic_retention,
        )
        .await?,
        trusted_proxies: config.trusted_proxies.clone(),
        public_url: config.public_url.clone(),
        eas_versions: config.eas_versions.join(","),
        device_locks: Default::default(),
        logins: limits::LoginLimiter::new(
            config.tuning.login_failures_per_address,
            config.tuning.login_failures_per_user,
        ),
        concurrency: limits::Concurrency::new(config.tuning.max_pings, config.tuning.max_commands),
        metrics: Default::default(),
        max_devices_per_user: config.tuning.max_devices_per_user,
        caps: config.tuning.caps,
        max_request_bytes: config.tuning.max_request_bytes,
    });
    let app = Router::new()
        .route("/", get(|| async { "tolk\n" }))
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(ready))
        .fallback(dispatch)
        .layer(DefaultBodyLimit::max(config.tuning.max_request_bytes))
        .with_state(state.clone());
    Ok((app, state))
}

/// The metrics endpoint, served on its own address.
pub fn metrics_app(state: Arc<AppState>, tuning: &config::Tuning) -> Router {
    let (pings, commands) = (tuning.max_pings, tuning.max_commands);
    Router::new().route(
        "/metrics",
        get(move || async move {
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; version=0.0.4",
                )],
                metrics::render(&state, pings, commands),
            )
        }),
    )
}

/// EAS and Autodiscover paths arrive in any letter case, so they are
/// matched here rather than as routes.
async fn dispatch(
    state: State<Arc<AppState>>,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    if autodiscover::is_autodiscover(uri.path()) {
        autodiscover::handle(state, peer, method, uri, headers, body).await
    } else {
        eas::handle(state, peer, method, uri, headers, body).await
    }
}

/// Ready when the state database and the JMAP server both answer.
async fn ready(State(state): State<Arc<AppState>>) -> (StatusCode, String) {
    // Details go to the log only: they can name internal hosts and paths.
    if let Err(e) = state.store.check() {
        tracing::warn!(error = format!("{e:#}"), "readyz: database");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable\n".to_owned(),
        );
    }
    if let Err(e) = state.jmap.reachable().await {
        tracing::warn!(error = format!("{e:#}"), "readyz: jmap");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "jmap unreachable\n".to_owned(),
        );
    }
    (StatusCode::OK, "ready\n".to_owned())
}

/// An HTTP client builder with TLS through rustls and the ring provider,
/// verifying certificates against the system's trust store.
pub fn http_client() -> reqwest::ClientBuilder {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        // Fails only when a provider is installed already, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
}

/// Locking that survives a panic elsewhere: a poisoned mutex would
/// otherwise fail every later request until a restart. The data stays
/// usable: SQLite rolls back an unfinished transaction, and the rest are
/// caches and counters.
pub trait LockExt<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

type DeviceLock = tokio::sync::Mutex<()>;

/// One async lock per (user, device); unused locks are dropped.
#[derive(Default)]
pub struct DeviceLocks(Mutex<HashMap<(String, String), Weak<DeviceLock>>>);

impl DeviceLocks {
    pub fn get(&self, user: &str, device_id: &str) -> Arc<DeviceLock> {
        let mut locks = self.0.locked();
        locks.retain(|_, weak| weak.strong_count() > 0);
        let key = (user.to_owned(), device_id.to_owned());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(DeviceLock::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_locks_are_shared_while_held() {
        let locks = DeviceLocks::default();
        let a = locks.get("rob", "d1");
        assert!(Arc::ptr_eq(&a, &locks.get("rob", "d1")));
        assert!(!Arc::ptr_eq(&a, &locks.get("rob", "d2")));
        drop(a);
        assert_eq!(
            locks
                .0
                .locked()
                .values()
                .filter(|w| w.strong_count() > 0)
                .count(),
            0
        );
    }
}
