//! Sending a message exactly once, also when the device retries.
//!
//! A send is keyed (a SendMail ClientId, or a draft) and goes in two steps,
//! both recorded: the message is stored in Sent, then submitted. When the
//! submission's outcome is unknown (the connection to the JMAP server broke
//! after the request went out), the key stays claimed with the stored
//! message, and the device's retry first asks the server whether that
//! message was submitted. A rejected submission leaves nothing in Sent and
//! frees the key.

use super::Request;
use crate::{
    jmap::{JmapClient, Submission},
    store::SendRecord,
};

/// What became of a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Sent now, or found sent by an earlier attempt whose answer was lost.
    Sent,
    /// An earlier request with this key was answered as sent.
    AlreadySent,
    /// Not sent; the device may try again.
    Rejected(Refusal),
    /// Maybe sent; a retry with the same key finds out.
    Unknown,
}

/// Why a message was not sent, as far as the device can act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NoRecipients,
    InvalidRecipients,
    Other,
}

impl Refusal {
    /// From a JMAP `EmailSubmission/set` SetError type.
    fn of(set_error: Option<&str>) -> Self {
        match set_error {
            Some("noRecipients") => Refusal::NoRecipients,
            Some("invalidRecipients") => Refusal::InvalidRecipients,
            _ => Refusal::Other,
        }
    }
}

/// One message to send.
pub struct Message<'a> {
    /// The ClientId or draft the send is keyed by; `None` for no retry
    /// protection.
    pub key: Option<&'a str>,
    pub mail_from: &'a str,
    pub rcpt_to: &'a [String],
    /// Whether the copy in Sent is kept.
    pub keep_copy: bool,
}

/// Sends `raw` once per key. An error only for a rejected login or the
/// state database.
pub async fn send(
    req: &Request<'_>,
    jmap: &JmapClient,
    message: &Message<'_>,
    raw: Vec<u8>,
) -> anyhow::Result<Outcome> {
    let record = match message.key {
        Some(key) => req.store.claim_send(req.user, req.device_id, key)?,
        None => SendRecord::New,
    };
    let stored = match record {
        SendRecord::Sent => return Ok(Outcome::AlreadySent),
        SendRecord::Pending(Some(email_id)) => match jmap.submitted(&email_id).await {
            Ok(true) => {
                tracing::info!(email_id, "an earlier attempt sent the message");
                finish(req, jmap, message, &email_id).await?;
                return Ok(Outcome::Sent);
            }
            Ok(false) => Some(email_id),
            Err(e) if crate::jmap::is_unauthorized(&e) => return Err(e),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "could not check an earlier send");
                return Ok(Outcome::Unknown);
            }
        },
        SendRecord::Pending(None) | SendRecord::New => None,
    };
    let email_id = match stored {
        Some(email_id) => email_id,
        None => match jmap.store_outgoing(raw).await {
            Ok(email_id) => {
                if let Some(key) = message.key {
                    req.store
                        .note_send_email(req.user, req.device_id, key, &email_id)?;
                }
                email_id
            }
            Err(e) => {
                // Nothing was submitted, so a retry is safe.
                release(req, message)?;
                if crate::jmap::is_unauthorized(&e) {
                    return Err(e);
                }
                tracing::warn!(error = format!("{e:#}"), "message not stored for sending");
                return Ok(Outcome::Rejected(Refusal::Other));
            }
        },
    };
    match jmap
        .submit(&email_id, message.mail_from, message.rcpt_to)
        .await
    {
        Ok(Submission::Sent) => {
            finish(req, jmap, message, &email_id).await?;
            Ok(Outcome::Sent)
        }
        Ok(Submission::Rejected(e, set_error)) => {
            tracing::warn!(error = format!("{e:#}"), "message submission rejected");
            if let Err(e) = jmap.destroy_email(&email_id).await {
                tracing::warn!(error = format!("{e:#}"), "rejected message left in Sent");
            }
            release(req, message)?;
            Ok(Outcome::Rejected(Refusal::of(set_error.as_deref())))
        }
        Ok(Submission::Unknown(e)) => {
            tracing::warn!(
                error = format!("{e:#}"),
                email_id,
                "message submission outcome unknown"
            );
            Ok(Outcome::Unknown)
        }
        Err(e) => {
            release(req, message)?;
            Err(e)
        }
    }
}

/// Records a send as done and removes the copy the device did not want.
async fn finish(
    req: &Request<'_>,
    jmap: &JmapClient,
    message: &Message<'_>,
    email_id: &str,
) -> anyhow::Result<()> {
    if let Some(key) = message.key {
        req.store.mark_sent(req.user, req.device_id, key)?;
    }
    if !message.keep_copy
        && let Err(e) = jmap.destroy_email(email_id).await
    {
        tracing::warn!(error = format!("{e:#}"), "sent message's copy left in Sent");
    }
    Ok(())
}

fn release(req: &Request<'_>, message: &Message<'_>) -> anyhow::Result<()> {
    if let Some(key) = message.key {
        req.store.release_send(req.user, req.device_id, key)?;
    }
    Ok(())
}
