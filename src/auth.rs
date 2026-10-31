//! Credentials from the device: HTTP Basic, or an OAuth bearer token issued
//! by the JMAP server (Stalwart is an OAuth provider). They are verified by
//! [`crate::jmap::JmapPool`] when it opens a session with them and passed on
//! as they came. Passwords and tokens are overwritten in memory when the
//! credentials are dropped.

use axum::http::{HeaderMap, header::AUTHORIZATION};
use base64::{Engine, engine::general_purpose::STANDARD};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Credentials {
    /// The login name; for a token, `token:` and a fingerprint of it until
    /// the JMAP session names the user.
    pub username: String,
    /// The password, or the token.
    pub password: String,
    pub bearer: bool,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.password);
    }
}

impl Credentials {
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
        let (scheme, encoded) = value.split_once(' ')?;
        if scheme.eq_ignore_ascii_case("bearer") {
            let token = encoded.trim();
            if token.is_empty() {
                return None;
            }
            return Some(Self {
                username: format!("token:{:016x}", fingerprint(token)),
                password: token.to_owned(),
                bearer: true,
            });
        }
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        // The decoded "user:password" is wiped as well when it goes.
        let decoded =
            zeroize::Zeroizing::new(String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?);
        let (user, password) = decoded.split_once(':')?;
        Some(Self {
            username: normalize_username(user),
            password: password.to_owned(),
            bearer: false,
        })
    }

    /// Adds these credentials to a request to the JMAP server.
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.bearer {
            request.bearer_auth(&self.password)
        } else {
            request.basic_auth(&self.username, Some(&self.password))
        }
    }
}

/// `request.credentials(&creds)`: [`Credentials::authorize`] in a chain.
pub trait WithCredentials {
    fn credentials(self, creds: &Credentials) -> Self;
}

impl WithCredentials for reqwest::RequestBuilder {
    fn credentials(self, creds: &Credentials) -> Self {
        creds.authorize(self)
    }
}

/// FNV-1a of a token: names it in logs and limits without revealing it.
fn fingerprint(token: &str) -> u64 {
    token.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Exchange clients may send `DOMAIN\user`; JMAP only knows `user`.
fn normalize_username(user: &str) -> String {
    match user.rsplit_once('\\') {
        Some((_, name)) => name.to_owned(),
        None => user.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_auth_with_domain() {
        let mut headers = HeaderMap::new();
        let encoded = STANDARD.encode("CORP\\rob:se:cret");
        headers.insert(AUTHORIZATION, format!("Basic {encoded}").parse().unwrap());
        let creds = Credentials::from_headers(&headers).unwrap();
        assert_eq!(creds.username, "rob");
        assert_eq!(creds.password, "se:cret");
    }

    #[test]
    fn takes_bearer_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer abc.def".parse().unwrap());
        let creds = Credentials::from_headers(&headers).unwrap();
        assert!(creds.bearer);
        assert_eq!(creds.password, "abc.def");
        assert!(creds.username.starts_with("token:"));
        assert!(!creds.username.contains("abc"));
    }

    #[test]
    fn rejects_other_schemes() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Digest abc".parse().unwrap());
        assert!(Credentials::from_headers(&headers).is_none());
        headers.insert(AUTHORIZATION, "Bearer  ".parse().unwrap());
        assert!(Credentials::from_headers(&headers).is_none());
    }
}
