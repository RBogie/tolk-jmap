//! Short EAS ids for folders and items, and the ClientIds that make
//! retried SendMail and Sync Adds idempotent.

use super::*;

impl Store {
    /// Returns the EAS folder id of each mailbox, assigning new ids as needed.
    pub fn folder_server_ids(
        &self,
        user: &str,
        mailbox_ids: &[&str],
    ) -> anyhow::Result<HashMap<String, u32>> {
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        let mut ids = HashMap::new();
        {
            let mut next: u32 = tx.query_row(
                "SELECT COALESCE(MAX(server_id), 0) + 1 FROM folder_ids WHERE user = ?1",
                [user],
                |row| row.get(0),
            )?;
            let mut select =
                tx.prepare("SELECT server_id FROM folder_ids WHERE user = ?1 AND mailbox_id = ?2")?;
            for &mailbox_id in mailbox_ids {
                let existing: Option<u32> = select
                    .query_row(params![user, mailbox_id], |row| row.get(0))
                    .optional()?;
                let server_id = match existing {
                    Some(id) => id,
                    None => {
                        tx.execute(
                            "INSERT INTO folder_ids (user, server_id, mailbox_id) VALUES (?1, ?2, ?3)",
                            params![user, next, mailbox_id],
                        )?;
                        next += 1;
                        next - 1
                    }
                };
                ids.insert(mailbox_id.to_owned(), server_id);
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    /// The mailbox behind an EAS folder id.
    pub fn mailbox_for_folder(&self, user: &str, folder_id: u32) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT mailbox_id FROM folder_ids WHERE user = ?1 AND server_id = ?2",
                params![user, folder_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Returns the EAS item id of each email in a folder, assigning new ids as needed.
    pub fn item_ids(
        &self,
        user: &str,
        folder_id: u32,
        email_ids: &[&str],
    ) -> anyhow::Result<HashMap<String, u32>> {
        let mut conn = self.conn.locked();
        let tx = conn.transaction()?;
        let mut ids = HashMap::new();
        {
            let mut next: u32 = tx.query_row(
                "SELECT COALESCE(MAX(item_id), 0) + 1 FROM item_ids
                 WHERE user = ?1 AND folder_id = ?2",
                params![user, folder_id],
                |row| row.get(0),
            )?;
            let mut select = tx.prepare(
                "SELECT item_id FROM item_ids WHERE user = ?1 AND folder_id = ?2 AND email_id = ?3",
            )?;
            for &email_id in email_ids {
                let existing: Option<u32> = select
                    .query_row(params![user, folder_id, email_id], |row| row.get(0))
                    .optional()?;
                let item_id = match existing {
                    Some(id) => id,
                    None => {
                        tx.execute(
                            "INSERT INTO item_ids (user, folder_id, item_id, email_id, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![user, folder_id, next, email_id, unix_now()],
                        )?;
                        next += 1;
                        next - 1
                    }
                };
                ids.insert(email_id.to_owned(), item_id);
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Lets an item id stand for another email (a draft replaced by its
    /// edited version keeps its ServerId).
    pub fn repoint_item(
        &self,
        user: &str,
        folder_id: u32,
        old_email: &str,
        new_email: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "UPDATE item_ids SET email_id = ?4 WHERE user = ?1 AND folder_id = ?2 AND email_id = ?3",
            params![user, folder_id, old_email, new_email],
        )?;
        Ok(())
    }

    /// The email behind an EAS item id.
    pub fn email_for_item(
        &self,
        user: &str,
        folder_id: u32,
        item_id: u32,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT email_id FROM item_ids WHERE user = ?1 AND folder_id = ?2 AND item_id = ?3",
                params![user, folder_id, item_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records a message ClientId; false when it was already used.
    pub fn claim_client_id(
        &self,
        user: &str,
        device_id: &str,
        client_id: &str,
    ) -> anyhow::Result<bool> {
        let inserted = self.conn.locked().execute(
            "INSERT OR IGNORE INTO sent_client_ids (user, device_id, client_id, sent_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![user, device_id, client_id, unix_now()],
        )?;
        Ok(inserted == 1)
    }

    /// The object an earlier Sync Add with this ClientId created.
    pub fn client_add(
        &self,
        user: &str,
        device_id: &str,
        folder_id: u32,
        client_id: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .locked()
            .query_row(
                "SELECT object_id FROM client_adds
                 WHERE user = ?1 AND device_id = ?2 AND folder_id = ?3 AND client_id = ?4",
                params![user, device_id, folder_id, client_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn record_client_add(
        &self,
        user: &str,
        device_id: &str,
        folder_id: u32,
        client_id: &str,
        object_id: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "INSERT OR REPLACE INTO client_adds
                 (user, device_id, folder_id, client_id, object_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![user, device_id, folder_id, client_id, object_id, unix_now()],
        )?;
        Ok(())
    }

    /// Frees a ClientId after a failed send, so the device can retry.
    pub fn release_client_id(
        &self,
        user: &str,
        device_id: &str,
        client_id: &str,
    ) -> anyhow::Result<()> {
        self.conn.locked().execute(
            "DELETE FROM sent_client_ids WHERE user = ?1 AND device_id = ?2 AND client_id = ?3",
            params![user, device_id, client_id],
        )?;
        Ok(())
    }
}
