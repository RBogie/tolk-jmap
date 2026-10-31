//! The request to execute: the cached last Sync for empty and partial
//! Syncs, and how long a Sync may wait for changes.

use super::*;

/// How long to wait for changes: `<Wait>` in minutes or
/// `<HeartbeatInterval>` in seconds; out of range is status 14.
pub(super) fn wait_time(request: &Element) -> Result<Option<Duration>, Element> {
    let seconds = match (
        parse::<u64>(request.child_text(airsync::Wait)),
        parse::<u64>(request.child_text(airsync::HeartbeatInterval)),
    ) {
        (Some(minutes), _) => minutes.saturating_mul(60),
        (None, Some(seconds)) => seconds,
        (None, None) => return Ok(None),
    };
    if !(MIN_WAIT..=MAX_WAIT).contains(&seconds) {
        let limit = seconds.clamp(MIN_WAIT, MAX_WAIT);
        let limit = match request.find(airsync::Wait) {
            Some(_) => limit / 60,
            None => limit,
        };
        return Err(sync_status(status::INVALID_WAIT)
            .child(Element::with_text(airsync::Limit, limit.to_string())));
    }
    Ok(Some(Duration::from_secs(seconds)))
}

/// The request to execute: the body itself, the cached last request for an
/// empty Sync, or the cached one with a partial Sync merged in. `Err` is the
/// status-13 reply when there is nothing to repeat or merge into.
pub(super) fn full_request(req: &Request) -> Result<Result<Element, Element>, CommandError> {
    let cached = || -> Result<Option<Element>, CommandError> {
        req.store
            .sync_request(req.user, req.device_id)?
            .and_then(|bytes| wbxml::decode(&bytes).ok())
            .map(|cached| with_current_keys(req, cached))
            .transpose()
    };
    let request = match &req.body {
        None => match cached()? {
            Some(cached) => cached,
            None => return Ok(Err(sync_status(status::INCOMPLETE_REQUEST))),
        },
        Some(_) => {
            let body = req.body_with_root(airsync::Sync)?;
            if body.find(airsync::Partial).is_some() || body.find(airsync::Collections).is_none() {
                match cached()? {
                    Some(cached) => merge(cached, body),
                    None => return Ok(Err(sync_status(status::INCOMPLETE_REQUEST))),
                }
            } else {
                body.clone()
            }
        }
    };
    let remembered = without_commands(&request);
    req.store
        .save_sync_request(req.user, req.device_id, &wbxml::encode(&remembered))?;
    Ok(Ok(request))
}

/// The cached request with each collection's SyncKey set to the key the
/// device got last (what it would send itself).
pub(super) fn with_current_keys(
    req: &Request,
    mut cached: Element,
) -> Result<Element, CommandError> {
    for node in collection_nodes(&mut cached) {
        let Some(id) = node.child_text(airsync::CollectionId).map(str::to_owned) else {
            continue;
        };
        if let Some(state) = req
            .store
            .collection_sync_state(req.user, req.device_id, &id)?
        {
            set_child_text(node, airsync::SyncKey, &state.sync_key.to_string());
        }
    }
    Ok(cached)
}

/// `cached` with the partial request's settings and collections on top.
pub(super) fn merge(cached: Element, partial: &Element) -> Element {
    let mut merged = Element::new(airsync::Sync);
    let has_wait =
        partial.find(airsync::Wait).is_some() || partial.find(airsync::HeartbeatInterval).is_some();
    for tag in [
        airsync::Wait,
        airsync::HeartbeatInterval,
        airsync::WindowSize,
    ] {
        let from_partial = partial.find(tag);
        let wait_tag = tag != airsync::WindowSize;
        let chosen = match from_partial {
            Some(element) => Some(element),
            // A new Wait replaces a cached HeartbeatInterval and vice versa.
            None if wait_tag && has_wait => None,
            None => cached.find(tag),
        };
        if let Some(element) = chosen {
            merged.push(element.clone());
        }
    }
    let mut collections = Element::new(airsync::Collections);
    let new: Vec<&Element> = partial
        .find(airsync::Collections)
        .into_iter()
        .flat_map(|c| c.find_all(airsync::Collection))
        .collect();
    let new_ids: HashSet<&str> = new
        .iter()
        .filter_map(|c| c.child_text(airsync::CollectionId))
        .collect();
    for collection in new {
        collections.push(collection.clone());
    }
    for collection in cached
        .find(airsync::Collections)
        .into_iter()
        .flat_map(|c| c.find_all(airsync::Collection))
        .filter(|c| {
            !c.child_text(airsync::CollectionId)
                .is_some_and(|id| new_ids.contains(id))
        })
    {
        collections.push(collection.clone());
    }
    merged.push(collections);
    merged
}

/// The request without client commands, which must never run twice.
pub(super) fn without_commands(request: &Element) -> Element {
    let mut copy = request.clone();
    for node in collection_nodes(&mut copy) {
        node.children
            .retain(|child| !matches!(child, Node::Element(e) if e.tag == airsync::Commands));
    }
    copy
}

pub(super) fn collection_nodes(request: &mut Element) -> Vec<&mut Element> {
    request
        .children
        .iter_mut()
        .filter_map(|n| match n {
            Node::Element(e) if e.tag == airsync::Collections => Some(e),
            _ => None,
        })
        .flat_map(|collections| collections.children.iter_mut())
        .filter_map(|n| match n {
            Node::Element(e) if e.tag == airsync::Collection => Some(e),
            _ => None,
        })
        .collect()
}

pub(super) fn set_child_text(element: &mut Element, tag: Tag, value: &str) {
    for node in &mut element.children {
        if let Node::Element(child) = node
            && child.tag == tag
        {
            *child = Element::with_text(tag, value);
            return;
        }
    }
    element.push(Element::with_text(tag, value));
}
