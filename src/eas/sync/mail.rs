//! Mail specifics of Sync: the device's own changes (flags, deletes,
//! fetches, drafts), the commands sent for new and changed mail, and moves.

use super::*;

/// Deletes, read/flag changes and new emails; returns how many were added.
pub(super) async fn mail_commands(
    req: &Request<'_>,
    jmap: &JmapClient,
    folder: &Folder,
    plan: &Plan,
    body: &BodyPreference,
    snapshot: &mut Snapshot,
    commands: &mut Element,
) -> anyhow::Result<usize> {
    let add_ids: Vec<&str> = plan.adds.iter().map(String::as_str).collect();
    let emails = jmap
        .get_emails(
            &add_ids,
            body.body_values(),
            body.truncation_size.unwrap_or(0),
        )
        .await?;
    let referenced: Vec<&str> = plan
        .deletes
        .iter()
        .chain(plan.changes.iter().map(|(id, _)| id))
        .chain(emails.iter().map(|e| &e.id))
        .map(String::as_str)
        .collect();
    let item_ids = req.store.item_ids(req.user, folder.id, &referenced)?;
    let server_id =
        |id: &str| Element::with_text(airsync::ServerId, folder.server_id(item_ids[id]));

    for id in &plan.deletes {
        snapshot.remove(id);
        commands.push(Element::new(airsync::Delete).child(server_id(id)));
    }
    for (id, flags) in &plan.changes {
        snapshot.insert(id.clone(), *flags);
        commands.push(
            Element::new(airsync::Change)
                .child(server_id(id))
                .child(flag_data(*flags)),
        );
    }
    // Emails that vanished between listing and fetching are simply skipped.
    for msg in &emails {
        snapshot.insert(msg.id.clone(), view::flags(&msg.keywords));
        let data = email::application_data(msg, body, jmap).await?;
        commands.push(
            Element::new(airsync::Add)
                .child(server_id(&msg.id))
                .child(data),
        );
    }
    Ok(emails.len())
}

pub(super) fn flag_data(flags: u64) -> Element {
    let mut data = Element::new(airsync::ApplicationData)
        .child(Element::with_text(
            email_cp::Read,
            if flags & READ != 0 { "1" } else { "0" },
        ))
        .child(email::flag_element(flags & FLAGGED != 0));
    if let Some(verb) = email::last_verb(flags) {
        data.push(verb);
    }
    data
}

/// The device's own changes inside a Sync request.
pub(super) struct ClientCommands<'a> {
    pub(super) req: &'a Request<'a>,
    pub(super) jmap: &'a JmapClient,
    pub(super) folder: &'a Folder,
    pub(super) body: &'a BodyPreference,
    pub(super) deletes_as_moves: bool,
}

/// A device change that was sent to JMAP and awaits its result.
pub(super) struct Pending {
    tag: Tag,
    server_id: String,
    email_id: String,
    /// New flags for a Change; `None` for a Delete.
    flags: Option<u64>,
}

impl ClientCommands<'_> {
    /// Applies the commands to JMAP and the snapshot; returns the elements
    /// for `<Responses>` (only failures, fetches and refused adds).
    pub(super) async fn apply(
        &self,
        commands: &Element,
        snapshot: &mut Snapshot,
    ) -> Result<Vec<Element>, CommandError> {
        let mut responses = Vec::new();
        let mut update = Map::new();
        let mut destroy = Vec::new();
        let mut pending = Vec::new();
        let mut fetches = Vec::new();
        let mut trash: Option<Option<String>> = None;

        for command in commands.elements() {
            if command.tag == airsync::Add {
                let client_id = command.child_text(airsync::ClientId).unwrap_or_default();
                // EAS 16 saves drafts this way; older devices cannot add mail.
                let response = match command.find(airsync::ApplicationData) {
                    Some(data) if self.req.version >= crate::eas::v16::VERSION => {
                        self.add_draft(client_id, data, snapshot).await?
                    }
                    _ => device_changes::add_status(client_id, None, status::CONVERSION_ERROR),
                };
                responses.push(response);
                continue;
            }
            let server_id = command.child_text(airsync::ServerId).unwrap_or_default();
            let Some(email_id) = self.folder.email_for(self.req, server_id)? else {
                responses.push(item_status(
                    command.tag,
                    server_id,
                    status::OBJECT_NOT_FOUND,
                ));
                continue;
            };
            match command.tag {
                airsync::Change => {
                    let Some(data) = command.find(airsync::ApplicationData) else {
                        continue;
                    };
                    if self.req.version >= crate::eas::v16::VERSION && drafts::is_draft_edit(data) {
                        if self.folder.account.is_some() {
                            responses.push(item_status(
                                command.tag,
                                server_id,
                                status::SERVER_ERROR,
                            ));
                            continue;
                        }
                        match self.edit_draft(&email_id, data, snapshot).await {
                            Ok(None) => {}
                            Ok(Some(attachments)) => responses.push(
                                item_status(command.tag, server_id, status::OK).child(
                                    Element::new(airsync::ApplicationData).child(attachments),
                                ),
                            ),
                            Err(e) if jmap::is_unauthorized(&e) => return Err(e.into()),
                            Err(e) => {
                                tracing::warn!(error = format!("{e:#}"), "draft change failed");
                                responses.push(item_status(
                                    command.tag,
                                    server_id,
                                    status::SERVER_ERROR,
                                ));
                            }
                        }
                        continue;
                    }
                    let mut flags = snapshot.get(&email_id).copied().unwrap_or(0);
                    let mut patch = Map::new();
                    if let Some(read) = data.child_text(email_cp::Read) {
                        let read = read == "1";
                        patch.insert(format!("keywords/{}", email::SEEN), keyword(read));
                        flags = if read { flags | READ } else { flags & !READ };
                    }
                    if let Some(flag) = data.find(email_cp::Flag) {
                        let flagged = flag.child_text(email_cp::Status) == Some("2");
                        patch.insert(format!("keywords/{}", email::FLAGGED), keyword(flagged));
                        flags = if flagged {
                            flags | FLAGGED
                        } else {
                            flags & !FLAGGED
                        };
                    }
                    if patch.is_empty() {
                        continue;
                    }
                    update.insert(email_id.clone(), Value::Object(patch));
                    pending.push(Pending {
                        tag: command.tag,
                        server_id: server_id.to_owned(),
                        email_id,
                        flags: Some(flags),
                    });
                }
                airsync::Delete => {
                    if trash.is_none() {
                        trash = Some(self.trash_mailbox().await?);
                    }
                    match trash.as_ref().and_then(Option::as_deref) {
                        Some(trash) if self.deletes_as_moves && trash != self.folder.jmap_id => {
                            update
                                .insert(email_id.clone(), move_patch(&self.folder.jmap_id, trash));
                        }
                        _ => destroy.push(email_id.clone()),
                    }
                    pending.push(Pending {
                        tag: command.tag,
                        server_id: server_id.to_owned(),
                        email_id,
                        flags: None,
                    });
                }
                airsync::Fetch => fetches.push((server_id.to_owned(), email_id)),
                _ => {}
            }
        }

        let result: SetResult = self
            .jmap
            .set(
                crate::jmap::EMAIL,
                crate::jmap::SetRequest::default()
                    .update(update)
                    .destroy(destroy),
            )
            .await?;
        for p in pending {
            if let Some(error) = result.error(&p.email_id) {
                tracing::warn!(email = p.email_id, error, "device change rejected");
                let status = if error == "notFound" {
                    status::OBJECT_NOT_FOUND
                } else {
                    status::SERVER_ERROR
                };
                responses.push(item_status(p.tag, &p.server_id, status));
                continue;
            }
            match p.flags {
                Some(flags) if snapshot.contains_key(&p.email_id) => {
                    snapshot.insert(p.email_id, flags);
                }
                Some(_) => {}
                None => {
                    snapshot.remove(&p.email_id);
                }
            }
        }

        if !fetches.is_empty() {
            responses.extend(self.fetch(&fetches).await?);
        }
        Ok(responses)
    }

    /// A draft the device created; the Add's `<Responses>` entry.
    async fn add_draft(
        &self,
        client_id: &str,
        data: &Element,
        snapshot: &mut Snapshot,
    ) -> Result<Element, CommandError> {
        if let Some((_, response)) = device_changes::repeated_add(self.req, self.folder, client_id)?
        {
            return Ok(response);
        }
        // Drafts are sent from the user's own account; not kept in shared ones.
        if self.folder.account.is_some() {
            return Ok(device_changes::add_status(
                client_id,
                None,
                status::SERVER_ERROR,
            ));
        }
        let saved = match drafts::create(self.req, self.jmap, self.folder, data).await {
            Ok(saved) => saved,
            Err(e) if jmap::is_unauthorized(&e) => return Err(e.into()),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "draft not saved");
                return Ok(device_changes::add_status(
                    client_id,
                    None,
                    status::SERVER_ERROR,
                ));
            }
        };
        let id = saved.id.clone();
        device_changes::record_add(self.req, self.folder, client_id, &id)?;
        let item = self
            .req
            .store
            .item_ids(self.req.user, self.folder.id, &[&id])?[&id];
        snapshot.insert(id.clone(), READ);
        if drafts::wants_send(data)
            && let Err(e) = drafts::send(self.req, self.jmap, &id).await
        {
            tracing::warn!(error = format!("{e:#}"), "draft not sent");
        }
        let mut response =
            device_changes::add_status(client_id, Some(&self.folder.server_id(item)), status::OK);
        if let Some(attachments) = drafts::attachment_references(&saved) {
            response.push(Element::new(airsync::ApplicationData).child(attachments));
        }
        Ok(response)
    }

    /// The device edited (or sent) one of its drafts; returns the new
    /// attachments' FileReferences when it added any.
    async fn edit_draft(
        &self,
        email_id: &str,
        data: &Element,
        snapshot: &mut Snapshot,
    ) -> anyhow::Result<Option<Element>> {
        let only_send = drafts::wants_send(data)
            && data
                .elements()
                .all(|e| e.tag == crate::wbxml::cp::email2::Send);
        let (id, attachments) = if only_send {
            (email_id.to_owned(), None)
        } else {
            let saved = drafts::replace(self.req, self.jmap, self.folder, email_id, data).await?;
            if let Some(flags) = snapshot.remove(email_id) {
                snapshot.insert(saved.id.clone(), flags);
            }
            let attachments = drafts::attachment_references(&saved);
            (saved.id, attachments)
        };
        if drafts::wants_send(data) {
            drafts::send(self.req, self.jmap, &id).await?;
        }
        Ok(attachments)
    }

    async fn trash_mailbox(&self) -> anyhow::Result<Option<String>> {
        Ok(self
            .jmap
            .mailboxes()
            .await?
            .into_iter()
            .find(|m| m.role.as_deref() == Some("trash"))
            .map(|m| m.id))
    }

    /// `<Fetch>` responses with the full items, for (server id, email id) pairs.
    async fn fetch(&self, items: &[(String, String)]) -> anyhow::Result<Vec<Element>> {
        let ids: Vec<&str> = items.iter().map(|(_, id)| id.as_str()).collect();
        let emails = self
            .jmap
            .get_emails(
                &ids,
                self.body.body_values(),
                self.body.truncation_size.unwrap_or(0),
            )
            .await?;
        let mut out = Vec::new();
        for (server_id, email_id) in items {
            let Some(msg) = emails.iter().find(|e| &e.id == email_id) else {
                out.push(item_status(
                    airsync::Fetch,
                    server_id,
                    status::OBJECT_NOT_FOUND,
                ));
                continue;
            };
            out.push(
                item_status(airsync::Fetch, server_id, status::OK)
                    .child(email::application_data(msg, self.body, self.jmap).await?),
            );
        }
        Ok(out)
    }
}

pub(super) fn keyword(set: bool) -> Value {
    if set { Value::Bool(true) } else { Value::Null }
}

/// An `Email/set` patch moving an email from one mailbox to another.
pub(crate) fn move_patch(from: &str, to: &str) -> Value {
    json!({ format!("mailboxIds/{from}"): null, format!("mailboxIds/{to}"): true })
}

/// Keeps the device's snapshots in step with a move it made itself
/// (MoveItems), so the next Sync neither deletes nor re-adds the email.
/// Records a move in both collections' snapshots; `new_id` is the email's
/// id in the destination (different after a move between accounts).
pub(crate) fn record_move(
    req: &Request,
    src_collection: &str,
    dst_collection: &str,
    email_id: &str,
    new_id: &str,
) -> anyhow::Result<()> {
    // The previous snapshots too: a retried Sync with the previous key must
    // not delete or re-add the email either.
    let mut flags = None;
    req.store.update_collection_syncs(
        req.user,
        req.device_id,
        &[src_collection, dst_collection],
        |collection, state| {
            let snapshots =
                std::iter::once(&mut state.snapshot).chain(state.previous.as_mut().map(|(_, s)| s));
            for json in snapshots {
                let mut snapshot: Snapshot = serde_json::from_str(json)?;
                if collection == src_collection {
                    if let Some(f) = snapshot.remove(email_id) {
                        flags.get_or_insert(f);
                    }
                } else {
                    snapshot.insert(new_id.to_owned(), flags.unwrap_or(0));
                }
                *json = to_json(&snapshot)?;
            }
            Ok(())
        },
    )
}
