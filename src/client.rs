//! A minimal EAS client: sending commands and the Provision handshake.
//! The `eas-client` tool and the integration tests both build on it.

use std::time::Duration;

use anyhow::Context;

use crate::wbxml::{self, Element, Tag, cp::provision as pv};

const POLICY_TYPE: &str = "MS-EAS-Provisioning-WBXML";

/// One device talking to an EAS server.
pub struct EasClient {
    http: reqwest::Client,
    /// The server, e.g. `https://mail.example.com`.
    pub url: String,
    pub user: String,
    /// The password, or with `bearer` an OAuth access token.
    pub password: String,
    pub bearer: bool,
    pub device_id: String,
    pub device_type: String,
    /// Sent as `X-MS-PolicyKey`; "0" before provisioning.
    pub policy_key: String,
    /// Sent as `MS-ASProtocolVersion`.
    pub version: String,
    /// Per request; long enough for the longest Ping by default.
    pub timeout: Duration,
}

/// A response: its HTTP status and decoded body, if any.
pub struct Reply {
    pub status: reqwest::StatusCode,
    pub body: Option<Element>,
}

impl Reply {
    /// The body; panics when there is none (for tests and scripts).
    pub fn root(&self) -> &Element {
        self.body.as_ref().expect("response has a body")
    }
}

impl EasClient {
    pub fn new(url: &str, user: &str, password: &str, device_id: &str, device_type: &str) -> Self {
        Self {
            http: crate::http_client().build().expect("HTTP client"),
            url: url.trim_end_matches('/').to_owned(),
            user: user.to_owned(),
            password: password.to_owned(),
            bearer: false,
            device_id: device_id.to_owned(),
            device_type: device_type.to_owned(),
            policy_key: "0".to_owned(),
            version: "14.1".to_owned(),
            timeout: Duration::from_secs(3600),
        }
    }

    fn authorization(&self, password: &str) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD};
        if self.bearer {
            format!("Bearer {password}")
        } else {
            format!(
                "Basic {}",
                STANDARD.encode(format!("{}:{password}", self.user))
            )
        }
    }

    pub async fn send(&self, cmd: &str, body: Option<Element>) -> anyhow::Result<Reply> {
        self.send_full(cmd, &[], body, &self.password).await
    }

    /// A command with extra query parameters (e.g. `SaveInSent`) and a
    /// password of choice.
    pub async fn send_full(
        &self,
        cmd: &str,
        params: &[(String, String)],
        body: Option<Element>,
        password: &str,
    ) -> anyhow::Result<Reply> {
        let mut url = format!(
            "{}/Microsoft-Server-ActiveSync?Cmd={cmd}&User={}&DeviceId={}&DeviceType={}",
            self.url, self.user, self.device_id, self.device_type
        );
        for (key, value) in params {
            url.push_str(&format!("&{key}={value}"));
        }
        let mut request = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization(password))
            .header("MS-ASProtocolVersion", &self.version)
            .header("X-MS-PolicyKey", &self.policy_key)
            .header("Content-Type", "application/vnd.ms-sync.wbxml")
            .timeout(self.timeout);
        if let Some(body) = &body {
            request = request.body(wbxml::encode(body));
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let body = match bytes.is_empty() {
            true => None,
            false => Some(
                wbxml::decode(&bytes).with_context(|| format!("{cmd}: undecodable response"))?,
            ),
        };
        Ok(Reply { status, body })
    }

    /// Both Provision phases; keeps the final policy key and returns the
    /// last response.
    pub async fn provision(&mut self) -> anyhow::Result<Element> {
        let first = self.send("Provision", Some(policy_request(None))).await?;
        let temporary = first
            .body
            .as_ref()
            .and_then(policy_key_of)
            .context("no temporary policy key")?;
        let second = self
            .send("Provision", Some(policy_request(Some(&temporary))))
            .await?;
        let body = second.body.context("empty Provision reply")?;
        self.policy_key = policy_key_of(&body).context("no final policy key")?;
        Ok(body)
    }
}

/// A Provision request: phase one without a key, the acknowledgement with
/// the temporary key.
pub fn policy_request(key: Option<&str>) -> Element {
    let mut policy = Element::new(pv::Policy).child(text(pv::PolicyType, POLICY_TYPE));
    if let Some(key) = key {
        policy.push(text(pv::PolicyKey, key));
        policy.push(text(pv::Status, "1"));
    }
    Element::new(pv::Provision).child(Element::new(pv::Policies).child(policy))
}

/// The policy key in a Provision response.
pub fn policy_key_of(provision: &Element) -> Option<String> {
    provision
        .find(pv::Policies)?
        .find(pv::Policy)?
        .child_text(pv::PolicyKey)
        .map(str::to_owned)
}

pub fn text(tag: Tag, value: impl Into<String>) -> Element {
    Element::with_text(tag, value)
}
