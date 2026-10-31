//! Contacts collections: JMAP address books as EAS contact folders.
//!
//! Like mail, each collection keeps a view of the server side (card ids
//! with a version) caught up from `ContactCard/changes`. A card's version is
//! a hash of what the device would receive, so only changes the device can
//! see are sent.

use futures_util::StreamExt;
use serde_json::{Map, Value};

use super::{
    device_changes::{Kind, Patch},
    jscontact,
    view::{Item, Source},
};
use crate::{
    jmap::{Changes, JmapClient, SetResult},
    wbxml::{self, Element, cp::airsync},
};

/// Folder ids of address books carry this prefix in the store, which keeps
/// them apart from mailbox ids.
pub const PREFIX: &str = "contacts:";

/// The version of a card: a hash of its EAS encoding (FNV-1a, so it stays
/// the same across builds and restarts).
/// A photo kept as a blob counts by its blob id, so versions need no download.
pub fn version(card: &Value, max_picture: usize) -> u64 {
    let mut bytes = wbxml::encode(&jscontact::to_eas(card, max_picture));
    if let Some((_, blob)) = jscontact::photo_blob(card) {
        bytes.extend_from_slice(blob.as_bytes());
    }
    fnv1a(&bytes)
}

/// Puts photos kept as blobs into the cards (see [`jscontact::photo_blob`]);
/// a photo that cannot be downloaded is left out.
async fn with_photos(jmap: &JmapClient, mut cards: Vec<Value>) -> Vec<Value> {
    let wanted: Vec<(usize, String, String)> = cards
        .iter()
        .enumerate()
        .filter_map(|(i, card)| {
            let (key, blob) = jscontact::photo_blob(card)?;
            Some((i, key.to_owned(), blob.to_owned()))
        })
        .collect();
    let downloads = wanted.into_iter().map(|(i, key, blob)| async move {
        let photo = jmap
            .download(&blob, "photo", "application/octet-stream")
            .await;
        (i, key, blob, photo)
    });
    let photos: Vec<_> = futures_util::stream::iter(downloads)
        .buffer_unordered(PHOTO_DOWNLOADS)
        .collect()
        .await;
    for (i, key, blob, photo) in photos {
        match photo {
            Ok(bytes) => jscontact::inline_photo(&mut cards[i], &key, &bytes),
            Err(e) => tracing::warn!(blob, "contact photo download failed: {e:#}"),
        }
    }
    cards
}

/// Photo downloads at once per request.
const PHOTO_DOWNLOADS: usize = 8;

pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// An address book's cards, versioned by a hash of what the device sees.
pub struct ContactsSource<'a> {
    pub jmap: &'a JmapClient,
    pub address_book: &'a str,
    /// See [`jscontact::to_eas`].
    pub max_picture: usize,
}

impl Source for ContactsSource<'_> {
    fn key(&self) -> String {
        format!("contacts:{}", self.address_book)
    }

    async fn state(&self) -> anyhow::Result<String> {
        self.jmap.state(crate::jmap::CONTACT_CARD).await
    }

    async fn changes(&self, since: &str, max: usize) -> anyhow::Result<Changes> {
        self.jmap
            .changes(crate::jmap::CONTACT_CARD, since, max)
            .await
    }

    async fn list(&self) -> anyhow::Result<Vec<Item>> {
        let ids = self.jmap.contact_ids(self.address_book).await?;
        self.refresh(&ids.iter().map(String::as_str).collect::<Vec<_>>())
            .await
    }

    async fn refresh(&self, ids: &[&str]) -> anyhow::Result<Vec<Item>> {
        Ok(self
            .jmap
            .get_objects(crate::jmap::CONTACT_CARD, ids)
            .await?
            .iter()
            .filter(|card| card["addressBookIds"][self.address_book] == true)
            .filter_map(|card| {
                Some(Item {
                    id: card["id"].as_str()?.to_owned(),
                    version: version(card, self.max_picture),
                    order: None,
                })
            })
            .collect())
    }
}

/// Cards with their version and `ApplicationData`; missing ids are skipped.
pub async fn fetch(
    jmap: &JmapClient,
    ids: &[&str],
    max_picture: usize,
) -> anyhow::Result<Vec<(String, u64, Element)>> {
    let cards = jmap.get_objects(crate::jmap::CONTACT_CARD, ids).await?;
    // Versions come from the cards as stored, before photos are put in.
    let versions: Vec<u64> = cards.iter().map(|c| version(c, max_picture)).collect();
    Ok(with_photos(jmap, cards)
        .await
        .iter()
        .zip(versions)
        .filter_map(|(card, version)| {
            Some((
                card["id"].as_str()?.to_owned(),
                version,
                jscontact::to_eas(card, max_picture),
            ))
        })
        .collect())
}

/// How the device's changes to cards become JMAP (see [`device_changes`]).
pub(super) struct ContactChanges<'a> {
    jmap: &'a JmapClient,
    address_book: &'a str,
    /// The fields the device manages (`<Supported>`), if it said.
    supported: Option<&'a [String]>,
    /// The cards being changed: a Change only writes what differs.
    current: Vec<Value>,
    max_picture: usize,
}

impl<'a> ContactChanges<'a> {
    pub fn new(
        jmap: &'a JmapClient,
        address_book: &'a str,
        supported: Option<&'a [String]>,
        max_picture: usize,
    ) -> Self {
        Self {
            jmap,
            address_book,
            supported,
            current: Vec::new(),
            max_picture,
        }
    }
}

impl Kind for ContactChanges<'_> {
    async fn prepare(&mut self, ids: &[&str]) -> anyhow::Result<()> {
        if !ids.is_empty() {
            // As the device sees them, so an unchanged photo stays as it is.
            let cards = self
                .jmap
                .get_objects(crate::jmap::CONTACT_CARD, ids)
                .await?;
            self.current = with_photos(self.jmap, cards).await;
        }
        Ok(())
    }

    fn create(&mut self, data: &Element, _client_id: &str) -> Map<String, Value> {
        let mut card: Map<String, Value> = jscontact::from_eas(data)
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .collect();
        card.insert("@type".to_owned(), "Card".into());
        card.insert("version".to_owned(), "1.0".into());
        card.insert(
            "addressBookIds".to_owned(),
            serde_json::json!({ self.address_book: true }),
        );
        card
    }

    fn patch(&mut self, id: &str, command: &Element) -> Patch {
        let Some(data) = command.find(airsync::ApplicationData) else {
            return Patch::Skip;
        };
        let Some(card) = self.current.iter().find(|c| c["id"] == id) else {
            return Patch::NotFound;
        };
        let patch = jscontact::merge(card, data, self.supported, self.max_picture);
        if patch.is_empty() {
            Patch::Skip
        } else {
            Patch::Apply(patch)
        }
    }

    async fn writable(&self) -> anyhow::Result<bool> {
        if !self.jmap.is_shared() {
            return Ok(true);
        }
        self.jmap.address_book_writable(self.address_book).await
    }

    async fn set(
        &self,
        create: Map<String, Value>,
        update: Map<String, Value>,
        destroy: Vec<String>,
    ) -> anyhow::Result<SetResult> {
        self.jmap
            .set(
                crate::jmap::CONTACT_CARD,
                crate::jmap::SetRequest::default()
                    .create(create)
                    .update(update)
                    .destroy(destroy),
            )
            .await
    }

    async fn fetch(&self, ids: &[&str]) -> anyhow::Result<Vec<(String, u64, Element)>> {
        fetch(self.jmap, ids, self.max_picture).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn versions_follow_what_the_device_sees() {
        let card = json!({ "id": "a", "name": { "full": "Ann" }, "uid": "x" });
        let same_for_device = json!({ "id": "a", "name": { "full": "Ann" }, "uid": "y", "updated": "2026-10-30T00:00:00Z" });
        let renamed = json!({ "id": "a", "name": { "full": "Anne" } });
        let version = |card| version(card, usize::MAX);
        assert_eq!(version(&card), version(&same_for_device));
        assert_ne!(version(&card), version(&renamed));
    }

    #[test]
    fn photo_blobs_count_towards_the_version() {
        let photo =
            |blob| json!({ "id": "a", "media": { "p": { "kind": "photo", "blobId": blob } } });
        let version = |card| version(card, usize::MAX);
        assert_ne!(version(&photo("b1")), version(&photo("b2")));
        let mut inlined = photo("b1");
        jscontact::inline_photo(&mut inlined, "p", b"JPEG");
        assert_eq!(jscontact::photo_blob(&inlined), None);
        assert_eq!(
            inlined["media"]["p"]["uri"],
            "data:image/jpeg;base64,SlBFRw=="
        );
    }

    #[test]
    fn hash_matches_fnv1a_test_vectors() {
        // Stored versions must stay valid across builds.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
