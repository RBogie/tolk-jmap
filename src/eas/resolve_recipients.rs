//! ResolveRecipients (MS-ASCMD 2.2.1.15): turns typed names or addresses
//! into people, from the same sources as recipient search (address books
//! and correspondents). Certificates are never available (no S/MIME), and
//! free/busy is not offered.

use super::{CommandResult, Request};
use crate::{
    jmap::{JmapClient, Person},
    wbxml::{Element, cp::resolve_recipients as rr},
};

mod status {
    pub const OK: &str = "1";
    pub const PROTOCOL_ERROR: &str = "5";
}

/// Per `<To>`: one match, several, or none.
mod response_status {
    pub const RESOLVED: &str = "1";
    pub const AMBIGUOUS: &str = "2";
    pub const NOT_RESOLVED: &str = "4";
}

/// No certificate found for the recipient.
const NO_CERTIFICATE: &str = "7";
const DEFAULT_AMBIGUOUS: usize = 19;
const MAX_ROUNDS: usize = 100;

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(rr::ResolveRecipients)?;
    let options = body.find(rr::Options);
    let wants_certificates = options
        .and_then(|o| o.child_text(rr::CertificateRetrieval))
        .is_some_and(|c| c != "1");
    let max_ambiguous = options
        .and_then(|o| o.child_text(rr::MaxAmbiguousRecipients))
        .and_then(|m| m.trim().parse().ok())
        .unwrap_or(DEFAULT_AMBIGUOUS)
        .min(DEFAULT_AMBIGUOUS);
    let queries: Vec<&str> = body.find_all(rr::To).filter_map(Element::text).collect();
    if queries.is_empty() || queries.len() > MAX_ROUNDS {
        return Ok(reply(status::PROTOCOL_ERROR));
    }
    let mut response = reply(status::OK);
    for query in queries {
        let people = resolve(jmap, query, max_ambiguous).await?;
        let state = match people.len() {
            0 => response_status::NOT_RESOLVED,
            1 => response_status::RESOLVED,
            _ => response_status::AMBIGUOUS,
        };
        let mut entry = Element::new(rr::Response)
            .child(Element::with_text(rr::To, query))
            .child(Element::with_text(rr::Status, state))
            .child(Element::with_text(
                rr::RecipientCount,
                people.len().to_string(),
            ));
        for person in &people {
            let mut recipient = Element::new(rr::Recipient)
                // 2: from the user's contacts, not a directory.
                .child(Element::with_text(rr::Type, "2"))
                .child(Element::with_text(
                    rr::DisplayName,
                    person.display_name.as_str(),
                ))
                .child(Element::with_text(rr::EmailAddress, person.email.as_str()));
            if wants_certificates {
                recipient.push(
                    Element::new(rr::Certificates)
                        .child(Element::with_text(rr::Status, NO_CERTIFICATE))
                        .child(Element::with_text(rr::CertificateCount, "0")),
                );
            }
            entry.push(recipient);
        }
        response.push(entry);
    }
    Ok(response)
}

/// An exact address wins; otherwise everyone whose name or address starts
/// with the query.
async fn resolve(jmap: &JmapClient, query: &str, limit: usize) -> anyhow::Result<Vec<Person>> {
    let query = query.trim();
    let people = jmap.search_people(query, limit.max(1)).await?;
    if let Some(exact) = people.iter().find(|p| p.email.eq_ignore_ascii_case(query)) {
        return Ok(vec![exact.clone()]);
    }
    Ok(people.into_iter().take(limit).collect())
}

fn reply(status: &str) -> Element {
    Element::new(rr::ResolveRecipients).child(Element::with_text(rr::Status, status))
}
