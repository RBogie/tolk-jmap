//! Test harness: Tolk's real router in front of [`fake_jmap::FakeJmap`],
//! driven by a small EAS client.

#![allow(dead_code)]

pub mod fake_jmap;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use fake_jmap::FakeJmap;
use tolk::{
    AppState,
    client::EasClient,
    config::Config,
    wbxml::{
        Element, Tag,
        cp::{airsync, airsyncbase, folder_hierarchy as fh, ping},
    },
};

pub struct Harness {
    pub jmap: FakeJmap,
    pub url: String,
    pub state: Arc<AppState>,
    pub dir: tempfile::TempDir,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_capturing(None).await
    }

    /// With `traffic_dir`, the test user's traffic is captured there.
    pub async fn start_capturing(traffic_dir: Option<&std::path::Path>) -> Self {
        Self::start_with(traffic_dir, None, Default::default(), None).await
    }

    /// On a copy of an existing state database (e.g. a restored backup).
    pub async fn start_on(db: &std::path::Path) -> Self {
        Self::start_with(None, Some(db), Default::default(), None).await
    }

    /// With these limits.
    pub async fn start_tuned(tuning: tolk::config::Tuning) -> Self {
        Self::start_with(None, None, tuning, None).await
    }

    /// With a second server for the test user's mail domain
    /// (`JMAP_SERVERS`): logins with the domain go there, the short login
    /// name to `jmap`.
    pub async fn start_mapped() -> (Self, FakeJmap) {
        let mapped = FakeJmap::start().await;
        let domain = fake_jmap::USER.rsplit_once('@').unwrap().1.to_owned();
        let h = Self::start_with(
            None,
            None,
            Default::default(),
            Some((domain, mapped.url.clone())),
        )
        .await;
        (h, mapped)
    }

    async fn start_with(
        traffic_dir: Option<&std::path::Path>,
        db: Option<&std::path::Path>,
        tuning: tolk::config::Tuning,
        mapped: Option<(String, String)>,
    ) -> Self {
        // Warnings and errors show up in the output of failing tests.
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "tolk=warn".into()),
            )
            .try_init();
        let jmap = FakeJmap::start().await;
        let dir = tempfile::tempdir().unwrap();
        if let Some(db) = db {
            std::fs::copy(db, dir.path().join("state.sqlite")).unwrap();
        }
        let config = Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            jmap_url: jmap.url.clone(),
            jmap_servers: mapped.into_iter().collect(),
            state_db: dir.path().join("state.sqlite"),
            traffic_dir: traffic_dir.map(Into::into),
            traffic_capture: vec![fake_jmap::USER.to_owned()],
            traffic_retention: Duration::from_secs(3600),
            auth_cache_ttl: Duration::from_secs(300),
            trusted_proxies: Vec::new(),
            public_url: None,
            eas_versions: tolk::config::SUPPORTED_EAS_VERSIONS
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
            tuning,
            metrics_addr: None,
            backup_dir: dir.path().join("backups"),
            backup_keep: 7,
        };
        let (app, state) = tolk::build_app(&config).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            jmap,
            url,
            state,
            dir,
        }
    }

    pub fn device(&self, id: &str) -> Device {
        let mut client =
            EasClient::new(&self.url, fake_jmap::USER, fake_jmap::PASSWORD, id, "Test");
        client.timeout = Duration::from_secs(30);
        Device(client)
    }

    /// A provisioned device after its first FolderSync.
    pub async fn ready_device(&self, id: &str) -> (Device, Folders) {
        let mut device = self.device(id);
        device.provision().await;
        let folders = device.folder_sync().await;
        (device, folders)
    }
}

pub fn text(tag: Tag, value: impl Into<String>) -> Element {
    Element::with_text(tag, value)
}

pub use tolk::client::Reply;

/// A test device: the shared EAS client, with test helpers that panic on
/// transport errors.
pub struct Device(pub EasClient);

impl std::ops::Deref for Device {
    type Target = EasClient;
    fn deref(&self) -> &EasClient {
        &self.0
    }
}

impl std::ops::DerefMut for Device {
    fn deref_mut(&mut self) -> &mut EasClient {
        &mut self.0
    }
}

/// Folder ids by display name, and the FolderSync key.
pub struct Folders {
    pub key: String,
    pub by_name: Vec<(String, String, String)>,
}

impl Folders {
    /// The server id of the folder named `name`.
    pub fn id(&self, name: &str) -> String {
        self.by_name
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, id, _)| id.clone())
            .unwrap_or_else(|| panic!("no folder {name}: {:?}", self.by_name))
    }

    /// The server id of the first folder of an EAS folder type.
    pub fn of_type(&self, folder_type: &str) -> String {
        self.by_name
            .iter()
            .find(|(_, _, t)| t == folder_type)
            .map(|(_, id, _)| id.clone())
            .unwrap_or_else(|| panic!("no folder of type {folder_type}"))
    }
}

/// One collection of a Sync response.
#[derive(Debug, Default)]
pub struct SyncResult {
    pub status: String,
    pub key: String,
    pub adds: Vec<Element>,
    pub changes: Vec<Element>,
    pub deletes: Vec<String>,
    pub more: bool,
    /// Replies to client commands: (kind, ClientId or ServerId, status, ServerId).
    pub responses: Vec<(String, String, String, String)>,
    /// The same replies as elements.
    pub response_elements: Vec<Element>,
}

impl SyncResult {
    pub fn add_ids(&self) -> Vec<String> {
        self.adds
            .iter()
            .filter_map(|a| a.child_text(airsync::ServerId).map(str::to_owned))
            .collect()
    }
}

impl Device {
    pub async fn send(&self, cmd: &str, body: Option<Element>) -> Reply {
        self.send_as(cmd, body, &self.0.password).await
    }

    pub async fn send_as(&self, cmd: &str, body: Option<Element>, password: &str) -> Reply {
        self.send_full(cmd, &[], body, password).await
    }

    /// A request with extra query parameters (e.g. `SaveInSent`).
    pub async fn send_full(
        &self,
        cmd: &str,
        params: &[(String, String)],
        body: Option<Element>,
        password: &str,
    ) -> Reply {
        self.0
            .send_full(cmd, params, body, password)
            .await
            .expect("request")
    }

    pub async fn provision(&mut self) {
        self.0.provision().await.expect("provisioning");
    }

    pub async fn folder_sync(&self) -> Folders {
        self.folder_sync_from("0").await
    }

    pub async fn folder_sync_from(&self, key: &str) -> Folders {
        let reply = self
            .send(
                "FolderSync",
                Some(Element::new(fh::FolderSync).child(text(fh::SyncKey, key))),
            )
            .await;
        let root = reply.root();
        assert_eq!(root.child_text(fh::Status), Some("1"), "FolderSync status");
        let by_name = root
            .find(fh::Changes)
            .into_iter()
            .flat_map(|c| c.find_all(fh::Add))
            .map(|add| {
                (
                    add.child_text(fh::DisplayName)
                        .unwrap_or_default()
                        .to_owned(),
                    add.child_text(fh::ServerId).unwrap_or_default().to_owned(),
                    add.child_text(fh::Type).unwrap_or_default().to_owned(),
                )
            })
            .collect();
        Folders {
            key: root.child_text(fh::SyncKey).unwrap_or_default().to_owned(),
            by_name,
        }
    }

    /// Sync of one collection; `commands` are client changes to send.
    pub async fn sync(&self, collection: &str, key: &str, commands: Option<Element>) -> SyncResult {
        self.sync_window(collection, key, commands, 100).await
    }

    pub async fn sync_window(
        &self,
        collection: &str,
        key: &str,
        commands: Option<Element>,
        window: usize,
    ) -> SyncResult {
        let mut c = Element::new(airsync::Collection)
            .child(text(airsync::SyncKey, key))
            .child(text(airsync::CollectionId, collection));
        if key != "0" {
            c.push(Element::new(airsync::GetChanges));
            c.push(text(airsync::WindowSize, window.to_string()));
            c.push(
                Element::new(airsync::Options).child(
                    Element::new(airsyncbase::BodyPreference)
                        .child(text(airsyncbase::Type, "1"))
                        .child(text(airsyncbase::TruncationSize, "500")),
                ),
            );
        }
        if let Some(commands) = commands {
            c.push(commands);
        }
        let reply = self
            .send(
                "Sync",
                Some(
                    Element::new(airsync::Sync).child(Element::new(airsync::Collections).child(c)),
                ),
            )
            .await;
        assert!(reply.status.is_success(), "Sync HTTP {}", reply.status);
        let root = reply.root();
        if let Some(status) = root.child_text(airsync::Status) {
            return SyncResult {
                status: status.to_owned(),
                ..Default::default()
            };
        }
        let collection = root
            .find(airsync::Collections)
            .and_then(|c| c.find(airsync::Collection))
            .expect("Sync collection");
        let mut result = SyncResult {
            status: collection
                .child_text(airsync::Status)
                .unwrap_or_default()
                .to_owned(),
            key: collection
                .child_text(airsync::SyncKey)
                .unwrap_or_default()
                .to_owned(),
            more: collection.find(airsync::MoreAvailable).is_some(),
            ..Default::default()
        };
        if let Some(commands) = collection.find(airsync::Commands) {
            for command in commands.elements() {
                match command.tag {
                    t if t == airsync::Add => result.adds.push(command.clone()),
                    t if t == airsync::Change => result.changes.push(command.clone()),
                    t if t == airsync::Delete || t == airsync::SoftDelete => result.deletes.push(
                        command
                            .child_text(airsync::ServerId)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    _ => {}
                }
            }
        }
        if let Some(responses) = collection.find(airsync::Responses) {
            for response in responses.elements() {
                result.response_elements.push(response.clone());
                result.responses.push((
                    response.tag.name().unwrap_or_default().to_owned(),
                    response
                        .child_text(airsync::ClientId)
                        .or_else(|| response.child_text(airsync::ServerId))
                        .unwrap_or_default()
                        .to_owned(),
                    response
                        .child_text(airsync::Status)
                        .unwrap_or_default()
                        .to_owned(),
                    response
                        .child_text(airsync::ServerId)
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
        }
        result
    }

    /// Primes a collection and fetches everything; returns the last result
    /// with all Adds collected.
    pub async fn initial_sync(&self, collection: &str) -> SyncResult {
        let primed = self.sync(collection, "0", None).await;
        assert_eq!(primed.status, "1", "priming Sync of {collection}");
        let mut key = primed.key;
        let mut adds = Vec::new();
        loop {
            let mut result = self.sync(collection, &key, None).await;
            assert_eq!(result.status, "1", "Sync of {collection}");
            adds.append(&mut result.adds);
            key = result.key.clone();
            if !result.more {
                result.adds = adds;
                return result;
            }
        }
    }

    /// Ping on the given folders; returns the status and changed folder ids.
    pub async fn ping(&self, heartbeat: u32, folders: &[(&str, &str)]) -> (String, Vec<String>) {
        let mut list = Element::new(ping::Folders);
        for (id, class) in folders {
            list.push(
                Element::new(ping::Folder)
                    .child(text(ping::Id, *id))
                    .child(text(ping::Class, *class)),
            );
        }
        let reply = self
            .send(
                "Ping",
                Some(
                    Element::new(ping::Ping)
                        .child(text(ping::HeartbeatInterval, heartbeat.to_string()))
                        .child(list),
                ),
            )
            .await;
        let root = reply.root();
        let changed = root
            .find(ping::Folders)
            .into_iter()
            .flat_map(|f| f.find_all(ping::Folder))
            .filter_map(|f| f.text().map(str::to_owned))
            .collect();
        (
            root.child_text(ping::Status).unwrap_or_default().to_owned(),
            changed,
        )
    }
}
