//! Search (MS-ASCMD 2.2.1.16) for two stores:
//!
//! - `GAL`: recipient lookup while composing. Answered from the account's
//!   JMAP address books and the server's principal directory.
//! - `Mailbox`: server-side mail search, answered with `Email/query`. Each
//!   result carries a LongId (`folder:item`, like a ServerId) that
//!   ItemOperations, SmartReply and SmartForward accept.

use serde_json::{Value, json};

use super::{
    CommandResult, Request,
    email::{self, BodyPreference},
    sync,
};
use crate::{
    jmap::{self, JmapClient, Person},
    wbxml::{
        Element, Tag,
        cp::{airsync, email as email_cp, gal, search as sp},
    },
};

mod status {
    pub const OK: &str = "1";
    pub const PROTOCOL_ERROR: &str = "2";
    pub const SERVER_ERROR: &str = "3";
}

/// Mailbox results without a BodyPreference get a short plain text body.
const DEFAULT_PREVIEW_BYTES: u32 = 2048;

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(sp::Search)?;
    let Some(store) = body.find(sp::Store) else {
        return Ok(search_status(status::PROTOCOL_ERROR));
    };
    let options = store.find(sp::Options);
    let max = req.caps.search_max_results;
    let range = options
        .and_then(|o| o.child_text(sp::Range))
        .map_or(Some((0, max - 1)), |r| parse_range(r, max));
    let Some(range) = range else {
        return Ok(search_status(status::PROTOCOL_ERROR));
    };

    let name = store.child_text(sp::Name).unwrap_or_default();
    let result = if name.eq_ignore_ascii_case("GAL") {
        search_gal(jmap, store.child_text(sp::Query).unwrap_or_default(), range).await
    } else if name.eq_ignore_ascii_case("Mailbox") {
        search_mailbox(req, jmap, store, options, range).await
    } else {
        Ok(store_status(status::PROTOCOL_ERROR))
    };
    let store = match result {
        Ok(store) => store,
        Err(e) if jmap::is_unauthorized(&e) => return Err(e.into()),
        Err(e) => {
            tracing::warn!(store = name, error = format!("{e:#}"), "search failed");
            store_status(status::SERVER_ERROR)
        }
    };
    Ok(search_status(status::OK).child(Element::new(sp::Response).child(store)))
}

fn search_status(status: &str) -> Element {
    Element::new(sp::Search).child(Element::with_text(sp::Status, status))
}

fn store_status(status: &str) -> Element {
    Element::new(sp::Store).child(Element::with_text(sp::Status, status))
}

/// `start-end` (inclusive), at most `max` long (`SEARCH_MAX_RESULTS`).
pub(super) fn parse_range(range: &str, max: usize) -> Option<(usize, usize)> {
    let (start, end) = range.split_once('-')?;
    let start: usize = start.trim().parse().ok()?;
    let end: usize = end.trim().parse().ok()?;
    (start <= end).then_some((start, end.min(start + max - 1)))
}

/// The store element with results, the returned range and the total.
fn results(results: Vec<Element>, start: usize, total: usize) -> Element {
    let mut store = store_status(status::OK);
    if results.is_empty() {
        store.push(Element::new(sp::Result));
    } else {
        let end = start + results.len() - 1;
        for result in results {
            store.push(result);
        }
        store.push(Element::with_text(sp::Range, format!("{start}-{end}")));
    }
    store.push(Element::with_text(sp::Total, total.to_string()));
    store
}

async fn search_gal(
    jmap: &JmapClient,
    query: &str,
    (start, end): (usize, usize),
) -> anyhow::Result<Element> {
    if query.trim().is_empty() {
        return Ok(results(Vec::new(), start, 0));
    }
    let people = jmap.search_people(query.trim(), end + 1).await?;
    let total = people.len();
    let page: Vec<Element> = people.iter().skip(start).map(gal_result).collect();
    tracing::debug!(query, total, "GAL search");
    Ok(results(page, start, total))
}

pub(super) fn gal_result(person: &Person) -> Element {
    Element::new(sp::Result).child(gal_properties(person, sp::Properties))
}

/// A person's GAL properties inside `tag`.
pub(super) fn gal_properties(person: &Person, tag: Tag) -> Element {
    let mut properties = Element::new(tag);
    let mut add = |tag: Tag, value: Option<&str>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            properties.push(Element::with_text(tag, value));
        }
    };
    add(gal::DisplayName, Some(&person.display_name));
    add(gal::Phone, person.phone.as_deref());
    add(gal::Company, person.company.as_deref());
    add(gal::FirstName, person.first_name.as_deref());
    add(gal::LastName, person.last_name.as_deref());
    add(gal::MobilePhone, person.mobile_phone.as_deref());
    add(gal::EmailAddress, Some(&person.email));
    properties
}

/// The JMAP `Email/query` filter for a Mailbox query, and the folder it is
/// limited to. `None` when the query has parts we cannot express.
fn mailbox_filter(
    req: &Request,
    query: &Element,
) -> anyhow::Result<Option<(Value, Option<sync::Folder>)>> {
    let mut filter = json!({});
    let mut folder = None;
    let conditions: Vec<&Element> = match query.find(sp::And) {
        Some(and) => and.elements().collect(),
        None => query.elements().collect(),
    };
    for condition in conditions {
        match condition.tag {
            sp::FreeText => filter["text"] = condition.text().unwrap_or_default().into(),
            airsync::Class if condition.text() != Some("Email") => return Ok(None),
            airsync::Class => {}
            airsync::CollectionId => {
                let Some(found) = sync::resolve(req, condition.text().unwrap_or_default())
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?
                    .filter(|f| f.kind == sync::FolderKind::Mail)
                else {
                    return Ok(None);
                };
                filter["inMailbox"] = found.jmap_id.clone().into();
                folder = Some(found);
            }
            sp::GreaterThan | sp::LessThan => {
                let Some(value) = condition
                    .child_text(sp::Value)
                    .filter(|_| condition.find(email_cp::DateReceived).is_some())
                else {
                    return Ok(None);
                };
                let key = if condition.tag == sp::GreaterThan {
                    "after"
                } else {
                    "before"
                };
                filter[key] = jmap_date(value).into();
            }
            _ => return Ok(None),
        }
    }
    Ok(Some((filter, folder)))
}

/// EAS dates carry milliseconds (`...T00:00:00.000Z`); JMAP UTCDate must not
/// when they are zero, so they are dropped.
pub(super) fn jmap_date(eas: &str) -> String {
    match (eas.find('.'), eas.strip_suffix('Z')) {
        (Some(dot), Some(_)) => format!("{}Z", &eas[..dot]),
        _ => eas.to_owned(),
    }
}

async fn search_mailbox(
    req: &Request<'_>,
    jmap: &JmapClient,
    store: &Element,
    options: Option<&Element>,
    (start, end): (usize, usize),
) -> anyhow::Result<Element> {
    let Some(query) = store.find(sp::Query) else {
        return Ok(store_status(status::PROTOCOL_ERROR));
    };
    let Some((filter, folder)) = mailbox_filter(req, query)? else {
        return Ok(store_status(status::PROTOCOL_ERROR));
    };
    let preferences = options.map(sync::body_preferences).unwrap_or_default();
    let body = if preferences.is_empty() {
        BodyPreference {
            body_type: email::body_type::PLAIN,
            truncation_size: Some(DEFAULT_PREVIEW_BYTES),
            all_or_none: false,
            preview: None,
        }
    } else {
        BodyPreference::choose(&preferences)
    };

    let jmap = &*match &folder {
        Some(folder) => folder.client(jmap),
        None => std::borrow::Cow::Borrowed(jmap),
    };
    let (page, total) = mailbox_page(req, jmap, filter, folder.as_ref(), start, end, &body).await?;
    let mut found = Vec::new();
    for hit in page {
        let data = email::application_data(&hit.email, &body, jmap).await?;
        found.push(
            Element::new(sp::Result)
                .child(Element::with_text(airsync::Class, "Email"))
                .child(Element::with_text(
                    sp::LongId,
                    format!("{}:{}", hit.folder_id, hit.item),
                ))
                .child(Element::with_text(
                    airsync::CollectionId,
                    hit.folder_id.to_string(),
                ))
                .child(Element {
                    tag: sp::Properties,
                    children: data.children,
                }),
        );
    }
    tracing::debug!(total, returned = found.len(), "mailbox search");
    Ok(results(found, start, total))
}

/// One mail search result with its EAS folder and item ids.
pub(super) struct Hit {
    pub folder_id: u32,
    pub item: u32,
    pub email: jmap::Email,
}

/// The results `start..=end` of an `Email/query` with their EAS ids, and
/// the total; `folder` is the folder the query is limited to, if any.
pub(super) async fn mailbox_page(
    req: &Request<'_>,
    jmap: &JmapClient,
    filter: Value,
    folder: Option<&sync::Folder>,
    start: usize,
    end: usize,
    body: &BodyPreference,
) -> anyhow::Result<(Vec<Hit>, usize)> {
    let (ids, total) = jmap
        .query_email_page(filter, start, end - start + 1)
        .await?;
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut emails = jmap
        .get_emails(
            &id_refs,
            body.body_values(),
            body.truncation_size.unwrap_or(0),
        )
        .await?;
    let mut hits = Vec::new();
    for id in &ids {
        let Some(at) = emails.iter().position(|e| &e.id == id) else {
            continue;
        };
        let email = emails.swap_remove(at);
        let folder_id = match folder {
            Some(folder) => folder.id,
            None => {
                let Some(mailbox) = email
                    .mailbox_ids
                    .iter()
                    .find(|(_, v)| **v)
                    .map(|(k, _)| sync::stored_id("", jmap.shared_account(), k))
                else {
                    continue;
                };
                req.store.folder_server_ids(req.user, &[&mailbox])?[mailbox.as_str()]
            }
        };
        let item = req
            .store
            .item_ids(req.user, folder_id, &[email.id.as_str()])?[&email.id];
        hits.push(Hit {
            folder_id,
            item,
            email,
        });
    }
    Ok((hits, total))
}

/// The folder part of a `folder:item` LongId.
pub fn long_id_folder(long_id: &str) -> Option<&str> {
    long_id.split_once(':').map(|(folder, _)| folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("10-1000", 100), Some((10, 109)));
        assert_eq!(parse_range("10-1000", 5), Some((10, 14)));
        assert_eq!(parse_range("5-1", 100), None);
        assert_eq!(parse_range("nope", 100), None);
    }

    #[test]
    fn converts_dates() {
        assert_eq!(
            jmap_date("2026-10-01T00:00:00.000Z"),
            "2026-10-01T00:00:00Z"
        );
        assert_eq!(jmap_date("2026-10-01T00:00:00Z"), "2026-10-01T00:00:00Z");
    }

    #[test]
    fn builds_gal_results() {
        let person = Person {
            display_name: "Ann Lee".to_owned(),
            last_name: Some("Lee".to_owned()),
            email: "ann@example.com".to_owned(),
            ..Person::default()
        };
        let store = results(vec![gal_result(&person)], 0, 1);
        let result = store
            .find(sp::Result)
            .unwrap()
            .find(sp::Properties)
            .unwrap();
        assert_eq!(result.child_text(gal::DisplayName), Some("Ann Lee"));
        assert_eq!(
            result.child_text(gal::EmailAddress),
            Some("ann@example.com")
        );
        assert_eq!(result.child_text(gal::FirstName), None);
        assert_eq!(store.child_text(sp::Range), Some("0-0"));
        assert_eq!(store.child_text(sp::Total), Some("1"));

        let empty = results(Vec::new(), 0, 0);
        assert!(empty.find(sp::Result).unwrap().children.is_empty());
        assert_eq!(empty.child_text(sp::Range), None);
    }

    #[test]
    fn maps_mailbox_queries_to_jmap_filters() {
        let store = Store::open_in_memory().unwrap();
        store.folder_server_ids("rob", &["inbox-id"]).unwrap();
        let req = Request::for_tests(&store, None);
        let query = Element::new(sp::Query).child(
            Element::new(sp::And)
                .child(Element::with_text(airsync::Class, "Email"))
                .child(Element::with_text(airsync::CollectionId, "1"))
                .child(Element::with_text(sp::FreeText, "invoice"))
                .child(
                    Element::new(sp::GreaterThan)
                        .child(Element::new(email_cp::DateReceived))
                        .child(Element::with_text(sp::Value, "2026-10-01T00:00:00.000Z")),
                ),
        );
        let (filter, folder) = mailbox_filter(&req, &query).unwrap().unwrap();
        assert_eq!(
            filter,
            json!({ "text": "invoice", "inMailbox": "inbox-id", "after": "2026-10-01T00:00:00Z" })
        );
        assert_eq!(folder.unwrap().id, 1);

        let unknown_folder =
            Element::new(sp::Query).child(Element::with_text(airsync::CollectionId, "99"));
        assert!(mailbox_filter(&req, &unknown_folder).unwrap().is_none());
        let contacts =
            Element::new(sp::Query).child(Element::with_text(airsync::Class, "Contacts"));
        assert!(mailbox_filter(&req, &contacts).unwrap().is_none());
    }

    #[test]
    fn replays_gmail_gal_search() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gmail-android/14.1-search-gal.req.wbxml"
        );
        let root = crate::wbxml::decode(&std::fs::read(path).unwrap()).unwrap();
        let store = root.find(sp::Store).unwrap();
        assert_eq!(store.child_text(sp::Name), Some("GAL"));
        assert_eq!(store.child_text(sp::Query), Some("bo"));
        let range = store
            .find(sp::Options)
            .and_then(|o| o.child_text(sp::Range))
            .and_then(|r| parse_range(r, 100));
        assert_eq!(range, Some((0, 9)));
    }

    #[test]
    fn splits_long_ids() {
        assert_eq!(long_id_folder("23:5"), Some("23"));
        assert_eq!(long_id_folder("x"), None);
    }
}
