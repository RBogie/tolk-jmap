//! Recipient lookup: people from address books, recent mail and the
//! directory, matched while a name is typed.

use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::*;

impl JmapClient {
    /// People whose name has a word starting with `text`, or whose address
    /// starts with it: address book contacts first, then people from recent
    /// mail (most frequent first), then the server directory if it allows
    /// queries. Each email address appears once.
    ///
    /// The server's own filters match whole words only, which is useless
    /// while a name is being typed, so contacts and correspondents are
    /// collected once (cached for `PEOPLE_TTL`) and matched here.
    pub async fn search_people(&self, text: &str, limit: usize) -> anyhow::Result<Vec<Person>> {
        let known = self.known_people().await?;
        let mut people: Vec<Person> = known
            .iter()
            .filter(|p| matches_person(text, &p.display_name, &p.email))
            .take(limit)
            .cloned()
            .collect();
        if people.len() < limit
            && let Some(account) = self.session.primary_accounts.get(PRINCIPALS)
        {
            for person in best_effort(
                "directory",
                self.search_principals(account, text, limit).await,
            )? {
                if !people
                    .iter()
                    .any(|p| p.email.eq_ignore_ascii_case(&person.email))
                {
                    people.push(person);
                }
            }
            people.truncate(limit);
        }
        Ok(people)
    }

    async fn known_people(&self) -> anyhow::Result<Arc<Vec<Person>>> {
        if let Some(people) = self.caches.people.get(&self.session.username) {
            return Ok(people);
        }
        let mut people = match self.session.primary_accounts.get(CONTACTS) {
            Some(account) => best_effort("address book", self.all_contacts(account).await)?,
            None => Vec::new(),
        };
        for person in best_effort("mail history", self.recent_correspondents().await)? {
            if !people
                .iter()
                .any(|p| p.email.eq_ignore_ascii_case(&person.email))
            {
                people.push(person);
            }
        }
        tracing::debug!(
            user = self.session.username,
            people = people.len(),
            "collected known people"
        );
        let people = Arc::new(people);
        self.caches
            .people
            .insert(self.session.username.clone(), people.clone());
        Ok(people)
    }

    async fn all_contacts(&self, account: &str) -> anyhow::Result<Vec<Person>> {
        /// Address books beyond this size are rare for a phone user.
        const MAX_CONTACTS: usize = 5000;
        let responses = self
            .request(
                &[CORE, CONTACTS],
                vec![
                    ("ContactCard/query", json!({ "accountId": account, "limit": MAX_CONTACTS })),
                    (
                        "ContactCard/get",
                        json!({
                            "accountId": account,
                            "#ids": { "resultOf": "0", "name": "ContactCard/query", "path": "/ids" },
                            "properties": ["name", "emails", "organizations", "phones"],
                        }),
                    ),
                ],
            )
            .await?;
        let list = method_result(responses, "ContactCard/get")?;
        Ok(list["list"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(Person::from_card)
            .collect())
    }

    /// Everyone in the From, To and Cc of recent mail, most frequent first.
    async fn recent_correspondents(&self) -> anyhow::Result<Vec<Person>> {
        /// Recent messages scanned; enough to find regular correspondents.
        const MESSAGES: usize = 500;
        #[derive(Deserialize)]
        struct Addresses {
            from: Option<Vec<EmailAddress>>,
            to: Option<Vec<EmailAddress>>,
            cc: Option<Vec<EmailAddress>>,
        }

        let (ids, _) = self.query_email_page(json!({}), 0, MESSAGES).await?;
        let own = self.session.username.to_lowercase();
        let mut counted: Vec<(Person, usize)> = Vec::new();
        for batch in ids.chunks(self.max_objects_in_get()) {
            let mut args = self
                .call(
                    "Email/get",
                    json!({ "accountId": self.account_id()?, "ids": batch, "properties": ["from", "to", "cc"] }),
                )
                .await?;
            let messages: Vec<Addresses> = serde_json::from_value(args["list"].take())?;
            let addresses = messages
                .into_iter()
                .flat_map(|m| [m.from, m.to, m.cc])
                .flatten()
                .flatten();
            for address in addresses {
                let Some(email) = address
                    .email
                    .filter(|e| !e.is_empty() && e.to_lowercase() != own)
                else {
                    continue;
                };
                let name = address.name.filter(|n| !n.is_empty());
                match counted
                    .iter_mut()
                    .find(|(p, _)| p.email.eq_ignore_ascii_case(&email))
                {
                    Some((person, count)) => {
                        *count += 1;
                        if person.display_name == person.email
                            && let Some(name) = name
                        {
                            person.display_name = name;
                        }
                    }
                    None => counted.push((
                        Person {
                            display_name: name.unwrap_or_else(|| email.clone()),
                            email,
                            ..Person::default()
                        },
                        1,
                    )),
                }
            }
        }
        // Stable, so equally frequent people stay in most-recent-first order.
        counted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        Ok(counted.into_iter().map(|(person, _)| person).collect())
    }

    async fn search_principals(
        &self,
        account: &str,
        text: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Person>> {
        let responses = self
            .request(
                &[CORE, PRINCIPALS],
                vec![
                    (
                        "Principal/query",
                        json!({ "accountId": account, "filter": { "text": text }, "limit": limit }),
                    ),
                    (
                        "Principal/get",
                        json!({
                            "accountId": account,
                            "#ids": { "resultOf": "0", "name": "Principal/query", "path": "/ids" },
                            "properties": ["name", "email", "type"],
                        }),
                    ),
                ],
            )
            .await?;
        let list = method_result(responses, "Principal/get")?;
        Ok(list["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| {
                p["type"]
                    .as_str()
                    .is_none_or(|t| t == "individual" || t == "group")
            })
            .filter_map(|p| {
                Some(Person {
                    display_name: p["name"].as_str().unwrap_or_default().to_owned(),
                    email: p["email"].as_str().filter(|e| !e.is_empty())?.to_owned(),
                    ..Person::default()
                })
            })
            .collect())
    }
}

/// Keeps a people source's failure from failing the whole lookup, except
/// for an expired login, which the caller must see.
fn best_effort(source: &str, result: anyhow::Result<Vec<Person>>) -> anyhow::Result<Vec<Person>> {
    match result {
        Ok(people) => Ok(people),
        Err(e) if is_unauthorized(&e) => Err(e),
        Err(e) => {
            tracing::debug!(
                source,
                error = format!("{e:#}"),
                "people source unavailable"
            );
            Ok(Vec::new())
        }
    }
}

/// Whether `text` starts a word of the name or starts the address.
fn matches_person(text: &str, name: &str, email: &str) -> bool {
    let text = text.trim().to_lowercase();
    !text.is_empty()
        && (email.to_lowercase().starts_with(&text)
            || name
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word.starts_with(&text))
            || name.to_lowercase().starts_with(&text))
}

/// The arguments of the `name` response, failing on any method error.
fn method_result(responses: Vec<(String, Value)>, name: &str) -> anyhow::Result<Value> {
    let mut found = None;
    for (method, args) in responses {
        if method == "error" {
            bail!("{name} failed: {args}");
        }
        if method == name {
            found = Some(args);
        }
    }
    found.with_context(|| format!("no {name} response"))
}

/// A person found in an address book or the server directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Person {
    pub display_name: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub company: Option<String>,
    pub phone: Option<String>,
    pub mobile_phone: Option<String>,
    pub email: String,
}

impl Person {
    /// One entry per email address of a JSContact card (RFC 9553).
    fn from_card(card: &Value) -> Vec<Person> {
        let component = |kind: &str| {
            card["name"]["components"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["kind"] == kind)
                .and_then(|c| c["value"].as_str())
                .map(str::to_owned)
        };
        let first_name = component("given");
        let last_name = component("surname");
        let display_name = card["name"]["full"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                [first_name.as_deref(), last_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        let company = card["organizations"]
            .as_object()
            .and_then(|o| o.values().find_map(|org| org["name"].as_str()))
            .map(str::to_owned);
        let phones: Vec<&Value> = card["phones"]
            .as_object()
            .map(|p| p.values().collect())
            .unwrap_or_default();
        let number = |mobile: bool| {
            phones
                .iter()
                .find(|p| (p["features"]["mobile"] == true) == mobile)
                .and_then(|p| p["number"].as_str())
                .map(str::to_owned)
        };
        let (mobile_phone, phone) = (number(true), number(false));
        card["emails"]
            .as_object()
            .into_iter()
            .flat_map(|emails| emails.values())
            .filter_map(|e| e["address"].as_str())
            .map(|email| Person {
                display_name: if display_name.is_empty() {
                    email.to_owned()
                } else {
                    display_name.clone()
                },
                first_name: first_name.clone(),
                last_name: last_name.clone(),
                company: company.clone(),
                phone: phone.clone(),
                mobile_phone: mobile_phone.clone(),
                email: email.to_owned(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_people_by_word_or_address_prefix() {
        assert!(matches_person("lau", "Laura de Vries", "l@example.com"));
        assert!(matches_person("vri", "Laura de Vries", "l@example.com"));
        assert!(matches_person("bob", "", "bob.smith@example.com"));
        assert!(matches_person(
            "Laura de",
            "Laura de Vries",
            "x@example.com"
        ));
        assert!(!matches_person("aura", "Laura", "x@example.com"));
        assert!(!matches_person(" ", "Laura", "x@example.com"));
    }

    #[test]
    fn reads_people_from_contact_cards() {
        let card = json!({
            "name": { "components": [
                { "kind": "given", "value": "Ann" },
                { "kind": "surname", "value": "Lee" },
            ]},
            "emails": { "e1": { "address": "ann@example.com" }, "e2": { "address": "ann@work.example" } },
            "organizations": { "o": { "name": "Acme" } },
            "phones": {
                "p1": { "number": "+31 6 1", "features": { "mobile": true } },
                "p2": { "number": "+31 20 2" },
            },
        });
        let people = Person::from_card(&card);
        assert_eq!(people.len(), 2);
        let ann = people
            .iter()
            .find(|p| p.email == "ann@example.com")
            .unwrap();
        assert_eq!(ann.display_name, "Ann Lee");
        assert_eq!(ann.last_name.as_deref(), Some("Lee"));
        assert_eq!(ann.company.as_deref(), Some("Acme"));
        assert_eq!(ann.mobile_phone.as_deref(), Some("+31 6 1"));
        assert_eq!(ann.phone.as_deref(), Some("+31 20 2"));

        let bare = Person::from_card(&json!({ "emails": { "e": { "address": "x@example.com" } } }));
        assert_eq!(bare[0].display_name, "x@example.com");
        assert!(Person::from_card(&json!({ "name": { "full": "No Mail" } })).is_empty());
    }
}
