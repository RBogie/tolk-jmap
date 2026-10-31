//! Autodiscover for mobile clients (MS-ASCMD 2.2.1.1 and the v2 JSON form),
//! so iOS Mail, Samsung Email and Outlook can set up an account from just an
//! address and password.
//!
//! - `POST /autodiscover/autodiscover.xml` (any case): the XML request with
//!   the `mobilesync` response schema, authenticated like EAS. The reply
//!   names the EAS URL and the user.
//! - `GET /autodiscover/autodiscover.json/v1.0/<address>?Protocol=ActiveSync`
//!   (or `?Email=`): unauthenticated, only the EAS URL.
//!
//! The EAS URL is `PUBLIC_URL` plus `/Microsoft-Server-ActiveSync`, or the
//! request's own host when `PUBLIC_URL` is unset (`X-Forwarded-Host` only
//! from a trusted proxy). Requests for other
//! schemas (Outlook desktop's IMAP settings) go on to the JMAP server.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};

use crate::{AppState, auth::Credentials, eas::EAS_PATH, jmap::AuthError};

const MOBILESYNC_SCHEMA: &str =
    "http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006";
const MAX_REQUEST: usize = 64 * 1024;
const RESPONSE_NS: &str = "http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006";

/// Whether `path` is an Autodiscover path (they arrive in any case).
pub fn is_autodiscover(path: &str) -> bool {
    path.to_ascii_lowercase().starts_with("/autodiscover/")
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    // Requests are a few hundred bytes; nothing larger is read.
    let Ok(body) = axum::body::to_bytes(body, MAX_REQUEST).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let path = uri.path().to_ascii_lowercase();
    let public_url = state.public_url.as_deref();
    let from_proxy = state
        .trusted_proxies
        .iter()
        .any(|range| range.contains(peer.ip()));
    let eas_url = |address: Option<&str>| eas_url(public_url, &headers, from_proxy, address);
    if path.starts_with("/autodiscover/autodiscover.json") {
        // The address is the last path segment, or the Email parameter.
        let query = uri.query().unwrap_or_default();
        let address = form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k.eq_ignore_ascii_case("email"))
            .map(|(_, v)| v.into_owned())
            .or_else(|| {
                let last = uri.path().rsplit('/').next()?;
                let last = form_urlencoded::parse(format!("a={last}").as_bytes())
                    .next()?
                    .1
                    .into_owned();
                last.contains('@').then_some(last)
            });
        return json(&uri, eas_url(address.as_deref()).as_deref());
    }
    if path != "/autodiscover/autodiscover.xml" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method == Method::GET {
        // Probes: the real request is a POST.
        return unauthorized();
    }
    let request = String::from_utf8_lossy(&body);
    let Some(address) = element_text(&request, "EMailAddress") else {
        return xml_response(&error_response("600", "Invalid Request"));
    };
    let schema = element_text(&request, "AcceptableResponseSchema");
    if schema.is_some_and(|s| s != MOBILESYNC_SCHEMA) {
        // Outlook desktop asks for IMAP and SMTP settings: the JMAP server
        // (Stalwart) answers those.
        return forward(&state, peer, &headers, body).await;
    }

    let Some(creds) = Credentials::from_headers(&headers) else {
        return unauthorized();
    };
    let client_ip = crate::eas::client_ip(&state, peer.ip(), &headers);
    if state.logins.is_blocked(&creds, client_ip) {
        tracing::warn!(user = %creds.username, %client_ip, "autodiscover: too many failed logins");
        return unauthorized();
    }
    let jmap = match state.jmap.connect(&creds, Some(client_ip)).await {
        Ok(jmap) => {
            state.logins.record_success(&creds, client_ip);
            jmap
        }
        Err(AuthError::Invalid) => {
            tracing::info!(user = %creds.username, %client_ip, "autodiscover: rejected credentials");
            state.logins.record_failure(&creds, client_ip);
            return unauthorized();
        }
        Err(AuthError::Backend(e)) => {
            tracing::warn!(error = format!("{e:#}"), "autodiscover: JMAP unavailable");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let email = if address.contains('@') {
        address.clone()
    } else {
        jmap.username().to_owned()
    };
    let Some(eas_url) = eas_url(Some(&email)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    tracing::info!(user = %creds.username, %email, url = %eas_url, "autodiscover");
    xml_response(&mobilesync_response(&email, &email, &eas_url))
}

/// The JMAP server's answer to a request for another schema, or error 601
/// when it has none.
async fn forward(state: &AppState, peer: SocketAddr, headers: &HeaderMap, body: Bytes) -> Response {
    let header = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    let client_ip = crate::eas::client_ip(state, peer.ip(), headers);
    match state
        .jmap
        .forward_autodiscover(
            header(header::AUTHORIZATION),
            header(header::CONTENT_TYPE),
            client_ip,
            body.to_vec(),
        )
        .await
    {
        Ok((status, content_type, body)) if status != 404 => {
            let mut response = (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                body,
            )
                .into_response();
            if let Some(value) = content_type.and_then(|c| HeaderValue::from_str(&c).ok()) {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            if status == 401 {
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"Autodiscover\""),
                );
            }
            response
        }
        Ok(_) => xml_response(&error_response("601", "Provider is not available")),
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "autodiscover: forwarding failed");
            xml_response(&error_response("601", "Provider is not available"))
        }
    }
}

/// The EAS URL to hand out. `{domain}` in `PUBLIC_URL` stands for the
/// domain of the address being looked up (`https://mail.{domain}`).
/// Without `PUBLIC_URL` the request's host is used; `X-Forwarded-Host`
/// counts only when the request came `from_proxy` (a trusted one).
fn eas_url(
    public_url: Option<&str>,
    headers: &HeaderMap,
    from_proxy: bool,
    address: Option<&str>,
) -> Option<String> {
    let base = match public_url {
        Some(url) if url.contains("{domain}") => {
            let domain = address?.rsplit_once('@')?.1.trim().to_ascii_lowercase();
            let valid = !domain.is_empty()
                && domain
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            if !valid {
                return None;
            }
            url.trim_end_matches('/').replace("{domain}", &domain)
        }
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => {
            let host = headers
                .get("x-forwarded-host")
                .filter(|_| from_proxy)
                .or_else(|| headers.get(header::HOST))
                .and_then(|h| h.to_str().ok())?
                .split(',')
                .next()?
                .trim();
            // A host name, an optional port, or a bracketed IPv6 address.
            let valid = host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
            if host.is_empty() || !valid {
                return None;
            }
            format!("https://{host}")
        }
    };
    Some(format!("{base}{EAS_PATH}"))
}

fn json(uri: &Uri, eas_url: Option<&str>) -> Response {
    let query = uri.query().unwrap_or_default();
    let protocol = form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k.eq_ignore_ascii_case("protocol"))
        .map(|(_, v)| v.into_owned());
    if !protocol.is_some_and(|p| p.eq_ignore_ascii_case("activesync")) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "ErrorCode": "ProtocolNotSupported",
                "ErrorMessage": "Only ActiveSync is offered",
            })),
        )
            .into_response();
    }
    let Some(url) = eas_url else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    axum::Json(serde_json::json!({ "Protocol": "ActiveSync", "Url": url })).into_response()
}

/// The text of the first `<name>` element (any namespace prefix), unescaped.
fn element_text(xml: &str, name: &str) -> Option<String> {
    let mut rest = xml;
    loop {
        let start = rest.find('<')?;
        rest = &rest[start + 1..];
        let end = rest.find('>')?;
        let tag = &rest[..end];
        let local = tag.rsplit(':').next().unwrap_or(tag).trim();
        rest = &rest[end + 1..];
        if local == name {
            let close = rest.find('<')?;
            return Some(unescape(rest[..close].trim()));
        }
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn mobilesync_response(display_name: &str, email: &str, url: &str) -> String {
    let (name, email, url) = (escape(display_name), escape(email), escape(url));
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="{RESPONSE_NS}">
  <Response xmlns="{MOBILESYNC_SCHEMA}">
    <Culture>en:us</Culture>
    <User>
      <DisplayName>{name}</DisplayName>
      <EMailAddress>{email}</EMailAddress>
    </User>
    <Action>
      <Settings>
        <Server>
          <Type>MobileSync</Type>
          <Url>{url}</Url>
          <Name>{url}</Name>
        </Server>
      </Settings>
    </Action>
  </Response>
</Autodiscover>
"#
    )
}

fn error_response(code: &str, message: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="{RESPONSE_NS}">
  <Response xmlns="{MOBILESYNC_SCHEMA}">
    <Culture>en:us</Culture>
    <Action>
      <Error>
        <Status>{code}</Status>
        <Message>{message}</Message>
      </Error>
    </Action>
  </Response>
</Autodiscover>
"#
    )
}

fn xml_response(body: &str) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/xml; charset=utf-8"),
        )],
        body.to_owned(),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"ActiveSync\""),
        )],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IOS_REQUEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006">
<Request>
<EMailAddress>rob@example.com</EMailAddress>
<AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006</AcceptableResponseSchema>
</Request>
</Autodiscover>"#;

    #[test]
    fn reads_the_request() {
        assert_eq!(
            element_text(IOS_REQUEST, "EMailAddress").as_deref(),
            Some("rob@example.com")
        );
        assert_eq!(
            element_text(IOS_REQUEST, "AcceptableResponseSchema").as_deref(),
            Some(MOBILESYNC_SCHEMA)
        );
        assert_eq!(
            element_text("<a:EMailAddress> x&amp;y </a:EMailAddress>", "EMailAddress").as_deref(),
            Some("x&y")
        );
        assert_eq!(element_text("<Request/>", "EMailAddress"), None);
    }

    #[test]
    fn hands_out_the_public_or_request_url() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "mail.example.com".parse().unwrap());
        assert_eq!(
            eas_url(None, &headers, false, None).as_deref(),
            Some("https://mail.example.com/Microsoft-Server-ActiveSync")
        );
        assert_eq!(
            eas_url(Some("https://eas.example.org/"), &headers, false, None).as_deref(),
            Some("https://eas.example.org/Microsoft-Server-ActiveSync")
        );
        headers.insert(header::HOST, "evil.example/x".parse().unwrap());
        assert_eq!(eas_url(None, &headers, false, None), None);
        assert_eq!(
            eas_url(
                Some("https://mail.{domain}"),
                &headers,
                false,
                Some("rob@Example.NL")
            )
            .as_deref(),
            Some("https://mail.example.nl/Microsoft-Server-ActiveSync")
        );
        assert_eq!(
            eas_url(
                Some("https://mail.{domain}"),
                &headers,
                false,
                Some("x@evil/x")
            ),
            None
        );
        assert_eq!(
            eas_url(Some("https://mail.{domain}"), &headers, false, None),
            None
        );
    }

    #[test]
    fn trusts_forwarded_hosts_only_from_proxies() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "mail.example.com".parse().unwrap());
        headers.insert("x-forwarded-host", "evil.example".parse().unwrap());
        let url = |from_proxy| eas_url(None, &headers, from_proxy, None);
        assert_eq!(
            url(false).as_deref(),
            Some("https://mail.example.com/Microsoft-Server-ActiveSync")
        );
        assert_eq!(
            url(true).as_deref(),
            Some("https://evil.example/Microsoft-Server-ActiveSync")
        );
        headers.insert(header::HOST, "a.example\"@x".parse().unwrap());
        assert_eq!(eas_url(None, &headers, false, None), None);
    }

    #[test]
    fn escapes_user_data_in_the_response() {
        let xml = mobilesync_response(
            "A <b>",
            "a@example.com",
            "https://x/Microsoft-Server-ActiveSync",
        );
        assert!(xml.contains("<DisplayName>A &lt;b&gt;</DisplayName>"));
        assert!(xml.contains("<Type>MobileSync</Type>"));
    }
}
