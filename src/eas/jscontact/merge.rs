//! Device Changes to cards: only what the device changed is written.
//!
//! Each EAS field the device sent is compared with what the card shows it
//! now ([`to_eas`]). Only fields that differ are written, into the entry
//! that fills that EAS slot, so entries the slots cannot show (a fourth
//! email, a third phone, other kinds of entries) are kept.
//!
//! A field the device did not send counts as cleared, unless the device
//! does not manage it: with `<Supported>` (MS-ASCMD ghosting) only the
//! listed fields are managed, without it every field the mapping knows. The
//! photo and the notes only change when `<Picture>` or `<Body>` is sent
//! (MS-ASCMD 2.2.3.24); an empty one removes them.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::{
    ADDRESS_FIELDS, ADDRESS_KINDS, NAME_PARTS, PHONE_SLOTS, RELATED, address_entry, address_field,
    address_slots, bare_address, child_names, children, eas_date, email_entry, email_slots,
    entries_array, keyed, partial_date, phone_entry, phone_slots, related_name, to_eas, yomi,
};
use crate::wbxml::{
    Element, Tag,
    cp::{airsyncbase, contacts as c, contacts2 as c2},
};

/// The patch for a device Change of `current`: whole properties, merged.
/// `supported` lists the fields the device manages (tags as `Display`s
/// them), `None` when it sent no `<Supported>`; `max_picture` is the photo
/// limit the device was sent the card with (see [`to_eas`]).
pub fn merge(
    current: &Value,
    data: &Element,
    supported: Option<&[String]>,
    max_picture: usize,
) -> Map<String, Value> {
    let ctx = Ctx {
        current,
        data,
        shown: to_eas(current, max_picture),
        supported,
    };
    let mut patch = Map::new();
    name(&ctx, &mut patch);
    organization(&ctx, &mut patch);
    slot(
        &ctx,
        &mut patch,
        "titles",
        "t",
        c::JobTitle,
        first_title,
        |e, v| e["name"] = v.into(),
    );
    emails(&ctx, &mut patch);
    phones(&ctx, &mut patch);
    addresses(&ctx, &mut patch);
    office_location(&ctx, &mut patch);
    for (tag, kind) in [(c::Birthday, "birth"), (c::Anniversary, "wedding")] {
        let first = |card: &Value| first_key(card, "anniversaries", |a| a["kind"] == kind);
        slot(
            &ctx,
            &mut patch,
            "anniversaries",
            "d",
            tag,
            first,
            |e, v| {
                if let Some(date) = partial_date(v) {
                    e["date"] = date;
                }
            },
        );
    }
    let first_link = |card: &Value| first_key(card, "links", |l| l["uri"].is_string());
    slot(
        &ctx,
        &mut patch,
        "links",
        "l",
        c::WebPage,
        first_link,
        |e, v| e["uri"] = v.into(),
    );
    let first_nick = |card: &Value| first_key(card, "nicknames", |n| n["name"].is_string());
    slot(
        &ctx,
        &mut patch,
        "nicknames",
        "n",
        c2::NickName,
        first_nick,
        |e, v| e["name"] = v.into(),
    );
    online_services(&ctx, &mut patch);
    related(&ctx, &mut patch);
    keywords(&ctx, &mut patch);
    let first_photo = |card: &Value| first_key(card, "media", |m| m["kind"] == "photo");
    slot(
        &ctx,
        &mut patch,
        "media",
        "m",
        c::Picture,
        first_photo,
        |e, v| {
            if let Some(entry) = e.as_object_mut() {
                entry.remove("blobId");
            }
            e["mediaType"] = "image/jpeg".into();
            e["uri"] = format!("data:image/jpeg;base64,{v}").into();
        },
    );
    notes(&ctx, &mut patch);
    patch
}

struct Ctx<'a> {
    current: &'a Value,
    data: &'a Element,
    /// What the device has now.
    shown: Element,
    supported: Option<&'a [String]>,
}

impl Ctx<'_> {
    /// Whether the device manages `tag`: it sent it, or it is supported.
    fn manages(&self, tag: Tag) -> bool {
        self.data.find(tag).is_some()
            || (tag != c::Picture && self.supported.is_none_or(|s| s.contains(&tag.to_string())))
    }

    /// What the device sent for `tag`, in the form [`to_eas`] writes it.
    fn device(&self, tag: Tag) -> Option<String> {
        normalized(tag, self.data.child_text(tag)?)
    }

    fn shown(&self, tag: Tag) -> Option<String> {
        normalized(tag, self.shown.child_text(tag)?)
    }

    /// The device's new value for `tag` (`Some(None)`: cleared), or `None`
    /// when it did not change it.
    fn change(&self, tag: Tag) -> Option<Option<String>> {
        if !self.manages(tag) {
            return None;
        }
        let device = self.device(tag);
        (device != self.shown(tag)).then_some(device)
    }
}

fn normalized(tag: Tag, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(match tag {
        t if t == c::Email1Address || t == c::Email2Address || t == c::Email3Address => {
            bare_address(value).to_owned()
        }
        // Phones east of UTC send local midnight.
        t if t == c::Birthday || t == c::Anniversary => partial_date(value)
            .and_then(|d| eas_date(&d))
            .unwrap_or_else(|| value.to_owned()),
        _ => value.to_owned(),
    })
}

/// Writes the merged `Id[...]` map `property` into the patch.
fn edit(
    ctx: &Ctx,
    patch: &mut Map<String, Value>,
    property: &str,
    f: impl FnOnce(&mut Map<String, Value>),
) {
    let mut map = patch
        .get(property)
        .unwrap_or(&ctx.current[property])
        .as_object()
        .cloned()
        .unwrap_or_default();
    f(&mut map);
    let value = if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    };
    patch.insert(property.to_owned(), value);
}

/// A key `prefix<n>` not used in `map`.
fn fresh_key(map: &Map<String, Value>, prefix: &str) -> String {
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|k| !map.contains_key(k))
        .unwrap()
}

/// Sets one EAS slot: `key` is the entry in it now, `value` the device's;
/// `set` writes a value into an entry and `new` makes one.
fn put_slot(
    map: &mut Map<String, Value>,
    prefix: &str,
    key: Option<&str>,
    value: Option<&str>,
    set: impl FnOnce(&mut Value, &str),
    new: impl FnOnce(&str) -> Value,
) {
    match (key, value) {
        (Some(key), Some(value)) => {
            if let Some(entry) = map.get_mut(key) {
                set(entry, value);
            }
        }
        (Some(key), None) => {
            map.remove(key);
        }
        (None, Some(value)) => {
            let key = fresh_key(map, prefix);
            map.insert(key, new(value));
        }
        (None, None) => {}
    }
}

/// A single-slot field: `first` finds the entry the device sees, `set`
/// writes a value into an entry (a new entry starts empty).
fn slot(
    ctx: &Ctx,
    patch: &mut Map<String, Value>,
    property: &str,
    prefix: &str,
    tag: Tag,
    first: impl Fn(&Value) -> Option<String>,
    set: impl Fn(&mut Value, &str),
) {
    let Some(value) = ctx.change(tag) else {
        return;
    };
    let key = first(ctx.current);
    edit(ctx, patch, property, |map| {
        put_slot(map, prefix, key.as_deref(), value.as_deref(), &set, |v| {
            let mut entry = json!({});
            set(&mut entry, v);
            if property == "titles" {
                entry["kind"] = "title".into();
            } else if property == "media" {
                entry["kind"] = "photo".into();
            } else if property == "anniversaries" {
                entry["kind"] = if tag == c::Birthday {
                    "birth"
                } else {
                    "wedding"
                }
                .into();
            }
            entry
        });
    });
}

/// The key of the first entry (in id order) of `property` matching `f`.
fn first_key(card: &Value, property: &str, f: impl Fn(&Value) -> bool) -> Option<String> {
    keyed(&card[property])
        .into_iter()
        .find(|(_, v)| f(v))
        .map(|(k, _)| k.clone())
}

fn first_title(card: &Value) -> Option<String> {
    first_key(card, "titles", |t| t["kind"] != "role")
}

/// Name components: changed ones are replaced in place, new ones inserted
/// in the usual order; other kinds stay.
fn name(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let changes: Vec<(&str, Option<String>)> = NAME_PARTS
        .iter()
        .filter_map(|(tag, kind)| Some((*kind, ctx.change(*tag)?)))
        .collect();
    let pronunciations: Vec<(&str, Option<String>)> = ["given", "surname"]
        .into_iter()
        .filter_map(|kind| Some((kind, ctx.change(yomi(kind)?)?)))
        .collect();
    if changes.is_empty() && pronunciations.is_empty() {
        return;
    }
    const ORDER: [&str; 6] = [
        "title",
        "given",
        "given2",
        "surname",
        "generation",
        "credential",
    ];
    let rank = |kind: Option<&str>| kind.and_then(|k| ORDER.iter().position(|o| *o == k));
    let mut name = ctx.current["name"].as_object().cloned().unwrap_or_default();
    let mut components: Vec<Value> = name
        .get("components")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let before = components.clone();
    for (kind, value) in changes {
        let has = |k: &str| components.iter().any(|c| c["kind"] == k);
        // The suffix shows `generation`, or `credential` when there is none.
        let kind = if kind == "generation" && !has("generation") && has("credential") {
            "credential"
        } else {
            kind
        };
        let first = components.iter().position(|c| c["kind"] == kind);
        match (first, value) {
            (Some(at), Some(value)) => {
                components[at]["value"] = value.into();
                let mut seen = false;
                components.retain(|c| {
                    if c["kind"] != kind {
                        return true;
                    }
                    !std::mem::replace(&mut seen, true)
                });
            }
            (Some(_), None) => components.retain(|c| c["kind"] != kind),
            (None, Some(value)) => {
                let own = rank(Some(kind));
                let at = components
                    .iter()
                    .position(|c| rank(c["kind"].as_str()) > own)
                    .unwrap_or(components.len());
                components.insert(at, json!({ "kind": kind, "value": value }));
            }
            (None, None) => {}
        }
    }
    for (kind, value) in pronunciations {
        let Some(component) = components.iter_mut().find(|c| c["kind"] == kind) else {
            continue;
        };
        match value {
            Some(value) => component["phonetic"] = value.into(),
            None => {
                if let Some(component) = component.as_object_mut() {
                    component.remove("phonetic");
                }
            }
        }
    }
    // A full name would now be out of date.
    if let Some(old) = name.get("full").and_then(Value::as_str) {
        let full = edited_full(old, &before, &components)
            .unwrap_or_else(|| joined_full(&name, &components, &rank));
        if full.is_empty() {
            name.remove("full");
        } else {
            name.insert("full".to_owned(), full.into());
        }
    }
    if components.is_empty() {
        name.remove("components");
    } else {
        name.insert("components".to_owned(), components.into());
    }
    let value = if name.is_empty() {
        Value::Null
    } else {
        Value::Object(name)
    };
    patch.insert("name".to_owned(), value);
}

/// `full` with only the changed components replaced, when each old value
/// appears in it once; `None` otherwise (a component was added, or `full`
/// is written differently).
fn edited_full(full: &str, before: &[Value], after: &[Value]) -> Option<String> {
    let value = |c: &Value| c["value"].as_str().unwrap_or_default().to_owned();
    let mut full = full.to_owned();
    let mut kinds: Vec<&str> = Vec::new();
    for kind in before
        .iter()
        .chain(after)
        .filter_map(|c| c["kind"].as_str())
    {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    for kind in kinds.into_iter().filter(|k| *k != "separator") {
        let old: Vec<String> = before
            .iter()
            .filter(|c| c["kind"] == kind)
            .map(value)
            .collect();
        let new: Vec<String> = after
            .iter()
            .filter(|c| c["kind"] == kind)
            .map(value)
            .collect();
        if old == new {
            continue;
        }
        let ([old], new) = (old.as_slice(), new.as_slice()) else {
            return None;
        };
        if old.is_empty() || full.matches(old.as_str()).count() != 1 {
            return None;
        }
        full = match new {
            [new] => full.replacen(old.as_str(), new, 1),
            [] => full
                .replacen(old.as_str(), "", 1)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            _ => return None,
        };
    }
    Some(full.trim().to_owned())
}

/// A full name made from the components: in their order when `isOrdered`,
/// otherwise in the usual one, with the card's separators.
fn joined_full(
    name: &Map<String, Value>,
    components: &[Value],
    rank: &dyn Fn(Option<&str>) -> Option<usize>,
) -> String {
    let separator = name
        .get("defaultSeparator")
        .and_then(Value::as_str)
        .unwrap_or(" ");
    let mut parts: Vec<&Value> = components.iter().collect();
    if name.get("isOrdered") != Some(&Value::Bool(true)) {
        parts.retain(|c| c["kind"] != "separator");
        parts.sort_by_key(|c| rank(c["kind"].as_str()).unwrap_or(usize::MAX));
    }
    let mut full = String::new();
    let mut pending_separator = false;
    for part in parts {
        let text = part["value"].as_str().unwrap_or_default();
        if part["kind"] == "separator" {
            full.push_str(text);
            pending_separator = false;
            continue;
        }
        if text.is_empty() {
            continue;
        }
        if pending_separator {
            full.push_str(separator);
        }
        full.push_str(text);
        pending_separator = true;
    }
    full.trim().to_owned()
}

/// Company and department: the first organization and its first unit.
fn organization(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let (company, department, yomi_company) = (
        ctx.change(c::CompanyName),
        ctx.change(c::Department),
        ctx.change(c::YomiCompanyName),
    );
    if company.is_none() && department.is_none() && yomi_company.is_none() {
        return;
    }
    let key = first_key(ctx.current, "organizations", |_| true);
    edit(ctx, patch, "organizations", |map| {
        let key = key.unwrap_or_else(|| fresh_key(map, "o"));
        let mut org = map
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        match company {
            Some(Some(name)) => {
                org.insert("name".to_owned(), name.into());
            }
            Some(None) => {
                org.remove("name");
            }
            None => {}
        }
        match yomi_company {
            Some(Some(yomi)) => {
                org.insert("sortAs".to_owned(), yomi.into());
            }
            Some(None) => {
                org.remove("sortAs");
            }
            None => {}
        }
        if let Some(department) = department {
            let mut units: Vec<Value> = org
                .get("units")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match (department, units.is_empty()) {
                (Some(name), true) => units.push(json!({ "name": name })),
                (Some(name), false) => units[0]["name"] = name.into(),
                (None, false) => {
                    units.remove(0);
                }
                (None, true) => {}
            }
            if units.is_empty() {
                org.remove("units");
            } else {
                org.insert("units".to_owned(), units.into());
            }
        }
        if org.contains_key("name") || org.contains_key("units") {
            map.insert(key, Value::Object(org));
        } else {
            map.remove(&key);
        }
    });
}

/// The three email slots. Entries are matched by address, so when the
/// device removes Email1 and moves the others up, each address keeps its
/// own entry (contexts, label) and only the preference follows the slots.
fn emails(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let tags = [c::Email1Address, c::Email2Address, c::Email3Address];
    let changes: Vec<_> = tags.iter().map(|t| ctx.change(*t)).collect();
    if changes.iter().all(Option::is_none) {
        return;
    }
    let wanted: Vec<Option<String>> = tags
        .iter()
        .zip(changes)
        .map(|(tag, change)| change.unwrap_or_else(|| ctx.shown(*tag)))
        .collect();
    let same = |e: &Value, address: &str| {
        e["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(address))
    };
    let keys: Vec<String> = email_slots(ctx.current)
        .into_iter()
        .map(str::to_owned)
        .collect();
    // Among emails without a preference, a new one with one would go first.
    let ranked = keyed(&ctx.current["emails"])
        .iter()
        .all(|(_, e)| e["pref"].is_u64());
    let reordered = keys.len() != wanted.iter().flatten().count()
        || keys
            .iter()
            .zip(wanted.iter().flatten())
            .any(|(key, address)| !same(&ctx.current["emails"][key], address));
    edit(ctx, patch, "emails", |map| {
        // Slot entries whose address the device no longer has; a new address
        // in the same slot takes its entry over (an edited address).
        let gone: Vec<(usize, String)> = keys
            .iter()
            .enumerate()
            .filter(|(_, key)| {
                map.get(*key)
                    .is_none_or(|e| !wanted.iter().flatten().any(|a| same(e, a)))
            })
            .map(|(i, key)| (i, key.clone()))
            .collect();
        let mut reused = Vec::new();
        for (i, address) in wanted.iter().enumerate() {
            let Some(address) = address else { continue };
            if let Some(entry) = map.values_mut().find(|e| same(e, address)) {
                if ranked && reordered {
                    entry["pref"] = (i + 1).into();
                }
                continue;
            }
            if let Some((_, key)) = gone.iter().find(|(slot, _)| *slot == i)
                && let Some(entry) = map.get_mut(key)
            {
                entry["address"] = address.as_str().into();
                if ranked && reordered {
                    entry["pref"] = (i + 1).into();
                }
                reused.push(key.clone());
                continue;
            }
            let mut email = email_entry(i, address);
            if !ranked && let Some(email) = email.as_object_mut() {
                email.remove("pref");
            }
            let key = fresh_key(map, "e");
            map.insert(key, email);
        }
        for (_, key) in gone.iter().filter(|(_, k)| !reused.contains(k)) {
            map.remove(key);
        }
    });
}

fn phones(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let changes: Vec<_> = PHONE_SLOTS
        .iter()
        .filter_map(|t| Some((*t, ctx.change(*t)?)))
        .collect();
    if changes.is_empty() {
        return;
    }
    let slots = phone_slots(ctx.current);
    edit(ctx, patch, "phones", |map| {
        for (tag, value) in changes {
            let key = slots.iter().find(|(t, _)| *t == tag).map(|(_, k)| *k);
            put_slot(
                map,
                "p",
                key,
                value.as_deref(),
                |e, v| e["number"] = v.into(),
                |v| phone_entry(tag, v),
            );
        }
    });
}

/// One address per kind; a changed one keeps its other components and
/// properties, but loses `full`, which would be out of date.
fn addresses(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let changed: Vec<_> = ADDRESS_FIELDS
        .iter()
        .filter_map(|(kind, tags)| {
            let changes = tags.map(|t| ctx.change(t));
            changes
                .iter()
                .any(Option::is_some)
                .then_some((*kind, changes))
        })
        .collect();
    if changed.is_empty() {
        return;
    }
    let slots = address_slots(ctx.current);
    edit(ctx, patch, "addresses", |map| {
        for (kind, changes) in changed {
            let key = slots.iter().find(|(k, _)| *k == kind).map(|(_, k)| *k);
            let Some(key) = key else {
                let fields = changes
                    .each_ref()
                    .map(|c| c.as_ref().and_then(|v| v.as_deref()));
                if let Some(address) = address_entry(kind, fields) {
                    let key = fresh_key(map, "a");
                    map.insert(key, address);
                }
                continue;
            };
            let current = &ctx.current["addresses"][key];
            // Parts EAS shows elsewhere (the office) or not at all keep it.
            let shown_kinds = ["name", "number", "separator"];
            let other_parts = entries_array(&current["components"]).any(|c| {
                !shown_kinds
                    .iter()
                    .chain(&ADDRESS_KINDS)
                    .any(|k| c["kind"] == *k)
            });
            let remains = other_parts
                || (0..5).any(|field| match &changes[field] {
                    Some(value) => value.is_some(),
                    None => !address_field(current, field).is_empty(),
                });
            if !remains {
                map.remove(key);
                continue;
            }
            let Some(Value::Object(address)) = map.get_mut(key) else {
                continue;
            };
            let mut components: Vec<Value> = address
                .get("components")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // A street shown from `full` becomes a component, since `full` goes.
            let street_kinds = ["name", "number"];
            if changes[0].is_none()
                && !entries_array(&address["components"])
                    .any(|c| street_kinds.iter().any(|k| c["kind"] == *k))
            {
                let street = address_field(current, 0);
                if !street.is_empty() {
                    components.push(json!({ "kind": "name", "value": street }));
                }
            }
            address.remove("full");
            let kinds = std::iter::once(&street_kinds[..]).chain(ADDRESS_KINDS.chunks(1));
            for (change, kinds) in changes.into_iter().zip(kinds) {
                let Some(value) = change else { continue };
                // Replaced where it was, so the order stays.
                let at = components
                    .iter()
                    .position(|c| kinds.iter().any(|k| c["kind"] == *k))
                    .unwrap_or(components.len());
                components.retain(|c| !kinds.iter().any(|k| c["kind"] == *k));
                if let Some(value) = value {
                    components.insert(
                        at.min(components.len()),
                        json!({ "kind": kinds[0], "value": value }),
                    );
                }
            }
            address.insert("components".to_owned(), components.into());
        }
    });
}

/// The office is the room of the work address (made when there is none).
fn office_location(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let Some(value) = ctx.change(c::OfficeLocation) else {
        return;
    };
    let shown = address_slots(ctx.current)
        .into_iter()
        .find(|(kind, _)| *kind == "work")
        .map(|(_, key)| key.to_owned());
    edit(ctx, patch, "addresses", |map| {
        let key = shown.filter(|k| map.contains_key(k)).or_else(|| {
            map.iter()
                .find(|(_, a)| a["contexts"]["work"] == true)
                .map(|(k, _)| k.clone())
        });
        match key {
            Some(key) => {
                let Some(address) = map.get_mut(&key) else {
                    return;
                };
                super::set_office(address, value.as_deref());
                let empty = entries_array(&address["components"]).next().is_none();
                if empty && !address["full"].is_string() {
                    map.remove(&key);
                }
            }
            None => {
                if let Some(office) = value {
                    let key = fresh_key(map, "a");
                    map.insert(
                        key,
                        json!({ "contexts": { "work": true }, "components": [{ "kind": "room", "value": office }] }),
                    );
                }
            }
        }
    });
}

/// IM addresses: the first three services, in id order, matched by
/// address like the emails.
fn online_services(ctx: &Ctx, patch: &mut Map<String, Value>) {
    let tags = [c2::IMAddress, c2::IMAddress2, c2::IMAddress3];
    let changes: Vec<_> = tags.iter().map(|t| ctx.change(*t)).collect();
    if changes.iter().all(Option::is_none) {
        return;
    }
    let wanted: Vec<String> = tags
        .iter()
        .zip(changes)
        .filter_map(|(tag, change)| change.unwrap_or_else(|| ctx.shown(*tag)))
        .collect();
    let user = |e: &Value| e["user"].as_str().or(e["uri"].as_str()).map(str::to_owned);
    let keys: Vec<String> = keyed(&ctx.current["onlineServices"])
        .into_iter()
        .take(3)
        .map(|(k, _)| k.clone())
        .collect();
    edit(ctx, patch, "onlineServices", |map| {
        let mut gone: Vec<String> = keys
            .iter()
            .filter(|key| {
                map.get(*key)
                    .and_then(user)
                    .is_none_or(|u| !wanted.contains(&u))
            })
            .cloned()
            .collect();
        for address in &wanted {
            if map
                .values()
                .any(|e| user(e).as_deref() == Some(address.as_str()))
            {
                continue;
            }
            // An edited address keeps its entry.
            if !gone.is_empty()
                && let Some(entry) = map.get_mut(&gone.remove(0))
            {
                entry["user"] = address.as_str().into();
                if let Some(entry) = entry.as_object_mut() {
                    entry.remove("uri");
                }
                continue;
            }
            let key = fresh_key(map, "im");
            map.insert(key, json!({ "user": address }));
        }
        for key in gone {
            map.remove(&key);
        }
    });
}

/// Spouse, assistant, manager and children are `relatedTo` entries keyed by
/// the name. A person keeps their entry, and other relations on it, when
/// only one relation changes.
fn related(ctx: &Ctx, patch: &mut Map<String, Value>) {
    for (tag, relation) in RELATED {
        let Some(value) = ctx.change(tag) else {
            continue;
        };
        let old = related_name(ctx.current, relation).map(str::to_owned);
        edit(ctx, patch, "relatedTo", |map| {
            set_relation(map, old.as_deref(), value.as_deref(), relation);
        });
    }
    if !ctx.manages(c::Children) {
        return;
    }
    let device = child_names(ctx.data);
    let shown: Vec<String> = children(ctx.current)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if device == shown {
        return;
    }
    edit(ctx, patch, "relatedTo", |map| {
        for name in shown.iter().filter(|n| !device.contains(n)) {
            set_relation(map, Some(name), None, "child");
        }
        for name in device.iter().filter(|n| !shown.contains(n)) {
            set_relation(map, None, Some(name), "child");
        }
    });
}

/// Moves `relation` from the person `old` to `new` (either may be absent):
/// an entry left without relations goes, one that gains it is made.
fn set_relation(
    map: &mut Map<String, Value>,
    old: Option<&str>,
    new: Option<&str>,
    relation: &str,
) {
    if old == new {
        return;
    }
    if let Some(old) = old
        && let Some(entry) = map.get_mut(old)
    {
        if let Some(relations) = entry["relation"].as_object_mut() {
            relations.remove(relation);
        }
        if entry["relation"].as_object().is_none_or(|r| r.is_empty()) {
            // Moved to the new name: it keeps the entry's other properties.
            let entry = map.remove(old);
            if let (Some(new), Some(mut entry)) = (new, entry)
                && !map.contains_key(new)
            {
                entry["relation"] = json!({ relation: true });
                map.insert(new.to_owned(), entry);
                return;
            }
        }
    }
    if let Some(new) = new {
        let entry = map
            .entry(new.to_owned())
            .or_insert_with(|| json!({ "relation": {} }));
        entry["relation"][relation] = true.into();
    }
}

fn keywords(ctx: &Ctx, patch: &mut Map<String, Value>) {
    if !ctx.manages(c::Categories) {
        return;
    }
    let names = |data: &Element| -> BTreeSet<String> {
        data.find(c::Categories)
            .into_iter()
            .flat_map(|c| c.find_all(c::Category))
            .filter_map(|c| c.text().map(str::trim).filter(|t| !t.is_empty()))
            .map(str::to_owned)
            .collect()
    };
    let device = names(ctx.data);
    if device == names(&ctx.shown) {
        return;
    }
    edit(ctx, patch, "keywords", |map| {
        map.retain(|name, v| *v != true && !device.contains(name));
        for name in device {
            map.insert(name, Value::Bool(true));
        }
    });
}

/// The body shows every note; a changed one replaces them. A Change
/// without a body leaves them as they are (MS-ASCMD 2.2.3.24).
fn notes(ctx: &Ctx, patch: &mut Map<String, Value>) {
    if ctx.data.find(airsyncbase::Body).is_none() {
        return;
    }
    // A body of a type notes cannot be is refused before this (see
    // `UNSUPPORTED_FIELDS`), or else left out.
    if super::super::device_body::unsupported(ctx.data, super::BODY_TYPES).is_some() {
        return;
    }
    let device = super::note_from_eas(ctx.data);
    let shown = super::note_from_eas(&ctx.shown);
    if device.as_deref().map(str::trim) == shown.as_deref().map(str::trim) {
        return;
    }
    edit(ctx, patch, "notes", |map| match device {
        // A single note keeps its other properties.
        Some(note) if map.len() == 1 => {
            if let Some(entry) = map.values_mut().next() {
                entry["note"] = note.into();
            }
        }
        Some(note) => {
            map.clear();
            map.insert("n1".to_owned(), json!({ "note": note }));
        }
        None => map.clear(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::cp::airsync;

    fn card() -> Value {
        json!({
            "name": { "full": "Ann Lee", "components": [
                { "kind": "given", "value": "Ann" },
                { "kind": "surname", "value": "Lee" },
            ]},
            "emails": {
                "a": { "address": "ann@example.com", "pref": 1, "contexts": { "private": true } },
                "b": { "address": "ann@work.example", "pref": 2 },
                "c": { "address": "ann@third.example", "pref": 3 },
                "d": { "address": "ann@fourth.example", "pref": 4 },
            },
            "phones": {
                "1": { "number": "+31 6 1", "features": { "mobile": true } },
                "2": { "number": "+31 30 2" },
                "3": { "number": "+31 30 3" },
                "4": { "number": "+31 30 4", "label": "cabin" },
            },
            "addresses": {
                "h": { "contexts": { "private": true }, "components": [
                    { "kind": "name", "value": "Main Street" },
                    { "kind": "number", "value": "1" },
                    { "kind": "locality", "value": "Utrecht" },
                ]},
                "w": { "contexts": { "work": true }, "full": "Work Road 2" },
                "x": { "contexts": { "private": true }, "full": "Holiday Lane 3" },
                "y": { "contexts": { "billing": true }, "full": "Billing Way 4" },
            },
            "media": {
                "p": { "kind": "photo", "uri": "data:image/png;base64,iVBOR" },
                "l": { "kind": "logo", "uri": "https://example.com/logo.png" },
            },
            "notes": { "n": { "note": "Met at FOSDEM", "author": { "name": "Rob" } } },
        })
    }

    /// The card as the device has it, with `edit` applied.
    fn device(card: &Value, edit: impl FnOnce(&mut Element)) -> Element {
        let mut data = to_eas(card, usize::MAX);
        edit(&mut data);
        data
    }

    fn set(data: &mut Element, tag: Tag, value: &str) {
        remove(data, tag);
        data.push(Element::with_text(tag, value));
    }

    fn remove(data: &mut Element, tag: Tag) {
        data.children
            .retain(|n| !matches!(n, crate::wbxml::Node::Element(e) if e.tag == tag));
    }

    fn apply(card: &Value, patch: Map<String, Value>) -> Value {
        let mut card = card.clone();
        for (k, v) in patch {
            card[k] = v;
        }
        card
    }

    #[test]
    fn unchanged_data_writes_nothing() {
        let card = card();
        assert_eq!(
            merge(&card, &device(&card, |_| {}), None, usize::MAX),
            Map::new()
        );
    }

    #[test]
    fn slot_edits_keep_entries_beyond_the_slots() {
        let card = card();
        let data = device(&card, |d| {
            set(d, c::Email2Address, "ann@new.example");
            set(d, c::HomePhoneNumber, "+31 30 9");
            set(d, c::HomeAddressCity, "Amsterdam");
        });
        let patch = merge(&card, &data, None, usize::MAX);
        assert_eq!(
            patch.keys().collect::<Vec<_>>(),
            ["addresses", "emails", "phones"]
        );
        let after = apply(&card, patch);
        assert_eq!(after["emails"]["b"]["address"], "ann@new.example");
        assert_eq!(after["emails"]["a"]["contexts"]["private"], true);
        assert_eq!(after["emails"]["d"]["address"], "ann@fourth.example");
        assert_eq!(after["phones"]["2"]["number"], "+31 30 9");
        assert_eq!(after["phones"]["4"]["label"], "cabin");
        let home = &after["addresses"]["h"]["components"];
        assert!(home.to_string().contains("Amsterdam"), "{home}");
        assert!(home.to_string().contains("Main Street"), "{home}");
        assert_eq!(after["addresses"]["x"]["full"], "Holiday Lane 3");
        assert_eq!(after["addresses"]["y"]["full"], "Billing Way 4");
    }

    #[test]
    fn cleared_slots_remove_only_their_entry() {
        let card = card();
        let data = device(&card, |d| {
            remove(d, c::Email3Address);
            remove(d, c::BusinessAddressStreet);
        });
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        let emails = after["emails"].as_object().unwrap();
        assert_eq!(emails.keys().collect::<Vec<_>>(), ["a", "b", "d"]);
        assert!(after["addresses"].get("w").is_none());
        assert!(after["addresses"].get("y").is_some());
        assert_eq!(after["phones"], card["phones"]);
    }

    #[test]
    fn new_values_get_new_entries() {
        let card = json!({ "emails": { "e1": { "address": "x@example.com" } } });
        let data = device(&card, |d| {
            set(d, c::Email2Address, "y@example.com");
            set(d, c::CompanyName, "Acme");
            set(d, c::BusinessPhoneNumber, "+1 2");
        });
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["emails"]["e2"]["address"], "y@example.com");
        assert_eq!(after["organizations"]["o1"]["name"], "Acme");
        assert_eq!(after["phones"]["p1"]["contexts"]["work"], true);
        assert_eq!(
            to_eas(&after, usize::MAX).child_text(c::Email2Address),
            Some("y@example.com")
        );
    }

    #[test]
    fn an_omitted_picture_keeps_the_photo() {
        let card = card();
        let data = device(&card, |d| remove(d, c::Picture));
        assert_eq!(merge(&card, &data, None, usize::MAX), Map::new());

        let data = device(&card, |d| set(d, c::Picture, ""));
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert!(after["media"].get("p").is_none());
        assert_eq!(after["media"]["l"]["kind"], "logo");

        let data = device(&card, |d| set(d, c::Picture, "AAAA"));
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["media"]["p"]["uri"], "data:image/jpeg;base64,AAAA");
    }

    #[test]
    fn an_omitted_body_keeps_the_notes() {
        let card = card();
        let data = device(&card, |d| remove(d, airsyncbase::Body));
        assert_eq!(merge(&card, &data, None, usize::MAX), Map::new());

        let data = device(&card, |d| {
            remove(d, airsyncbase::Body);
            d.push(
                Element::new(airsyncbase::Body)
                    .child(Element::with_text(airsyncbase::Type, "1"))
                    .child(Element::new(airsyncbase::Data)),
            );
        });
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["notes"], Value::Null);
    }

    #[test]
    fn supported_limits_what_omissions_clear() {
        let card = card();
        let supported = vec![c::Email1Address.to_string(), c::FirstName.to_string()];
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(c::FirstName, "Anne"))
            .child(Element::with_text(c::MobilePhoneNumber, "+31 6 2"));
        let after = apply(&card, merge(&card, &data, Some(&supported), usize::MAX));
        // Listed but not sent: cleared.
        assert!(after["emails"].get("a").is_none());
        assert_eq!(after["emails"]["b"]["address"], "ann@work.example");
        // Sent: written; neither listed nor sent: kept.
        assert_eq!(after["phones"]["1"]["number"], "+31 6 2");
        assert_eq!(after["phones"]["2"]["number"], "+31 30 2");
        assert_eq!(after["name"]["components"][0]["value"], "Anne");
        assert_eq!(after["name"]["components"][1]["value"], "Lee");
        assert_eq!(after["name"]["full"], "Anne Lee");
        assert_eq!(after["notes"], card["notes"]);
        assert_eq!(after["addresses"], card["addresses"]);

        // Without Supported, the omitted fields are cleared, except the
        // notes and the photo.
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert!(after["name"]["components"].as_array().unwrap().len() == 1);
        assert_eq!(after["notes"], card["notes"]);
        assert_eq!(after["media"], card["media"]);
        assert_eq!(after["addresses"]["y"]["full"], "Billing Way 4");
    }

    #[test]
    fn renumbered_emails_keep_their_entries() {
        let card = card();
        // Email1 removed on the phone: the others move up a slot.
        let data = device(&card, |d| {
            set(d, c::Email1Address, "ann@work.example");
            set(d, c::Email2Address, "ann@third.example");
            remove(d, c::Email3Address);
        });
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert!(after["emails"].get("a").is_none(), "removed");
        assert_eq!(after["emails"]["b"]["address"], "ann@work.example");
        assert_eq!(after["emails"]["b"]["pref"], 1);
        assert_eq!(after["emails"]["c"]["pref"], 2);
        assert_eq!(after["emails"]["d"]["address"], "ann@fourth.example");
    }

    #[test]
    fn a_name_edit_keeps_how_full_is_written() {
        let mut card = card();
        card["name"]["full"] = "Lee, Ann".into();
        let data = device(&card, |d| set(d, c::FirstName, "Anne"));
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["name"]["full"], "Lee, Anne");

        // A component that was not there: rebuilt in the usual order.
        let data = device(&card, |d| set(d, c::MiddleName, "M."));
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["name"]["full"], "Ann M. Lee");
    }

    #[test]
    fn related_people_and_office_merge() {
        let card = super::super::tests::extras();
        let data = device(&card, |d| {
            remove(d, c::Children);
            d.push(
                Element::new(c::Children)
                    .child(Element::with_text(c::Child, "Tom"))
                    .child(Element::with_text(c::Child, "Lisa")),
            );
            set(d, c2::ManagerName, "Carol");
            set(d, c::OfficeLocation, "Building 5");
            set(d, c::YomiFirstName, "ケンジ");
        });
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        let related = &after["relatedTo"];
        assert!(related.get("Kim").is_none(), "child removed");
        assert_eq!(related["Tom"]["relation"]["child"], true);
        assert_eq!(related["Lisa"]["relation"]["child"], true);
        assert!(related.get("Bob").is_none());
        assert_eq!(related["Carol"]["relation"]["manager"], true);
        assert_eq!(related["Mary"]["relation"]["spouse"], true);
        let work = &after["addresses"]["w"]["components"];
        assert_eq!(work[0], json!({ "kind": "room", "value": "Building 5" }));
        assert_eq!(work[1]["value"], "Main St 1");
        assert_eq!(after["name"]["components"][0]["phonetic"], "ケンジ");
    }

    #[test]
    fn an_edited_address_field_stays_in_place() {
        let card = card();
        let data = device(&card, |d| set(d, c::HomeAddressStreet, "Side Street 2"));
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        let home = after["addresses"]["h"]["components"].as_array().unwrap();
        assert_eq!(home[0]["value"], "Side Street 2");
        assert_eq!(home.last().unwrap()["kind"], "locality");
    }

    #[test]
    fn changed_notes_keep_their_properties() {
        let card = card();
        let mut data = to_eas(&card, usize::MAX);
        remove(&mut data, airsyncbase::Body);
        data.push(
            Element::new(airsyncbase::Body)
                .child(Element::with_text(airsyncbase::Type, "1"))
                .child(Element::with_text(airsyncbase::Data, "Met in Brussels")),
        );
        let after = apply(&card, merge(&card, &data, None, usize::MAX));
        assert_eq!(after["notes"]["n"]["note"], "Met in Brussels");
        assert_eq!(after["notes"]["n"]["author"]["name"], "Rob");
    }

    #[test]
    fn dates_compare_as_days() {
        let card = json!({ "anniversaries": { "b": { "kind": "birth", "date": { "@type": "PartialDate", "year": 1990, "month": 5, "day": 1 } } } });
        // Local midnight in UTC+2, as phones send it.
        let data = Element::new(airsync::ApplicationData)
            .child(Element::with_text(c::Birthday, "1990-04-30T22:00:00.000Z"));
        assert_eq!(merge(&card, &data, None, usize::MAX), Map::new());
    }
}
