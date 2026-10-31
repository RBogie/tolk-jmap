//! MoveItems (MS-ASCMD 2.2.1.12): moving emails between folders.

use serde_json::Map;

use super::{
    CommandError, CommandResult, Request,
    sync::{self, Folder, FolderKind},
};
use crate::{
    jmap::{self, JmapClient},
    wbxml::{Element, cp::move_items as mv},
};

mod status {
    pub const INVALID_SOURCE: &str = "1";
    pub const INVALID_DESTINATION: &str = "2";
    pub const SUCCESS: &str = "3";
    pub const SAME_FOLDER: &str = "4";
    pub const FAILED: &str = "5";
}

pub async fn handle(req: &Request<'_>, jmap: &JmapClient) -> CommandResult {
    let body = req.body_with_root(mv::MoveItems)?;
    let mut out = Element::new(mv::MoveItems);
    for request in body.find_all(mv::Move) {
        let reply = match move_one(req, jmap, request).await {
            Ok(reply) => reply,
            Err(CommandError::Backend(e)) if !jmap::is_unauthorized(&e) => {
                tracing::warn!(error = format!("{e:#}"), "move failed");
                Element::new(mv::Response)
                    .child(Element::with_text(
                        mv::SrcMsgId,
                        request.child_text(mv::SrcMsgId).unwrap_or_default(),
                    ))
                    .child(Element::with_text(mv::Status, status::FAILED))
            }
            Err(e) => return Err(e),
        };
        out.push(reply);
    }
    Ok(out)
}

async fn move_one(req: &Request<'_>, jmap: &JmapClient, request: &Element) -> CommandResult {
    let src_msg = request.child_text(mv::SrcMsgId).unwrap_or_default();
    let src_fld = request.child_text(mv::SrcFldId).unwrap_or_default();
    let dst_fld = request.child_text(mv::DstFldId).unwrap_or_default();
    let reply = |status: &str| {
        Element::new(mv::Response)
            .child(Element::with_text(mv::SrcMsgId, src_msg))
            .child(Element::with_text(mv::Status, status))
    };

    // Only emails move between folders.
    let mail = |f: &Folder| f.kind == FolderKind::Mail;
    let Some(src) = sync::resolve(req, src_fld)?.filter(mail) else {
        return Ok(reply(status::INVALID_SOURCE));
    };
    let Some(dst) = sync::resolve(req, dst_fld)?.filter(mail) else {
        return Ok(reply(status::INVALID_DESTINATION));
    };
    if src.id == dst.id {
        return Ok(reply(status::SAME_FOLDER));
    }
    let Some(email_id) = src.email_for(req, src_msg)? else {
        return Ok(reply(status::INVALID_SOURCE));
    };

    let new_id = if src.account == dst.account {
        let mut update = Map::new();
        update.insert(
            email_id.clone(),
            sync::move_patch(&src.jmap_id, &dst.jmap_id),
        );
        let result = src
            .client(jmap)
            .set(
                crate::jmap::EMAIL,
                crate::jmap::SetRequest::default().update(update),
            )
            .await?;
        if let Some(error) = result.error(&email_id) {
            tracing::warn!(email = email_id, error, "move rejected");
            return Ok(reply(status::FAILED));
        }
        email_id.clone()
    } else {
        // Between the user's account and a shared one: copied over, and
        // the original destroyed.
        let from = src.account.as_deref().unwrap_or(jmap.account_for_mail()?);
        let new_id = match dst
            .client(jmap)
            .copy_email_from(from, &email_id, &dst.jmap_id)
            .await
        {
            Ok(new_id) => new_id,
            Err(e) if crate::jmap::is_unauthorized(&e) => return Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    email = email_id,
                    error = format!("{e:#}"),
                    "move between accounts failed"
                );
                return Ok(reply(status::FAILED));
            }
        };
        let destroyed = src
            .client(jmap)
            .set(
                crate::jmap::EMAIL,
                crate::jmap::SetRequest::default().destroy(vec![email_id.clone()]),
            )
            .await?;
        if let Some(error) = destroyed.error(&email_id) {
            // The copy stays: better twice than lost.
            tracing::warn!(
                email = email_id,
                error,
                "moved copy kept, original not removed"
            );
        }
        new_id
    };

    sync::record_move(req, src_fld, dst_fld, &email_id, &new_id)?;
    let dst_item = dst_server_id(req, &dst, &new_id)?;
    Ok(reply(status::SUCCESS).child(Element::with_text(mv::DstMsgId, dst_item)))
}

fn dst_server_id(req: &Request, dst: &Folder, email_id: &str) -> anyhow::Result<String> {
    let ids = req.store.item_ids(req.user, dst.id, &[email_id])?;
    Ok(dst.server_id(ids[email_id]))
}
