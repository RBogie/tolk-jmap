//! A scripted Exchange ActiveSync client for testing Tolk end to end.
//!
//! ```text
//! eas-client --url https://mail.example.com --user me@example.com \
//!     --password-file ~/pw.txt [--device probe1] <command> [args]
//!
//! commands:
//!   provision                         both policy phases; caches the policy key
//!   foldersync [key]                  default key 0
//!   sync <collection> <key> [window]  key 0 primes, then fetch with plain bodies
//!   sync-read <collection> <key> <server-id> <0|1>   mark read or unread
//!   sync-delete <collection> <key> <server-id>
//!   change-contact <collection> <key> <server-id> <Field=value>...
//!                                     a contact Change with only these fields
//!                                     (Contacts tag names; an empty value
//!                                     sends an empty element)
//!   add-event <collection> <key> <subject> <start> <minutes> [attendee]
//!                                     start as 20261105T090000Z; an attendee
//!                                     makes it a meeting (invitations go out)
//!   folder-create <key> <parent> <name> [type]   type 12 mail (default), 13 calendar, 14 contacts
//!   folder-update <key> <server-id> <parent> <name>
//!   folder-delete <key> <server-id>
//!   estimate <collection> <key>
//!   fetch <collection> <server-id> [body-type]   ItemOperations of one item
//!                                     (body type 1 plain, 2 HTML, 4 MIME)
//!   fetch-attachment <file-reference> [start-end]  ItemOperations of one
//!                                     attachment, or a range of it
//!   move <server-id> <from-folder> <to-folder>
//!   ping <heartbeat> <folder[:Class]>...
//!   sendmail <to> <subject> [body]
//!   smartreply <collection> <server-id> <to> <body>
//!   meeting-response <collection> <request-id> <1|2|3>   accept/tentative/decline
//!   search-gal <query>
//!   scenario <file>                   runs a scenario (see below)
//! ```
//!
//! Responses are printed as XML. The policy key is cached per user and
//! device in `$EAS_CLIENT_STATE` (default `./.eas-client`).
//!
//! A scenario file has one step per line: a command with its arguments,
//! then checks after `|` separators. `#` starts a comment; `$name` in an
//! argument is replaced by a saved value; quotes group words.
//!
//! ```text
//! login <user> <password-file> [device]   switch account (and device)
//! foldersync 0 | expect Status=1 | folder 2 inbox
//! sync $inbox 0 | save SyncKey key
//! sync $inbox $key | count Add=1 | retry 30
//! ```
//!
//! Checks: `expect <Tag>=<value>` (some element has that text; `~` instead
//! of `=` means it contains the value), `count
//! <Tag>=<n>`, `save <Tag> <name>` (first such text), `folder <type> <name>`
//! (the ServerId of the first folder of that EAS type), `item <Tag> <name>`
//! (the ServerId of the first Add or Change containing that tag), `within
//! <Tag>=<value>` or `~` (later checks only look at the first Add or Change with
//! that value) and `retry <seconds>` (repeat the step until its checks
//! pass). Tags match by name, or `Namespace:Name`. `$run` is unique per run;
//! `$user1`, `$password1`, `$user2` and `$password2` are read from the
//! environment variables `EAS_USER1`, `EAS_PASSWORD_FILE1` and so on.

use std::{collections::HashMap, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, bail};
use tolk::client::{EasClient, text};
use tolk::wbxml::{
    self, Element,
    cp::{
        airsync, airsyncbase, calendar as cal, compose_mail as cm, email as em,
        folder_hierarchy as fh, item_estimate as ie, item_operations as io, meeting_response as mr,
        move_items as mv, ping, search as sp,
    },
};

/// The shared EAS client, with the policy key kept in a file per user and
/// device between runs.
struct Client {
    eas: EasClient,
    state_dir: PathBuf,
}

impl Client {
    fn new(url: &str, user: &str, password: &str, device: &str, state_dir: PathBuf) -> Self {
        let mut client = Self {
            eas: EasClient::new(url, user, password, device, "EasClient"),
            state_dir,
        };
        if let Ok(key) = std::fs::read_to_string(client.key_file()) {
            client.eas.policy_key = key.trim().to_owned();
        }
        client
    }

    fn key_file(&self) -> PathBuf {
        self.state_dir.join(format!(
            "{}-{}.policykey",
            self.eas.user, self.eas.device_id
        ))
    }

    async fn send(&self, cmd: &str, body: Option<Element>) -> anyhow::Result<Option<Element>> {
        let reply = self.eas.send(cmd, body).await?;
        if !reply.status.is_success() {
            bail!("{cmd}: HTTP {}", reply.status);
        }
        Ok(reply.body)
    }
}

async fn run(
    client: &mut Client,
    command: &str,
    args: &[String],
) -> anyhow::Result<Option<Element>> {
    let arg = |i: usize, name: &str| {
        args.get(i)
            .cloned()
            .with_context(|| format!("missing <{name}>"))
    };
    match command {
        "provision" => {
            let response = client.eas.provision().await?;
            std::fs::create_dir_all(&client.state_dir)?;
            std::fs::write(client.key_file(), &client.eas.policy_key)?;
            Ok(Some(response))
        }
        "foldersync" => {
            let key = args.first().cloned().unwrap_or_else(|| "0".to_owned());
            client
                .send(
                    "FolderSync",
                    Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, key))),
                )
                .await
        }
        "sync" => {
            let (collection, key) = (arg(0, "collection")?, arg(1, "key")?);
            let mut c = Element::new(airsync::Collection)
                .child(text(airsync::SyncKey, key.clone()))
                .child(text(airsync::CollectionId, collection));
            if key != "0" {
                c.push(Element::new(airsync::GetChanges));
                c.push(text(
                    airsync::WindowSize,
                    args.get(2).cloned().unwrap_or_else(|| "25".to_owned()),
                ));
                c.push(
                    Element::new(airsync::Options).child(
                        Element::new(airsyncbase::BodyPreference)
                            .child(text(airsyncbase::Type, "1"))
                            .child(text(airsyncbase::TruncationSize, "500")),
                    ),
                );
            }
            client
                .send(
                    "Sync",
                    Some(
                        Element::new(airsync::Sync)
                            .child(Element::new(airsync::Collections).child(c)),
                    ),
                )
                .await
        }
        "estimate" => {
            let body = Element::new(ie::GetItemEstimate).child(
                Element::new(ie::Collections).child(
                    Element::new(ie::Collection)
                        .child(text(airsync::SyncKey, arg(1, "key")?))
                        .child(text(ie::CollectionId, arg(0, "collection")?)),
                ),
            );
            client.send("GetItemEstimate", Some(body)).await
        }
        "fetch" => {
            let mut fetch = Element::new(io::Fetch)
                .child(text(io::Store, "Mailbox"))
                .child(text(airsync::CollectionId, arg(0, "collection")?))
                .child(text(airsync::ServerId, arg(1, "server-id")?));
            if let Some(body_type) = args.get(2) {
                fetch.push(
                    Element::new(io::Options).child(
                        Element::new(airsyncbase::BodyPreference)
                            .child(text(airsyncbase::Type, body_type.as_str())),
                    ),
                );
            }
            let body = Element::new(io::ItemOperations).child(fetch);
            client.send("ItemOperations", Some(body)).await
        }
        "fetch-attachment" => {
            let mut fetch = Element::new(io::Fetch)
                .child(text(io::Store, "Mailbox"))
                .child(text(airsyncbase::FileReference, arg(0, "file-reference")?));
            if let Some(range) = args.get(1) {
                fetch.push(Element::new(io::Options).child(text(io::Range, range.as_str())));
            }
            let body = Element::new(io::ItemOperations).child(fetch);
            client.send("ItemOperations", Some(body)).await
        }
        "meeting-response" => {
            let body = Element::new(mr::MeetingResponse).child(
                Element::new(mr::Request)
                    .child(text(mr::UserResponse, arg(2, "1|2|3")?))
                    .child(text(mr::CollectionId, arg(0, "collection")?))
                    .child(text(mr::RequestId, arg(1, "request-id")?)),
            );
            client.send("MeetingResponse", Some(body)).await
        }
        "sync-read" | "sync-delete" | "add-event" | "change-contact" => {
            let (collection, key) = (arg(0, "collection")?, arg(1, "key")?);
            let command = match command {
                "sync-read" => Element::new(airsync::Change)
                    .child(text(airsync::ServerId, arg(2, "server-id")?))
                    .child(
                        Element::new(airsync::ApplicationData)
                            .child(text(em::Read, arg(3, "0|1")?)),
                    ),
                "sync-delete" => Element::new(airsync::Delete)
                    .child(text(airsync::ServerId, arg(2, "server-id")?)),
                "change-contact" => {
                    let mut data = Element::new(airsync::ApplicationData);
                    for field in args.iter().skip(3) {
                        let (name, value) = field.split_once('=').context("Field=value")?;
                        let tag = [1u8, 12]
                            .into_iter()
                            .filter_map(wbxml::cp::page)
                            .find_map(|page| {
                                let (token, _) = page.tags.iter().find(|(_, n)| *n == name)?;
                                Some(wbxml::Tag {
                                    page: page.number,
                                    token: *token,
                                })
                            })
                            .with_context(|| format!("no contacts field {name}"))?;
                        data.push(text(tag, value));
                    }
                    Element::new(airsync::Change)
                        .child(text(airsync::ServerId, arg(2, "server-id")?))
                        .child(data)
                }
                _ => {
                    let start = arg(3, "start")?;
                    let minutes: i64 = arg(4, "minutes")?.parse().context("minutes")?;
                    let begin = chrono::NaiveDateTime::parse_from_str(&start, "%Y%m%dT%H%M%SZ")
                        .context("start must look like 20261105T090000Z")?;
                    let end = (begin + chrono::Duration::minutes(minutes))
                        .format("%Y%m%dT%H%M%SZ")
                        .to_string();
                    let mut data = Element::new(airsync::ApplicationData)
                        .child(text(cal::Subject, arg(2, "subject")?))
                        .child(text(cal::StartTime, start))
                        .child(text(cal::EndTime, end))
                        .child(text(cal::AllDayEvent, "0"))
                        .child(text(cal::BusyStatus, "2"));
                    if let Some(attendee) = args.get(5) {
                        data.push(text(cal::MeetingStatus, "1"));
                        data.push(
                            Element::new(cal::Attendees).child(
                                Element::new(cal::Attendee)
                                    .child(text(cal::Email, attendee.clone()))
                                    .child(text(cal::Name, attendee.clone()))
                                    .child(text(cal::AttendeeType, "1")),
                            ),
                        );
                    }
                    Element::new(airsync::Add)
                        .child(text(airsync::ClientId, unique_id()))
                        .child(data)
                }
            };
            let c = Element::new(airsync::Collection)
                .child(text(airsync::SyncKey, key))
                .child(text(airsync::CollectionId, collection))
                .child(text(airsync::GetChanges, "0"))
                .child(Element::new(airsync::Commands).child(command));
            client
                .send(
                    "Sync",
                    Some(
                        Element::new(airsync::Sync)
                            .child(Element::new(airsync::Collections).child(c)),
                    ),
                )
                .await
        }
        "folder-create" | "folder-update" | "folder-delete" => {
            let key = text(fh::SyncKey, arg(0, "key")?);
            let (cmd, body) = match command {
                "folder-create" => (
                    "FolderCreate",
                    Element::new(fh::FolderCreate)
                        .child(key)
                        .child(text(fh::ParentId, arg(1, "parent")?))
                        .child(text(fh::DisplayName, arg(2, "name")?))
                        .child(text(
                            fh::Type,
                            args.get(3).cloned().unwrap_or_else(|| "12".to_owned()),
                        )),
                ),
                "folder-update" => (
                    "FolderUpdate",
                    Element::new(fh::FolderUpdate)
                        .child(key)
                        .child(text(fh::ServerId, arg(1, "server-id")?))
                        .child(text(fh::ParentId, arg(2, "parent")?))
                        .child(text(fh::DisplayName, arg(3, "name")?)),
                ),
                _ => (
                    "FolderDelete",
                    Element::new(fh::FolderDelete)
                        .child(key)
                        .child(text(fh::ServerId, arg(1, "server-id")?)),
                ),
            };
            client.send(cmd, Some(body)).await
        }
        "move" => {
            let body = Element::new(mv::MoveItems).child(
                Element::new(mv::Move)
                    .child(text(mv::SrcMsgId, arg(0, "server-id")?))
                    .child(text(mv::SrcFldId, arg(1, "from-folder")?))
                    .child(text(mv::DstFldId, arg(2, "to-folder")?)),
            );
            client.send("MoveItems", Some(body)).await
        }
        "ping" => {
            let heartbeat = arg(0, "heartbeat")?;
            let mut folders = Element::new(ping::Folders);
            for folder in args.iter().skip(1) {
                let (id, class) = folder.split_once(':').unwrap_or((folder, "Email"));
                folders.push(
                    Element::new(ping::Folder)
                        .child(text(ping::Id, id))
                        .child(text(ping::Class, class)),
                );
            }
            let body = Element::new(ping::Ping)
                .child(text(ping::HeartbeatInterval, heartbeat))
                .child(folders);
            client.send("Ping", Some(body)).await
        }
        "sendmail" | "smartreply" => {
            let (to, subject, message, source) = if command == "sendmail" {
                (
                    arg(0, "to")?,
                    arg(1, "subject")?,
                    args.get(2)
                        .cloned()
                        .unwrap_or_else(|| "Sent by eas-client".to_owned()),
                    None,
                )
            } else {
                (
                    arg(2, "to")?,
                    "Re: eas-client".to_owned(),
                    arg(3, "body")?,
                    Some((arg(0, "collection")?, arg(1, "server-id")?)),
                )
            };
            let mime = format!(
                "From: {}\r\nTo: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\r\n{message}\r\n",
                client.eas.user
            );
            let mut body = Element::new(if source.is_some() {
                cm::SmartReply
            } else {
                cm::SendMail
            })
            .child(text(cm::ClientId, unique_id()));
            if let Some((collection, server_id)) = source {
                body.push(
                    Element::new(cm::Source)
                        .child(text(cm::FolderId, collection))
                        .child(text(cm::ItemId, server_id)),
                );
            }
            body.push(Element::new(cm::SaveInSentItems));
            body.push(Element::with_opaque(cm::Mime, mime.into_bytes()));
            let cmd = if command == "sendmail" {
                "SendMail"
            } else {
                "SmartReply"
            };
            client.send(cmd, Some(body)).await
        }
        "search-gal" => {
            let body = Element::new(sp::Search).child(
                Element::new(sp::Store)
                    .child(text(sp::Name, "GAL"))
                    .child(text(sp::Query, arg(0, "query")?))
                    .child(Element::new(sp::Options).child(text(sp::Range, "0-9"))),
            );
            client.send("Search", Some(body)).await
        }
        other => bail!("unknown command {other}"),
    }
}

fn unique_id() -> String {
    format!(
        "eas-client-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

/// Whether `tag` is called `name` (`Name` or `Namespace:Name`).
fn tag_is(tag: wbxml::Tag, name: &str) -> bool {
    match name.split_once(':') {
        Some((namespace, local)) => tag.namespace() == Some(namespace) && tag.name() == Some(local),
        None => tag.name() == Some(name),
    }
}

/// `Tag=value` (equal) or `Tag~value` (contains): the tag and a test.
fn matcher(spec: &str) -> anyhow::Result<(&str, impl Fn(&str) -> bool + '_)> {
    let at = spec
        .find(['=', '~'])
        .context("expected <Tag>=<value> or <Tag>~<value>")?;
    let (name, value) = (&spec[..at], &spec[at + 1..]);
    let contains = spec.as_bytes()[at] == b'~';
    Ok((name, move |text: &str| {
        if contains {
            text.contains(value)
        } else {
            text == value
        }
    }))
}

/// Every element in the tree, depth first.
fn all_elements(root: &Element) -> Vec<&Element> {
    let mut out = vec![root];
    for child in root.elements() {
        out.extend(all_elements(child));
    }
    out
}

/// Splits a scenario line into words; double quotes group words.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            c => {
                current.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(current);
    }
    out
}

/// Checks a step's response; saves values into `vars`. `save` and `folder`
/// run first, so a retried step still keeps its newest SyncKey. Errors
/// describe the first failed check.
fn check(
    response: Option<&Element>,
    checks: &[Vec<String>],
    vars: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    let is_save =
        |c: &Vec<String>| matches!(c.first().map(String::as_str), Some("save" | "folder"));
    let (saves, rest): (Vec<&Vec<String>>, Vec<&Vec<String>>) =
        checks.iter().partition(|c| is_save(c));
    let mut elements = response.map(all_elements).unwrap_or_default();
    for check in saves.into_iter().chain(rest) {
        let texts = |name: &str| -> Vec<&str> {
            elements
                .iter()
                .filter(|e| tag_is(e.tag, name))
                .filter_map(|e| e.text())
                .collect()
        };
        match check
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["within", spec] => {
                let (name, test) = matcher(spec)?;
                let item = elements
                    .iter()
                    .filter(|e| tag_is(e.tag, "Add") || tag_is(e.tag, "Change"))
                    .find(|item| {
                        all_elements(item)
                            .iter()
                            .any(|e| tag_is(e.tag, name) && e.text().is_some_and(&test))
                    })
                    .with_context(|| format!("no item with {spec}"))?;
                elements = all_elements(item);
            }
            ["expect", spec] => {
                let (name, test) = matcher(spec)?;
                if !texts(name).into_iter().any(&test) {
                    bail!("no {spec} (found {:?})", texts(name));
                }
            }
            ["count", spec] => {
                let (name, n) = spec.split_once('=').context("count <Tag>=<n>")?;
                let found = elements.iter().filter(|e| tag_is(e.tag, name)).count();
                if found.to_string() != n {
                    bail!("{found} {name} elements, expected {n}");
                }
            }
            ["save", name, var] => {
                let value = texts(name)
                    .first()
                    .map(|v| (*v).to_owned())
                    .with_context(|| format!("no {name} to save"))?;
                vars.insert((*var).to_owned(), value);
            }
            ["folder", folder_type, var] => {
                let id = elements
                    .iter()
                    .filter(|e| tag_is(e.tag, "Add"))
                    .find(|add| add.child_text(fh::Type) == Some(*folder_type))
                    .and_then(|add| add.child_text(fh::ServerId))
                    .with_context(|| format!("no folder of type {folder_type}"))?;
                vars.insert((*var).to_owned(), id.to_owned());
            }
            ["item", name, var] => {
                let id = elements
                    .iter()
                    .filter(|e| tag_is(e.tag, "Add") || tag_is(e.tag, "Change"))
                    .find(|item| all_elements(item).iter().any(|e| tag_is(e.tag, name)))
                    .and_then(|item| item.child_text(airsync::ServerId))
                    .with_context(|| format!("no item containing {name}"))?;
                vars.insert((*var).to_owned(), id.to_owned());
            }
            ["retry", _] => {}
            other => bail!("unknown check {other:?}"),
        }
    }
    Ok(())
}

/// Runs a scenario file (see the module docs); stops at the first failure.
async fn scenario(first: Client, path: &str) -> anyhow::Result<()> {
    let script =
        std::fs::read_to_string(shellexpand(path)).with_context(|| format!("read {path}"))?;
    let mut client = first;
    // `$run` is unique per run, for subjects that must not match old runs;
    // `$user1`, `$password1`, `$user2` and `$password2` come from the
    // environment (EAS_USER1, EAS_PASSWORD_FILE1, ...).
    let mut vars: HashMap<String, String> = HashMap::from([("run".to_owned(), unique_id())]);
    for n in [1, 2] {
        for (var, env) in [("user", "EAS_USER"), ("password", "EAS_PASSWORD_FILE")] {
            if let Ok(value) = std::env::var(format!("{env}{n}")) {
                vars.insert(format!("{var}{n}"), value);
            }
        }
    }
    for (n, line) in script.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        // Longest names first, so `$key2` is not read as `$key` + "2".
        let mut names: Vec<(&String, &String)> = vars.iter().collect();
        names.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
        let substitute = |word: String| {
            names.iter().fold(word, |w, (name, value)| {
                w.replace(&format!("${name}"), value)
            })
        };
        let mut parts = line.split('|').map(|part| {
            words(part)
                .into_iter()
                .map(substitute)
                .collect::<Vec<String>>()
        });
        let step = parts.next().unwrap_or_default();
        let checks: Vec<Vec<String>> = parts.collect();
        let Some((command, args)) = step.split_first() else {
            continue;
        };
        if command == "login" {
            let (Some(user), Some(password_file)) = (args.first(), args.get(1)) else {
                bail!("line {}: login <user> <password-file> [device]", n + 1);
            };
            let password = std::fs::read_to_string(shellexpand(password_file))?;
            let device = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| client.eas.device_id.clone());
            let url = client.eas.url.clone();
            client = Client::new(
                &url,
                user,
                password.trim(),
                &device,
                client.state_dir.clone(),
            );
            println!("ok   {line}");
            continue;
        }
        let retry_for = checks
            .iter()
            .find(|c| c.first().map(String::as_str) == Some("retry"))
            .and_then(|c| c.get(1)?.parse::<u64>().ok())
            .map(Duration::from_secs);
        let deadline = std::time::Instant::now() + retry_for.unwrap_or_default();
        loop {
            let response = run(&mut client, command, args).await;
            let outcome = response
                .as_ref()
                .map_err(|e| anyhow::anyhow!("{e:#}"))
                .and_then(|r| check(r.as_ref(), &checks, &mut vars));
            match outcome {
                Ok(()) => {
                    println!("ok   {line}");
                    break;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    if let Ok(Some(response)) = &response {
                        eprint!("{}", wbxml::to_xml(response));
                    }
                    bail!("line {}: {line}\n  {e:#}", n + 1);
                }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let value = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        value
    };
    // `--token-file` holds an OAuth access token instead of a password.
    let token_file = take("--token-file");
    let bearer = token_file.is_some();
    let (Some(url), Some(user), Some(password_file)) = (
        take("--url"),
        take("--user"),
        take("--password-file").or(token_file),
    ) else {
        eprintln!(
            "usage: eas-client --url URL --user USER (--password-file FILE | --token-file FILE) [--device ID] <command> [args]"
        );
        return ExitCode::FAILURE;
    };
    let device = take("--device").unwrap_or_else(|| "easclient1".to_owned());
    let password_file = shellexpand(&password_file);
    let password = match std::fs::read_to_string(&password_file) {
        Ok(p) => p.trim().to_owned(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", password_file.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(command) = args.first().cloned() else {
        eprintln!("missing command");
        return ExitCode::FAILURE;
    };
    let state_dir = std::env::var_os("EAS_CLIENT_STATE")
        .map_or_else(|| PathBuf::from(".eas-client"), PathBuf::from);
    let mut client = Client::new(&url, &user, &password, &device, state_dir);
    client.eas.bearer = bearer;
    if command == "scenario" {
        let Some(path) = args.get(1) else {
            eprintln!("missing scenario file");
            return ExitCode::FAILURE;
        };
        return match scenario(client, path).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("FAIL {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    match run(&mut client, &command, &args[1..]).await {
        Ok(Some(response)) => {
            print!("{}", wbxml::to_xml(&response));
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("(empty response)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn shellexpand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}
