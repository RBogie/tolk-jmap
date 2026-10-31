//! Find (MS-ASCMD 2.2.1.2, EAS 16.1): the newer search command, for mail
//! (`MailBoxSearchCriterion`, with a free text query) and the GAL
//! (`GalSearchCriterion`). Answered like Search, with `Email/query` and the
//! people lookup; results carry a ServerId and CollectionId instead of a
//! LongId.

use std::collections::HashSet;

use serde_json::json;

use super::{
    CommandResult, Request,
    email::{self, BodyPreference},
    search, sync,
};
use crate::{
    jmap::{self, JmapClient},
    wbxml::{
        Element, Tag,
        cp::{airsync, email as email_cp, email2, find as fp, item_operations as io},
    },
};

mod status {
    pub const OK: &str = "1";
    pub const INVALID: &str = "2";
    pub const FOLDER_SYNC_REQUIRED: &str = "3";
    pub const RANGE_NOT_FROM_ZERO: &str = "4";
}

/// GAL queries shorter than this are refused (the schema's minimum).
const MIN_GAL_QUERY: usize = 4;

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(fp::Find)?;
    let Some(execute) = body.find(fp::ExecuteSearch) else {
        return Ok(find_status(status::INVALID));
    };
    let (criterion, store) = match (
        execute.find(fp::MailBoxSearchCriterion),
        execute.find(fp::GalSearchCriterion),
    ) {
        (Some(c), None) => (c, "Mailbox"),
        (None, Some(c)) => (c, "GAL"),
        _ => return Ok(find_status(status::INVALID)),
    };
    let options = criterion.find(fp::Options);
    let max = req.caps.search_max_results;
    let range = match options.and_then(|o| o.child_text(fp::Range)) {
        Some(r) => search::parse_range(r, max),
        None => Some((0, max - 1)),
    };
    let Some((start, end)) = range else {
        return Ok(find_status(status::INVALID));
    };
    if start != 0 {
        return Ok(answer(response_status(store, status::RANGE_NOT_FROM_ZERO)));
    }
    let Some(query) = criterion.find(fp::Query) else {
        return Ok(find_status(status::INVALID));
    };

    let result = if store == "GAL" {
        find_gal(jmap, query.text().unwrap_or_default().trim(), end).await
    } else {
        let deep = options.is_some_and(|o| o.find(fp::DeepTraversal).is_some());
        match find_mail(req, jmap, query, deep, end).await {
            Ok(None) => return Ok(find_status(status::FOLDER_SYNC_REQUIRED)),
            Ok(Some(response)) => Ok(response),
            Err(e) => Err(e),
        }
    };
    match result {
        Ok(response) => Ok(answer(response)),
        Err(e) if jmap::is_unauthorized(&e) => Err(e.into()),
        Err(e) => {
            tracing::warn!(store, error = format!("{e:#}"), "find failed");
            Ok(find_status(status::INVALID))
        }
    }
}

fn find_status(status: &str) -> Element {
    Element::new(fp::Find).child(Element::with_text(fp::Status, status))
}

fn answer(response: Element) -> Element {
    find_status(status::OK).child(response)
}

fn response_status(store: &str, status: &str) -> Element {
    Element::new(fp::Response)
        .child(Element::with_text(io::Store, store))
        .child(Element::with_text(fp::Status, status))
}

/// The Response with its results (all from 0), range and total.
fn response(store: &str, results: Vec<Element>, total: usize) -> Element {
    let mut response = response_status(store, status::OK);
    let count = results.len();
    for result in results {
        response.push(result);
    }
    if count > 0 {
        response.push(Element::with_text(fp::Range, format!("0-{}", count - 1)));
    }
    response.push(Element::with_text(fp::Total, total.to_string()));
    response
}

async fn find_gal(jmap: &JmapClient, query: &str, end: usize) -> anyhow::Result<Element> {
    if query.chars().count() < MIN_GAL_QUERY {
        return Ok(response_status("GAL", status::INVALID));
    }
    let people = jmap.search_people(query, end + 1).await?;
    let total = people.len();
    let results = people
        .iter()
        .map(|person| {
            Element::new(fp::Result).child(search::gal_properties(person, fp::Properties))
        })
        .collect();
    Ok(response("GAL", results, total))
}

/// `None` when the query names a folder the device should not know.
async fn find_mail(
    req: &Request<'_>,
    jmap: &JmapClient,
    query: &Element,
    deep: bool,
    end: usize,
) -> anyhow::Result<Option<Element>> {
    if query
        .child_text(airsync::Class)
        .is_some_and(|class| class != "Email")
    {
        return Ok(Some(response_status("Mailbox", status::INVALID)));
    }
    let mut filter = json!({});
    if let Some(text) = query
        .child_text(fp::FreeText)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        filter["text"] = text.into();
    }
    let mut folder = None;
    let mut client = std::borrow::Cow::Borrowed(jmap);
    if let Some(id) = query.child_text(airsync::CollectionId) {
        let Some(found) = sync::resolve(req, id)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .filter(|f| f.kind == sync::FolderKind::Mail)
        else {
            return Ok(None);
        };
        client = found.client(jmap);
        filter = if deep {
            let mailboxes = client.mailboxes().await?;
            let ids = with_subfolders(&mailboxes, &found.jmap_id);
            let conditions: Vec<_> = ids
                .iter()
                .map(|id| {
                    let mut condition = filter.clone();
                    condition["inMailbox"] = id.as_str().into();
                    condition
                })
                .collect();
            json!({ "operator": "OR", "conditions": conditions })
        } else {
            filter["inMailbox"] = found.jmap_id.clone().into();
            filter
        };
        // Deep results can be in any of the subfolders.
        folder = (!deep).then_some(found);
    }

    let body = BodyPreference {
        body_type: email::body_type::PLAIN,
        truncation_size: Some(0),
        all_or_none: false,
        preview: None,
    };
    let jmap = &*client;
    let (hits, total) =
        search::mailbox_page(req, jmap, filter, folder.as_ref(), 0, end, &body).await?;
    let mut results = Vec::with_capacity(hits.len());
    for hit in hits {
        let data = email::application_data(&hit.email, &body, jmap).await?;
        results.push(
            Element::new(fp::Result)
                .child(Element::with_text(airsync::Class, "Email"))
                .child(Element::with_text(
                    airsync::ServerId,
                    format!("{}:{}", hit.folder_id, hit.item),
                ))
                .child(Element::with_text(
                    airsync::CollectionId,
                    hit.folder_id.to_string(),
                ))
                .child(properties(&hit.email, &data)),
        );
    }
    tracing::debug!(total, returned = results.len(), "find");
    Ok(Some(response("Mailbox", results, total)))
}

/// The mailbox and every mailbox below it.
fn with_subfolders(mailboxes: &[jmap::Mailbox], root: &str) -> Vec<String> {
    let mut ids = vec![root.to_owned()];
    let mut seen: HashSet<&str> = HashSet::from([root]);
    let mut i = 0;
    while i < ids.len() {
        let parent = ids[i].clone();
        for mailbox in mailboxes {
            if mailbox.parent_id.as_deref() == Some(parent.as_str())
                && seen.insert(mailbox.id.as_str())
            {
                ids.push(mailbox.id.clone());
            }
        }
        i += 1;
    }
    ids
}

/// The Find properties of a message, taken from its Sync data where they
/// are the same.
fn properties(msg: &jmap::Email, data: &Element) -> Element {
    const FROM_SYNC: [Tag; 7] = [
        email_cp::Subject,
        email_cp::DateReceived,
        email_cp::DisplayTo,
        email_cp::Importance,
        email_cp::Read,
        email_cp::From,
        email2::IsDraft,
    ];
    let mut properties = Element::new(fp::Properties);
    for child in data.elements().filter(|e| FROM_SYNC.contains(&e.tag)) {
        properties.push(child.clone());
    }
    if let Some(cc) = email::display_names(msg.cc.as_deref()) {
        properties.push(Element::with_text(fp::DisplayCc, cc));
    }
    if let Some(preview) = msg.preview.as_deref().filter(|p| !p.is_empty()) {
        properties.push(Element::with_text(fp::Preview, preview));
    }
    let attachments = msg
        .attachments
        .iter()
        .any(|a| a.disposition.as_deref() != Some("inline"));
    properties.push(Element::with_text(
        fp::HasAttachments,
        if attachments { "1" } else { "0" },
    ));
    properties
}
