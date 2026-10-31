//! JSContact cards (RFC 9553, as JMAP `ContactCard`) to EAS Contacts
//! `ApplicationData` (MS-ASCNTC) and back.
//!
//! EAS has fixed slots (three emails, two home and two business phones, one
//! address per kind), so the phone does not see entries beyond them. A
//! device Add writes the properties listed in [`MANAGED`]; a device Change
//! only writes the fields it changed ([`merge`]), so the extras survive.

use serde_json::{Map, Value, json};

mod merge;

pub use merge::merge;

use crate::wbxml::{
    Element, Tag,
    cp::{airsync, airsyncbase, contacts as c, contacts2 as c2},
};

/// Card properties this mapping writes.
pub const MANAGED: &[&str] = &[
    "name",
    "nicknames",
    "organizations",
    "titles",
    "emails",
    "phones",
    "addresses",
    "anniversaries",
    "links",
    "notes",
    "keywords",
    "media",
    "onlineServices",
    "relatedTo",
];

/// EAS address fields per kind: street, city, state, postal code, country.
const ADDRESS_FIELDS: [(&str, [Tag; 5]); 3] = [
    (
        "work",
        [
            c::BusinessAddressStreet,
            c::BusinessAddressCity,
            c::BusinessAddressState,
            c::BusinessAddressPostalCode,
            c::BusinessAddressCountry,
        ],
    ),
    (
        "private",
        [
            c::HomeAddressStreet,
            c::HomeAddressCity,
            c::HomeAddressState,
            c::HomeAddressPostalCode,
            c::HomeAddressCountry,
        ],
    ),
    (
        "other",
        [
            c::OtherAddressStreet,
            c::OtherAddressCity,
            c::OtherAddressState,
            c::OtherAddressPostalCode,
            c::OtherAddressCountry,
        ],
    ),
];

const PHONE_SLOTS: [Tag; 8] = [
    c::MobilePhoneNumber,
    c::HomePhoneNumber,
    c::Home2PhoneNumber,
    c::BusinessPhoneNumber,
    c::Business2PhoneNumber,
    c::BusinessFaxNumber,
    c::HomeFaxNumber,
    c::PagerNumber,
];

/// JSContact address component kinds matching the EAS fields above (the
/// street is assembled from `number` and `name`).
const ADDRESS_KINDS: [&str; 4] = ["locality", "region", "postcode", "country"];

/// EAS name fields and the JSContact name component kinds they show.
const NAME_PARTS: [(Tag, &str); 5] = [
    (c::Title, "title"),
    (c::FirstName, "given"),
    (c::MiddleName, "given2"),
    (c::LastName, "surname"),
    (c::Suffix, "generation"),
];

/// The phone a device sends in an EAS slot: its features and context.
fn phone_entry(tag: Tag, number: &str) -> Value {
    let (feature, context) = match tag {
        t if t == c::MobilePhoneNumber => ("mobile", None),
        t if t == c::HomePhoneNumber || t == c::Home2PhoneNumber => ("voice", Some("private")),
        t if t == c::BusinessPhoneNumber || t == c::Business2PhoneNumber => ("voice", Some("work")),
        t if t == c::BusinessFaxNumber => ("fax", Some("work")),
        t if t == c::HomeFaxNumber => ("fax", Some("private")),
        _ => ("pager", None),
    };
    let mut phone = json!({ "number": number, "features": { feature: true } });
    if let Some(context) = context {
        phone["contexts"] = json!({ context: true });
    }
    phone
}

/// The email a device sends in slot `index` (0 to 2).
fn email_entry(index: usize, address: &str) -> Value {
    json!({ "address": bare_address(address), "pref": index + 1 })
}

/// An address of `kind` from the five EAS fields; `None` when all are empty.
fn address_entry(kind: &str, fields: [Option<&str>; 5]) -> Option<Value> {
    let [street, rest @ ..] = fields;
    let mut components = Vec::new();
    if let Some(street) = street {
        components.push(json!({ "kind": "name", "value": street }));
    }
    for (value, component) in rest.into_iter().zip(ADDRESS_KINDS) {
        if let Some(value) = value {
            components.push(json!({ "kind": component, "value": value }));
        }
    }
    if components.is_empty() {
        return None;
    }
    let mut address = json!({ "components": components });
    if kind != "other" {
        address["contexts"] = json!({ kind: true });
    }
    Some(address)
}

/// The keys of the emails in the three EAS slots, most preferred first.
fn email_slots(card: &Value) -> Vec<&str> {
    let mut emails = keyed(&card["emails"]);
    emails.sort_by_key(|(_, e)| e["pref"].as_u64().unwrap_or(u64::MAX));
    emails
        .into_iter()
        .take(3)
        .map(|(k, _)| k.as_str())
        .collect()
}

/// The EAS phone slots and the keys of the phones in them, in slot order.
fn phone_slots(card: &Value) -> Vec<(Tag, &str)> {
    let mut slots: Vec<(Tag, &str)> = Vec::new();
    for (key, phone) in keyed(&card["phones"]) {
        if !phone["number"].is_string() {
            continue;
        }
        let feature = |f: &str| phone["features"][f] == true;
        let work = phone["contexts"]["work"] == true;
        let candidates: &[Tag] = if feature("mobile") {
            &[c::MobilePhoneNumber]
        } else if feature("pager") {
            &[c::PagerNumber]
        } else if feature("fax") {
            if work {
                &[c::BusinessFaxNumber]
            } else {
                &[c::HomeFaxNumber]
            }
        } else if work {
            &[c::BusinessPhoneNumber, c::Business2PhoneNumber]
        } else {
            &[c::HomePhoneNumber, c::Home2PhoneNumber]
        };
        if let Some(tag) = candidates
            .iter()
            .find(|t| !slots.iter().any(|(s, _)| s == *t))
        {
            slots.push((*tag, key));
        }
    }
    // A fixed order keeps the encoding (and so the change hash) stable.
    slots.sort_by_key(|(tag, _)| PHONE_SLOTS.iter().position(|t| t == tag));
    slots
}

/// The EAS address kinds and the keys of the addresses shown in them.
fn address_slots(card: &Value) -> Vec<(&'static str, &str)> {
    let mut slots: Vec<(&'static str, &str)> = Vec::new();
    for (key, address) in keyed(&card["addresses"]) {
        let kind = if address["contexts"]["work"] == true {
            "work"
        } else if address["contexts"]["private"] == true {
            "private"
        } else {
            "other"
        };
        let used = |kind| slots.iter().any(|(k, _)| *k == kind);
        let kind = if used(kind) { "other" } else { kind };
        if used(kind) {
            continue;
        }
        slots.push((kind, key));
    }
    slots
}

/// The address's text for one of the five EAS fields (0 is the street).
fn address_field(address: &Value, field: usize) -> String {
    let part = |k: &str| {
        entries_array(&address["components"])
            .filter(|c| c["kind"] == k)
            .filter_map(|c| c["value"].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    if field > 0 {
        return part(ADDRESS_KINDS[field - 1]);
    }
    let street = [part("name"), part("number")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if street.is_empty() {
        address["full"].as_str().unwrap_or_default().to_owned()
    } else {
        street
    }
}

/// The card's `ApplicationData` for the device; a photo larger than
/// `max_picture` bytes is left out (`CONTACT_PICTURE_MAX_KB`).
pub fn to_eas(card: &Value, max_picture: usize) -> Element {
    let mut data = Element::new(airsync::ApplicationData);
    let mut put = |tag: Tag, value: Option<&str>| {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            data.push(Element::with_text(tag, value));
        }
    };

    // Name
    let component = |kind: &str| -> Option<String> {
        let values: Vec<&str> = entries_array(&card["name"]["components"])
            .filter(|c| c["kind"] == kind)
            .filter_map(|c| c["value"].as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(" "))
    };
    let (first, middle, last) = (
        component("given"),
        component("given2"),
        component("surname"),
    );
    put(c::Title, component("title").as_deref());
    put(c::FirstName, first.as_deref());
    put(c::MiddleName, middle.as_deref());
    put(c::LastName, last.as_deref());
    put(
        c::Suffix,
        component("generation")
            .or_else(|| component("credential"))
            .as_deref(),
    );
    let file_as = card["name"]["full"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| match (&last, &first) {
            (Some(last), Some(first)) => Some(format!("{last}, {first}")),
            (Some(name), None) | (None, Some(name)) => Some(name.clone()),
            (None, None) => None,
        });

    // Organization
    let organization = entries(&card["organizations"]).next();
    let company = organization.and_then(|o| o["name"].as_str());
    put(c::CompanyName, company);
    put(
        c::Department,
        organization
            .and_then(|o| entries_array(&o["units"]).next())
            .and_then(|u| u["name"].as_str()),
    );
    put(
        c::JobTitle,
        entries(&card["titles"])
            .find(|t| t["kind"] != "role")
            .and_then(|t| t["name"].as_str()),
    );
    put(c::FileAs, file_as.as_deref().or(company));

    // Emails, most preferred first
    for (tag, key) in [c::Email1Address, c::Email2Address, c::Email3Address]
        .into_iter()
        .zip(email_slots(card))
    {
        put(tag, card["emails"][key]["address"].as_str());
    }

    // Phones into their EAS slots
    for (tag, key) in phone_slots(card) {
        put(tag, card["phones"][key]["number"].as_str());
    }

    // One address per kind
    for (kind, key) in address_slots(card) {
        let address = &card["addresses"][key];
        let tags = ADDRESS_FIELDS.iter().find(|(k, _)| *k == kind).unwrap().1;
        for (field, tag) in tags.into_iter().enumerate() {
            put(tag, Some(&address_field(address, field)));
        }
    }

    // Dates
    for anniversary in entries(&card["anniversaries"]) {
        let tag = match anniversary["kind"].as_str() {
            Some("birth") => c::Birthday,
            Some("wedding") => c::Anniversary,
            _ => continue,
        };
        put(tag, eas_date(&anniversary["date"]).as_deref());
    }

    put(
        c::WebPage,
        entries(&card["links"]).find_map(|l| l["uri"].as_str()),
    );
    put(
        c::Spouse,
        card["relatedTo"]
            .as_object()
            .and_then(|r| r.iter().find(|(_, v)| v["relation"]["spouse"] == true))
            .map(|(name, _)| name.as_str()),
    );

    let categories: Vec<&String> = card["keywords"]
        .as_object()
        .map(|k| {
            k.iter()
                .filter(|(_, v)| **v == true)
                .map(|(name, _)| name)
                .collect()
        })
        .unwrap_or_default();
    if !categories.is_empty() {
        data.push(
            categories
                .into_iter()
                .fold(Element::new(c::Categories), |list, name| {
                    list.child(Element::with_text(c::Category, name.as_str()))
                }),
        );
    }

    let mut put2 = |tag: Tag, value: Option<&str>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            data.push(Element::with_text(tag, value));
        }
    };
    put2(
        c2::NickName,
        entries(&card["nicknames"]).find_map(|n| n["name"].as_str()),
    );
    for (tag, service) in [c2::IMAddress, c2::IMAddress2, c2::IMAddress3]
        .into_iter()
        .zip(entries(&card["onlineServices"]))
    {
        put2(tag, service["user"].as_str().or(service["uri"].as_str()));
    }

    if let Some(photo) = entries(&card["media"])
        .filter(|m| m["kind"] == "photo")
        .find_map(|m| {
            m["uri"]
                .as_str()?
                .split_once(";base64,")
                .map(|(_, data)| data)
        })
        .filter(|data| data.len() * 3 / 4 <= max_picture)
    {
        data.push(Element::with_text(c::Picture, photo));
    }

    let notes: Vec<&str> = entries(&card["notes"])
        .filter_map(|n| n["note"].as_str())
        .collect();
    if !notes.is_empty() {
        let text = notes.join("\n\n");
        data.push(
            Element::new(airsyncbase::Body)
                .child(Element::with_text(airsyncbase::Type, "1"))
                .child(Element::with_text(
                    airsyncbase::EstimatedDataSize,
                    text.len().to_string(),
                ))
                .child(Element::with_text(airsyncbase::Data, text)),
        );
    }
    data
}

/// The values of a JSContact `Id[...]` map, in id order.
/// The photo the device is shown when the server keeps it as a blob (as
/// Stalwart does): its key in `media` and its blob id. [`to_eas`] only sends
/// `data:` photos, so the blob is downloaded and put in with [`inline_photo`].
pub fn photo_blob(card: &Value) -> Option<(&str, &str)> {
    let (key, photo) = keyed(&card["media"])
        .into_iter()
        .find(|(_, m)| m["kind"] == "photo")?;
    if photo["uri"]
        .as_str()
        .is_some_and(|uri| uri.contains(";base64,"))
    {
        return None;
    }
    Some((key, photo["blobId"].as_str()?))
}

/// Replaces the photo `key` with a `data:` one holding `bytes`.
pub fn inline_photo(card: &mut Value, key: &str, bytes: &[u8]) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let photo = &mut card["media"][key];
    let media_type = photo["mediaType"]
        .as_str()
        .unwrap_or("image/jpeg")
        .to_owned();
    *photo = json!({
        "@type": "Media",
        "kind": "photo",
        "mediaType": media_type,
        "uri": format!("data:{media_type};base64,{}", STANDARD.encode(bytes)),
    });
}

fn entries(map: &Value) -> impl Iterator<Item = &Value> {
    keyed(map).into_iter().map(|(_, v)| v)
}

/// The ids and values of a JSContact `Id[...]` map, in id order.
fn keyed(map: &Value) -> Vec<(&String, &Value)> {
    let mut values: Vec<(&String, &Value)> = map
        .as_object()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    values.sort_by(|a, b| a.0.cmp(b.0));
    values
}

fn entries_array(list: &Value) -> impl Iterator<Item = &Value> {
    list.as_array().into_iter().flatten()
}

/// A JSContact date (PartialDate or Timestamp) as an EAS date.
fn eas_date(date: &Value) -> Option<String> {
    if let Some(utc) = date["utc"].as_str() {
        return Some(format!("{}T00:00:00.000Z", utc.get(..10)?));
    }
    let (year, month, day) = (
        date["year"].as_u64()?,
        date["month"].as_u64()?,
        date["day"].as_u64()?,
    );
    Some(format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z"))
}

/// The card properties of a device Add; every key of [`MANAGED`] is present,
/// `null` when the device has nothing for it.
pub fn from_eas(data: &Element) -> Map<String, Value> {
    let text = |tag: Tag| {
        data.child_text(tag)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let mut card = Map::new();
    for key in MANAGED {
        card.insert((*key).to_owned(), Value::Null);
    }

    let mut components = Vec::new();
    for (tag, kind) in NAME_PARTS {
        if let Some(value) = text(tag) {
            components.push(json!({ "kind": kind, "value": value }));
        }
    }
    if !components.is_empty() {
        card.insert("name".to_owned(), json!({ "components": components }));
    }

    if text(c::CompanyName).is_some() || text(c::Department).is_some() {
        let mut org = Map::new();
        if let Some(name) = text(c::CompanyName) {
            org.insert("name".to_owned(), name.into());
        }
        if let Some(unit) = text(c::Department) {
            org.insert("units".to_owned(), json!([{ "name": unit }]));
        }
        card.insert("organizations".to_owned(), json!({ "o1": org }));
    }
    if let Some(title) = text(c::JobTitle) {
        card.insert(
            "titles".to_owned(),
            json!({ "t1": { "name": title, "kind": "title" } }),
        );
    }

    let emails = id_map(
        "e",
        [c::Email1Address, c::Email2Address, c::Email3Address]
            .iter()
            .enumerate()
            .filter_map(|(i, tag)| Some(email_entry(i, text(*tag)?))),
    );
    insert_nonempty(&mut card, "emails", emails);

    let phones = id_map(
        "p",
        PHONE_SLOTS
            .into_iter()
            .filter_map(|tag| Some(phone_entry(tag, text(tag)?))),
    );
    insert_nonempty(&mut card, "phones", phones);

    let addresses = id_map(
        "a",
        ADDRESS_FIELDS
            .iter()
            .filter_map(|(kind, tags)| address_entry(kind, tags.map(text))),
    );
    insert_nonempty(&mut card, "addresses", addresses);

    let anniversaries = id_map(
        "d",
        [(c::Birthday, "birth"), (c::Anniversary, "wedding")]
            .into_iter()
            .filter_map(|(tag, kind)| {
                Some(json!({ "kind": kind, "date": partial_date(text(tag)?)? }))
            }),
    );
    insert_nonempty(&mut card, "anniversaries", anniversaries);

    if let Some(uri) = text(c::WebPage) {
        card.insert("links".to_owned(), json!({ "l1": { "uri": uri } }));
    }
    if let Some(spouse) = text(c::Spouse) {
        card.insert(
            "relatedTo".to_owned(),
            json!({ spouse: { "relation": { "spouse": true } } }),
        );
    }
    if let Some(categories) = data.find(c::Categories) {
        let keywords: Map<String, Value> = categories
            .find_all(c::Category)
            .filter_map(|c| c.text().filter(|t| !t.is_empty()))
            .map(|name| (name.to_owned(), Value::Bool(true)))
            .collect();
        if !keywords.is_empty() {
            card.insert("keywords".to_owned(), Value::Object(keywords));
        }
    }
    if let Some(nick) = text(c2::NickName) {
        card.insert("nicknames".to_owned(), json!({ "n1": { "name": nick } }));
    }
    let services = id_map(
        "im",
        [c2::IMAddress, c2::IMAddress2, c2::IMAddress3]
            .into_iter()
            .filter_map(|tag| Some(json!({ "user": text(tag)? }))),
    );
    insert_nonempty(&mut card, "onlineServices", services);

    if let Some(photo) = text(c::Picture) {
        card.insert(
            "media".to_owned(),
            json!({ "m1": { "kind": "photo", "mediaType": "image/jpeg", "uri": format!("data:image/jpeg;base64,{photo}") } }),
        );
    }
    if let Some(note) = data
        .find(airsyncbase::Body)
        .and_then(|b| b.child_text(airsyncbase::Data))
        .filter(|n| !n.trim().is_empty())
    {
        card.insert("notes".to_owned(), json!({ "n1": { "note": note } }));
    }
    card
}

/// Numbered ids (`p1`, `p2`, ...) for the values of a JSContact map.
fn id_map(prefix: &str, values: impl Iterator<Item = Value>) -> Map<String, Value> {
    values
        .enumerate()
        .map(|(i, v)| (format!("{prefix}{}", i + 1), v))
        .collect()
}

fn insert_nonempty(card: &mut Map<String, Value>, key: &str, map: Map<String, Value>) {
    if !map.is_empty() {
        card.insert(key.to_owned(), Value::Object(map));
    }
}

/// `"Name" <addr>` (as EAS sometimes sends emails) to `addr`.
fn bare_address(value: &str) -> &str {
    match (value.rfind('<'), value.rfind('>')) {
        (Some(start), Some(end)) if start < end => &value[start + 1..end],
        _ => value,
    }
}

/// An EAS date as a JSContact PartialDate. A time at or after noon UTC is
/// a phone east of UTC sending local midnight, so it counts as the next day.
fn partial_date(eas: &str) -> Option<Value> {
    let year: i64 = eas.get(..4)?.parse().ok()?;
    let month: u32 = eas.get(5..7)?.parse().ok()?;
    let day: u32 = eas.get(8..10)?.parse().ok()?;
    let hour: u32 = eas.get(11..13).and_then(|h| h.parse().ok()).unwrap_or(0);
    let (year, month, day) = if hour >= 12 {
        next_day(year, month, day)
    } else {
        (year, month, day)
    };
    Some(json!({ "@type": "PartialDate", "year": year, "month": month, "day": day }))
}

fn next_day(year: i64, month: u32, day: u32) -> (i64, u32, u32) {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    match (day < days_in_month, month < 12) {
        (true, _) => (year, month, day + 1),
        (false, true) => (year, month + 1, 1),
        (false, false) => (year + 1, 1, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> Value {
        json!({
            "name": { "components": [
                { "kind": "given", "value": "Ann" },
                { "kind": "surname", "value": "Lee" },
            ]},
            "organizations": { "o": { "name": "Acme", "units": [{ "name": "R&D" }] } },
            "titles": { "t": { "name": "Engineer" } },
            "emails": {
                "b": { "address": "ann@work.example", "pref": 2 },
                "a": { "address": "ann@example.com", "pref": 1 },
            },
            "phones": {
                "1": { "number": "+31 6 1", "features": { "mobile": true } },
                "2": { "number": "+31 20 2", "contexts": { "work": true }, "features": { "voice": true } },
                "3": { "number": "+31 30 3" },
            },
            "addresses": { "h": {
                "contexts": { "private": true },
                "components": [
                    { "kind": "name", "value": "Main Street" },
                    { "kind": "number", "value": "1" },
                    { "kind": "locality", "value": "Utrecht" },
                    { "kind": "postcode", "value": "3511 AA" },
                    { "kind": "country", "value": "NL" },
                ],
            }},
            "anniversaries": { "b": { "kind": "birth", "date": { "@type": "PartialDate", "year": 1990, "month": 5, "day": 1 } } },
            "notes": { "n": { "note": "Met at FOSDEM" } },
            "keywords": { "friends": true },
            "nicknames": { "n": { "name": "Annie" } },
            "media": { "p": { "kind": "photo", "uri": "data:image/png;base64,iVBOR" } },
        })
    }

    #[test]
    fn maps_a_card_to_eas() {
        let data = to_eas(&card(), usize::MAX);
        let text = |tag| data.child_text(tag);
        assert_eq!(text(c::FirstName), Some("Ann"));
        assert_eq!(text(c::LastName), Some("Lee"));
        assert_eq!(text(c::FileAs), Some("Lee, Ann"));
        assert_eq!(text(c::CompanyName), Some("Acme"));
        assert_eq!(text(c::Department), Some("R&D"));
        assert_eq!(text(c::JobTitle), Some("Engineer"));
        assert_eq!(text(c::Email1Address), Some("ann@example.com"));
        assert_eq!(text(c::Email2Address), Some("ann@work.example"));
        assert_eq!(text(c::MobilePhoneNumber), Some("+31 6 1"));
        assert_eq!(text(c::BusinessPhoneNumber), Some("+31 20 2"));
        assert_eq!(text(c::HomePhoneNumber), Some("+31 30 3"));
        assert_eq!(text(c::HomeAddressStreet), Some("Main Street 1"));
        assert_eq!(text(c::HomeAddressCity), Some("Utrecht"));
        assert_eq!(text(c::HomeAddressPostalCode), Some("3511 AA"));
        assert_eq!(text(c::Birthday), Some("1990-05-01T00:00:00.000Z"));
        assert_eq!(text(c2::NickName), Some("Annie"));
        assert_eq!(text(c::Picture), Some("iVBOR"));
        assert_eq!(
            data.find(c::Categories)
                .and_then(|c| c.child_text(c::Category)),
            Some("friends")
        );
        assert_eq!(
            data.find(airsyncbase::Body)
                .and_then(|b| b.child_text(airsyncbase::Data)),
            Some("Met at FOSDEM")
        );
    }

    #[test]
    fn large_photos_stay_on_the_server() {
        // Five base64 characters are about three bytes.
        let data = to_eas(&card(), 2);
        assert_eq!(data.child_text(c::Picture), None);
        assert_eq!(to_eas(&card(), 3).child_text(c::Picture), Some("iVBOR"));
    }

    #[test]
    fn round_trips_through_eas() {
        let first = to_eas(&card(), usize::MAX);
        let back = Value::Object(from_eas(&first));
        let second = to_eas(&back, usize::MAX);
        // What the device sees is stable across a round trip.
        assert_eq!(crate::wbxml::encode(&first), crate::wbxml::encode(&second));
        // Properties the device did not send are explicitly cleared.
        assert_eq!(back["relatedTo"], Value::Null);
    }

    #[test]
    fn reads_device_quirks() {
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(
                c::Email1Address,
                "\"Bob\" <bob@example.com>",
            ))
            .child(Element::with_text(c::Birthday, "1985-12-30T23:00:00.000Z"));
        let card = from_eas(&data);
        assert_eq!(card["emails"]["e1"]["address"], "bob@example.com");
        assert_eq!(card["anniversaries"]["d1"]["date"]["day"], 31);
        assert_eq!(card["name"], Value::Null);
    }

    #[test]
    fn rolls_dates_over() {
        assert_eq!(next_day(2024, 2, 28), (2024, 2, 29));
        assert_eq!(next_day(2023, 2, 28), (2023, 3, 1));
        assert_eq!(next_day(2026, 12, 31), (2027, 1, 1));
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        /// A word-ish value as a device would type it.
        fn value() -> impl Strategy<Value = String> {
            "[A-Za-zÀ-ÿ0-9'.-]{1,12}( [A-Za-zÀ-ÿ0-9'.-]{1,12}){0,2}"
        }

        proptest! {
            /// What a device sends survives the trip to JSContact and back.
            #[test]
            fn device_contacts_round_trip(
                fields in proptest::collection::btree_map(
                    prop::sample::select(vec![
                        c::FirstName, c::MiddleName, c::LastName, c::CompanyName,
                        c::JobTitle, c::Department, c::MobilePhoneNumber,
                        c::BusinessPhoneNumber, c::HomePhoneNumber,
                        c::HomeAddressCity, c::BusinessAddressStreet, c::WebPage,
                    ]).prop_map(|t| (t.page, t.token)),
                    value(),
                    1..8,
                ),
                email in proptest::option::of("[a-z]{1,8}@[a-z]{1,8}\\.(nl|com)"),
            ) {
                let mut data = Element::new(airsync::ApplicationData);
                for ((page, token), value) in &fields {
                    data.push(Element::with_text(Tag { page: *page, token: *token }, value.as_str()));
                }
                if let Some(email) = &email {
                    data.push(Element::with_text(c::Email1Address, email.as_str()));
                }
                let back = to_eas(&Value::Object(from_eas(&data)), usize::MAX);
                for ((page, token), value) in &fields {
                    let tag = Tag { page: *page, token: *token };
                    prop_assert_eq!(back.child_text(tag), Some(value.as_str()), "{}", tag);
                }
                prop_assert_eq!(back.child_text(c::Email1Address), email.as_deref());
            }
        }
    }
}
