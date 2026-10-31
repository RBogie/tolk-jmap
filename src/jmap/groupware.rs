//! Contacts and calendars: address books, cards, calendars and events.

use serde::Deserialize;
use serde_json::{Value, json};

use super::*;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressBook {
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub is_default: bool,
    /// What the user may do; `mayWrite` for cards.
    #[serde(default, deserialize_with = "null_as_default")]
    pub my_rights: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calendar {
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub is_default: bool,
    /// What the user may do; `mayWriteAll` for events.
    #[serde(default, deserialize_with = "null_as_default")]
    pub my_rights: Map<String, Value>,
}

impl AddressBook {
    /// Whether cards can be changed (rights the server does not report
    /// count as given).
    pub fn writable(&self) -> bool {
        self.my_rights.get("mayWrite") != Some(&Value::Bool(false))
    }
}

impl Calendar {
    /// Whether events can be changed (see [`AddressBook::writable`]).
    pub fn writable(&self) -> bool {
        self.my_rights.get("mayWriteAll") != Some(&Value::Bool(false))
    }
}

impl JmapClient {
    pub fn has_contacts(&self) -> bool {
        self.session.primary_accounts.contains_key(CONTACTS)
    }

    pub async fn address_books(&self) -> anyhow::Result<Vec<AddressBook>> {
        self.get_all(ADDRESS_BOOK, &["id", "name", "isDefault"])
            .await
    }

    pub fn has_calendars(&self) -> bool {
        self.session.primary_accounts.contains_key(CALENDARS)
    }

    pub async fn calendars(&self) -> anyhow::Result<Vec<Calendar>> {
        self.get_all(CALENDAR, &["id", "name", "isDefault"]).await
    }

    /// Other accounts with address books (shared with the user), with
    /// their names.
    pub fn shared_contact_accounts(&self) -> Vec<(&str, &str)> {
        self.shared_accounts(CONTACTS)
    }

    /// Other accounts with calendars, with their names.
    pub fn shared_calendar_accounts(&self) -> Vec<(&str, &str)> {
        self.shared_accounts(CALENDARS)
    }

    /// Whether cards in this address book may be changed.
    pub async fn address_book_writable(&self, id: &str) -> anyhow::Result<bool> {
        if self.is_read_only(CONTACTS) {
            return Ok(false);
        }
        let books: Vec<AddressBook> = self.get_all(ADDRESS_BOOK, &["id", "myRights"]).await?;
        Ok(books
            .iter()
            .find(|b| b.id == id)
            .is_none_or(AddressBook::writable))
    }

    /// Whether events in this calendar may be changed.
    pub async fn calendar_writable(&self, id: &str) -> anyhow::Result<bool> {
        if self.is_read_only(CALENDARS) {
            return Ok(false);
        }
        let calendars: Vec<Calendar> = self.get_all(CALENDAR, &["id", "myRights"]).await?;
        Ok(calendars
            .iter()
            .find(|c| c.id == id)
            .is_none_or(Calendar::writable))
    }

    /// Ids of a calendar's events that end (or recur) after `after`, if given.
    pub async fn calendar_event_ids(
        &self,
        calendar_id: &str,
        after: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let mut filter = json!({ "inCalendar": calendar_id });
        if let Some(after) = after {
            filter["after"] = after.into();
        }
        self.query_all(
            &[CORE, CALENDARS],
            self.account_for(CALENDARS)?,
            "CalendarEvent/query",
            filter,
            None,
        )
        .await
    }

    /// Ids of events with this iCalendar UID, in any calendar.
    pub async fn calendar_event_ids_by_uid(&self, uid: &str) -> anyhow::Result<Vec<String>> {
        let mut args = self
            .call_with(
                CALENDARS,
                "CalendarEvent/query",
                json!({ "accountId": self.account_for(CALENDARS)?, "filter": { "uid": uid } }),
            )
            .await?;
        Ok(serde_json::from_value(args["ids"].take())?)
    }

    /// The events in an iCalendar blob (such as an invitation), parsed by
    /// the server; read only.
    pub async fn parse_calendar_blob(&self, blob_id: &str) -> anyhow::Result<Vec<Value>> {
        let mut args = self
            .call_using(
                &[CORE, CALENDARS, CALENDARS_PARSE],
                "CalendarEvent/parse",
                json!({ "accountId": self.account_for(CALENDARS)?, "blobIds": [blob_id] }),
            )
            .await?;
        Ok(match args["parsed"][blob_id].take() {
            Value::Array(events) => events,
            event @ Value::Object(_) => vec![event],
            _ => Vec::new(),
        })
    }

    /// Ids of all cards in an address book.
    pub async fn contact_ids(&self, address_book_id: &str) -> anyhow::Result<Vec<String>> {
        let filter = json!({ "inAddressBook": address_book_id });
        self.query_all(
            &[CORE, CONTACTS],
            self.account_for(CONTACTS)?,
            "ContactCard/query",
            filter,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_the_server_does_not_report_count_as_given() {
        let book = |rights: Value| -> AddressBook {
            serde_json::from_value(json!({ "id": "b", "myRights": rights })).unwrap()
        };
        assert!(book(Value::Null).writable());
        assert!(book(json!({ "mayWrite": true })).writable());
        assert!(!book(json!({ "mayWrite": false })).writable());
        let calendar: Calendar =
            serde_json::from_value(json!({ "id": "c", "myRights": { "mayWriteAll": false } }))
                .unwrap();
        assert!(!calendar.writable());
    }
}
