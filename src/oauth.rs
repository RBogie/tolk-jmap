//! OAuth discovery (RFC 9728), only when `OAUTH_ISSUER` is set.
//!
//! Tolk takes bearer tokens the JMAP server issued and passes them on (see
//! [`crate::auth`]); it is no OAuth server itself. With `OAUTH_ISSUER`, a
//! client can find out where to get such a token:
//!
//! - `GET /.well-known/oauth-protected-resource/Microsoft-Server-ActiveSync`
//!   describes the EAS endpoint as a protected resource: the authorization
//!   server and the scopes a token needs.
//! - A 401 from the EAS endpoint offers `Bearer` with a link to that
//!   document, after `Basic`.
//!
//! Off by default: phone mail apps that see a Bearer challenge may try
//! Microsoft's login instead of asking for a password.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{AppState, autodiscover, eas::EAS_PATH};

/// Where the metadata of the EAS endpoint is served.
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource/Microsoft-Server-ActiveSync";

/// What a token needs for Tolk to work: mail, contacts and calendars (as
/// Stalwart names the scopes), and refresh tokens.
pub const SCOPES: [&str; 4] = [
    "urn:ietf:params:oauth:scope:mail",
    "urn:ietf:params:oauth:scope:contacts",
    "urn:ietf:params:oauth:scope:calendars",
    "offline_access",
];

/// The protected resource metadata, or 404 without `OAUTH_ISSUER`.
pub async fn metadata(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let (Some(issuer), Some(base)) = (&state.oauth_issuer, base_url(&state, peer, &headers)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(serde_json::json!({
        "resource": format!("{base}{EAS_PATH}"),
        "authorization_servers": [issuer],
        "scopes_supported": SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "Tolk (Exchange ActiveSync)",
    }))
    .into_response()
}

/// The `Bearer` challenge for a 401, or `None` without `OAUTH_ISSUER`.
pub fn challenge(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> Option<HeaderValue> {
    state.oauth_issuer.as_ref()?;
    let base = base_url(state, peer, headers)?;
    HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{base}{METADATA_PATH}\", scope=\"{}\"",
        SCOPES.join(" ")
    ))
    .ok()
}

/// The address devices reach Tolk at, as Autodiscover hands it out. With a
/// `{domain}` in `PUBLIC_URL` there is no address to fill it from, so the
/// request's host is used.
fn base_url(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> Option<String> {
    let from_proxy = state
        .trusted_proxies
        .iter()
        .any(|range| range.contains(peer.ip()));
    let public_url = state
        .public_url
        .as_deref()
        .filter(|url| !url.contains("{domain}"));
    autodiscover::eas_url(public_url, headers, from_proxy, None)
        .and_then(|url| url.strip_suffix(EAS_PATH).map(str::to_owned))
}
