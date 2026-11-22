//! Mail: mailboxes, emails, submission, identities and the vacation
//! response.

use std::collections::HashMap;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::*;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub role: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub sort_order: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyPart {
    pub part_id: Option<String>,
    pub blob_id: Option<String>,
    #[serde(default)]
    pub size: u64,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub cid: Option<String>,
    pub disposition: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyValue {
    pub value: String,
    #[serde(default)]
    pub is_truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Email {
    pub id: String,
    pub blob_id: String,
    #[serde(default)]
    pub mailbox_ids: HashMap<String, bool>,
    #[serde(default)]
    pub keywords: HashMap<String, bool>,
    #[serde(default)]
    pub size: u64,
    pub received_at: String,
    pub subject: Option<String>,
    pub from: Option<Vec<EmailAddress>>,
    pub to: Option<Vec<EmailAddress>>,
    pub cc: Option<Vec<EmailAddress>>,
    pub reply_to: Option<Vec<EmailAddress>>,
    #[serde(default)]
    pub html_body: Vec<BodyPart>,
    #[serde(default)]
    pub text_body: Vec<BodyPart>,
    #[serde(default)]
    pub attachments: Vec<BodyPart>,
    #[serde(default)]
    pub body_values: HashMap<String, BodyValue>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub message_id: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub in_reply_to: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub references: Vec<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// The server's plain text preview (up to 256 characters).
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default, rename = "header:Importance:asText")]
    pub importance: Option<String>,
    #[serde(default, rename = "header:X-Priority:asText")]
    pub priority: Option<String>,
}

/// What became of a message submission.
#[derive(Debug)]
pub enum Submission {
    Sent,
    /// The server answered and did not send it; the SetError type, when it
    /// gave one (RFC 8621 section 7.5).
    Rejected(anyhow::Error, Option<String>),
    /// No answer: the message may or may not have gone out.
    Unknown(anyhow::Error),
}

/// Whether a failed request certainly changed nothing on the JMAP server:
/// no connection, a request refused as a whole (4xx, RFC 8620 section 3.6),
/// or a method error other than `serverPartialFail` (section 3.6.2).
fn never_processed(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<super::MethodError>()
            .is_some_and(|m| m.error["type"] != "serverPartialFail")
            || cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.status().is_some_and(|s| s.is_client_error()))
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailMeta {
    pub id: String,
    #[serde(default)]
    pub mailbox_ids: HashMap<String, bool>,
    #[serde(default)]
    pub keywords: HashMap<String, bool>,
    pub received_at: String,
}

/// Which body values `Email/get` should include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyValues {
    None,
    Text,
    Html,
    Both,
}

impl JmapClient {
    /// One page of emails matching `filter`, newest first, plus the total.
    pub async fn query_email_page(
        &self,
        filter: Value,
        position: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<String>, usize)> {
        let mut args = self
            .call(
                "Email/query",
                json!({
                    "accountId": self.account_for(MAIL)?,
                    "filter": filter,
                    "sort": [{ "property": "receivedAt", "isAscending": false }],
                    "position": position,
                    "limit": limit,
                    "calculateTotal": true,
                }),
            )
            .await?;
        let ids = serde_json::from_value(args["ids"].take())?;
        Ok((ids, args["total"].as_u64().unwrap_or(0) as usize))
    }

    /// Mailboxes, keywords and dates of emails, in batches the server accepts.
    pub async fn email_meta(&self, ids: &[&str]) -> anyhow::Result<Vec<EmailMeta>> {
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(self.max_objects_in_get()) {
            let mut args = self
                .call(
                    "Email/get",
                    json!({
                        "accountId": self.account_for(MAIL)?,
                        "ids": batch,
                        "properties": ["id", "mailboxIds", "keywords", "receivedAt"],
                    }),
                )
                .await?;
            out.extend(serde_json::from_value::<Vec<EmailMeta>>(
                args["list"].take(),
            )?);
        }
        Ok(out)
    }

    /// Other accounts with mail (mailboxes shared with the user), with
    /// their names.
    pub fn shared_mail_accounts(&self) -> Vec<(&str, &str)> {
        self.shared_accounts(MAIL)
    }

    pub async fn mailboxes(&self) -> anyhow::Result<Vec<Mailbox>> {
        self.get_all(MAILBOX, &["id", "name", "parentId", "role", "sortOrder"])
            .await
    }

    /// All email ids matching `filter`, newest first.
    pub async fn query_email_ids(&self, filter: Value) -> anyhow::Result<Vec<String>> {
        let sort = json!([{ "property": "receivedAt", "isAscending": false }]);
        self.query_all(
            &[CORE, MAIL],
            self.account_for(MAIL)?,
            "Email/query",
            filter,
            Some(sort),
        )
        .await
    }

    /// Fetches emails with headers, structure and optionally body values
    /// truncated to `max_body_bytes` (0 means no limit).
    pub async fn get_emails(
        &self,
        ids: &[&str],
        body_values: BodyValues,
        max_body_bytes: u32,
    ) -> anyhow::Result<Vec<Email>> {
        let mut emails = Vec::with_capacity(ids.len());
        // A sync window may be larger than the server's maxObjectsInGet.
        for batch in ids.chunks(self.max_objects_in_get()) {
            let mut args = self
                .call(
                    "Email/get",
                    json!({
                        "accountId": self.account_for(MAIL)?,
                        "ids": batch,
                        "properties": [
                            "id", "blobId", "mailboxIds", "keywords", "size", "receivedAt", "subject",
                            "from", "to", "cc", "replyTo", "htmlBody", "textBody",
                            "attachments", "bodyValues", "messageId", "inReplyTo", "references",
                            "threadId", "preview", "header:Importance:asText", "header:X-Priority:asText",
                        ],
                        "bodyProperties": ["partId", "blobId", "size", "name", "type", "cid", "disposition"],
                        "fetchTextBodyValues": matches!(body_values, BodyValues::Text | BodyValues::Both),
                        "fetchHTMLBodyValues": matches!(body_values, BodyValues::Html | BodyValues::Both),
                        "maxBodyValueBytes": max_body_bytes,
                    }),
                )
                .await?;
            let list: Vec<Email> = serde_json::from_value(args["list"].take())?;
            emails.extend(list);
        }
        Ok(emails)
    }

    /// Stores a message that is about to be sent in Sent (Drafts without
    /// one), marked read, and returns its id.
    pub async fn store_outgoing(&self, raw: Vec<u8>) -> anyhow::Result<String> {
        let mailboxes = self.mailboxes().await?;
        let sent = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("sent"))
            .or_else(|| {
                mailboxes
                    .iter()
                    .find(|m| m.role.as_deref() == Some("drafts"))
            })
            .context("no Sent or Drafts mailbox to store the message in")?;
        self.import_email(raw, &sent.id, &["$seen"]).await
    }

    /// Submits a stored message with an explicit envelope, so a Bcc header
    /// never reaches the recipients. An error only for a rejected login.
    pub async fn submit(
        &self,
        email_id: &str,
        mail_from: &str,
        rcpt_to: &[String],
    ) -> anyhow::Result<Submission> {
        let identity = match self.identity_for(mail_from).await {
            Ok(identity) => identity,
            Err(e) if super::is_unauthorized(&e) => return Err(e),
            Err(e) => return Ok(Submission::Rejected(e, None)),
        };
        let args = json!({
            "accountId": self.account_id()?,
            "create": { "s": {
                "identityId": identity,
                "emailId": email_id,
                "envelope": {
                    "mailFrom": { "email": mail_from },
                    "rcptTo": rcpt_to.iter().map(|r| json!({ "email": r })).collect::<Vec<_>>(),
                },
            }},
        });
        match self.call("EmailSubmission/set", args).await {
            Ok(args) if args["created"]["s"].is_object() => Ok(Submission::Sent),
            Ok(args) => {
                let failure = &args["notCreated"]["s"];
                Ok(Submission::Rejected(
                    anyhow::anyhow!("the server rejected the message: {failure}"),
                    failure["type"].as_str().map(str::to_owned),
                ))
            }
            Err(e) if super::is_unauthorized(&e) => Err(e),
            Err(e) if never_processed(&e) => Ok(Submission::Rejected(e, None)),
            Err(e) => Ok(Submission::Unknown(e)),
        }
    }

    /// Whether a stored message has been submitted.
    pub async fn submitted(&self, email_id: &str) -> anyhow::Result<bool> {
        let args = self
            .call(
                "EmailSubmission/query",
                json!({
                    "accountId": self.account_id()?,
                    "filter": { "emailIds": [email_id] },
                    "limit": 1,
                }),
            )
            .await?;
        Ok(args["ids"].as_array().is_some_and(|ids| !ids.is_empty()))
    }

    /// Destroys a message; one that is already gone is no error.
    pub async fn destroy_email(&self, email_id: &str) -> anyhow::Result<()> {
        self.set(
            EMAIL,
            SetRequest::default().destroy(vec![email_id.to_owned()]),
        )
        .await?;
        Ok(())
    }

    /// Stores a raw message in a mailbox (a draft) and returns its id.
    pub async fn import_email(
        &self,
        raw: Vec<u8>,
        mailbox_id: &str,
        keywords: &[&str],
    ) -> anyhow::Result<String> {
        let blob_id = self.upload(raw, "message/rfc822").await?;
        let keywords: Map<String, Value> = keywords
            .iter()
            .map(|k| ((*k).to_owned(), Value::Bool(true)))
            .collect();
        let args = self
            .call(
                "Email/import",
                json!({
                    "accountId": self.account_for(MAIL)?,
                    "emails": { "d": { "blobId": blob_id, "mailboxIds": { mailbox_id: true }, "keywords": keywords } },
                }),
            )
            .await?;
        args["created"]["d"]["id"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("Email/import refused: {}", args["notCreated"]))
    }

    /// Copies an email from another account into `mailbox_id` of this
    /// client's account (`Email/copy`); returns its new id. The caller
    /// destroys the original with a plain `Email/set`: Stalwart does not
    /// record `onSuccessDestroyOriginal` destroys in `Email/changes`.
    pub async fn copy_email_from(
        &self,
        from_account: &str,
        id: &str,
        mailbox_id: &str,
    ) -> anyhow::Result<String> {
        let args = self
            .call(
                "Email/copy",
                json!({
                    "fromAccountId": from_account,
                    "accountId": self.account_for(MAIL)?,
                    "create": { "c": { "id": id, "mailboxIds": { mailbox_id: true } } },
                }),
            )
            .await?;
        args["created"]["c"]["id"]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("Email/copy refused: {}", args["notCreated"]))
    }

    /// The addresses of the account's sending identities (its primary
    /// address and aliases), cached for `PEOPLE_TTL`.
    pub async fn own_addresses(&self) -> anyhow::Result<Vec<String>> {
        let account = self.account_id()?.to_owned();
        if let Some(addresses) = self.caches.identities.get(&account) {
            return Ok(addresses);
        }
        let args = self
            .call(
                "Identity/get",
                json!({ "accountId": account, "ids": null, "properties": ["email"] }),
            )
            .await?;
        let mut addresses: Vec<String> = Vec::new();
        for identity in args["list"].as_array().into_iter().flatten() {
            if let Some(email) = identity["email"].as_str().filter(|e| e.contains('@'))
                && !addresses.iter().any(|a| a.eq_ignore_ascii_case(email))
            {
                addresses.push(email.to_owned());
            }
        }
        self.caches.identities.insert(account, addresses.clone());
        Ok(addresses)
    }

    /// The identity whose address matches `email`, or the first one.
    async fn identity_for(&self, email: &str) -> anyhow::Result<String> {
        let args = self
            .call(
                "Identity/get",
                json!({ "accountId": self.account_id()?, "ids": null }),
            )
            .await?;
        let identities = args["list"].as_array().cloned().unwrap_or_default();
        identities
            .iter()
            .find(|i| {
                i["email"]
                    .as_str()
                    .is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
            .or_else(|| identities.first())
            .and_then(|i| i["id"].as_str())
            .map(str::to_owned)
            .context("account has no sending identity")
    }

    /// The account's vacation response (RFC 8621 section 8), `None` when the
    /// server does not offer one.
    pub async fn vacation_response(&self) -> anyhow::Result<Option<Value>> {
        let Some(account) = self.session.primary_accounts.get(VACATION) else {
            return Ok(None);
        };
        let mut args = self
            .call_with(
                VACATION,
                "VacationResponse/get",
                json!({ "accountId": account, "ids": ["singleton"] }),
            )
            .await?;
        Ok(args["list"].as_array_mut().and_then(|l| l.pop()))
    }

    /// Changes the vacation response; false when the server has none.
    pub async fn set_vacation_response(&self, patch: Map<String, Value>) -> anyhow::Result<bool> {
        let Some(account) = self.session.primary_accounts.get(VACATION) else {
            return Ok(false);
        };
        let args = self
            .call_with(
                VACATION,
                "VacationResponse/set",
                json!({ "accountId": account, "update": { "singleton": patch } }),
            )
            .await?;
        let result: SetResult = serde_json::from_value(args)?;
        if let Some(error) = result.error("singleton") {
            bail!("VacationResponse/set rejected: {error}");
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method_error(kind: &str) -> anyhow::Error {
        super::super::MethodError {
            method: "EmailSubmission/set".to_owned(),
            error: json!({ "type": kind }),
        }
        .into()
    }

    #[test]
    fn only_certain_failures_count_as_not_sent() {
        assert!(never_processed(&method_error("forbidden")));
        // RFC 8620 3.6.2: a partial failure may have changed state.
        assert!(!never_processed(&method_error("serverPartialFail")));
        assert!(!never_processed(&anyhow::anyhow!("connection reset")));
    }
}
