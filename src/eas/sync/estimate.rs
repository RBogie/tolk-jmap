//! GetItemEstimate: how many changes a Sync would deliver.

use super::*;

pub(super) mod estimate_status {
    pub const OK: &str = "1";
    pub const INVALID_COLLECTION: &str = "2";
    pub const NOT_PRIMED: &str = "3";
    pub const INVALID_SYNC_KEY: &str = "4";
}

pub async fn estimate(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(item_estimate::GetItemEstimate)?;
    let collections = body
        .find(item_estimate::Collections)
        .ok_or_else(|| CommandError::bad("GetItemEstimate without Collections"))?;
    let mut out = Element::new(item_estimate::GetItemEstimate);
    for collection in collections.find_all(item_estimate::Collection) {
        let reply = match estimate_collection(req, jmap, collection).await {
            Ok(reply) => reply,
            Err(CommandError::Backend(e)) if !jmap::is_unauthorized(&e) => {
                tracing::warn!(error = format!("{e:#}"), "estimate failed");
                Element::new(item_estimate::Response)
                    .child(Element::with_text(item_estimate::Status, "2"))
            }
            Err(e) => return Err(e),
        };
        out.push(reply);
    }
    Ok(out)
}

pub(super) async fn estimate_collection(
    req: &Request<'_>,
    jmap: &JmapClient,
    collection: &Element,
) -> CommandResult {
    let collection_id = collection
        .child_text(item_estimate::CollectionId)
        .unwrap_or_default();
    let reply = |status: &str| {
        Element::new(item_estimate::Response)
            .child(Element::with_text(item_estimate::Status, status))
    };
    let Some(folder) = resolve(req, collection_id)? else {
        return Ok(reply(estimate_status::INVALID_COLLECTION));
    };
    let key: u32 = parse(collection.child_text(airsync::SyncKey)).unwrap_or(0);
    if key == 0 {
        return Ok(reply(estimate_status::NOT_PRIMED));
    }
    let state = req
        .store
        .collection_sync_state(req.user, req.device_id, collection_id)?;
    let Some((base, _)) = base_snapshot(state.as_ref(), key)? else {
        return Ok(reply(estimate_status::INVALID_SYNC_KEY));
    };
    let options = SyncOptions::from_collection(collection)
        .or_else(|| SyncOptions::stored(state.as_ref()))
        .unwrap_or_default();
    let current = current_items(req, jmap, collection_id, &folder, &options).await?;
    let estimate = Plan::new(&base, &current).len();
    Ok(reply(estimate_status::OK).child(
        Element::new(item_estimate::Collection)
            .child(Element::with_text(
                item_estimate::CollectionId,
                collection_id,
            ))
            .child(Element::with_text(
                item_estimate::Estimate,
                estimate.to_string(),
            )),
    ))
}
