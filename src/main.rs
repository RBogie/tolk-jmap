//! The `tolk` binary: the server, plus `tolk devices ...` for device
//! management, `tolk healthcheck` for container health checks and
//! `tolk --version`.

use std::{net::SocketAddr, time::Duration};

use tolk::config;
use tracing_subscriber::EnvFilter;

/// `0.1.0 (abc1234)`: the crate version and, in images, the git commit.
fn version() -> String {
    match option_env!("GIT_SHA") {
        Some(sha) if !sha.is_empty() => format!("{} ({sha})", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_owned(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("tolk {}", version());
            return Ok(());
        }
        // For the container HEALTHCHECK; the image has no curl.
        Some("healthcheck") => std::process::exit(healthcheck().await),
        Some("devices") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            if let Err(e) = devices(&args) {
                eprintln!("tolk devices: {e:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tolk=debug"));
    // LOG_FORMAT=json: one JSON object per line, with the request's fields.
    if std::env::var("LOG_FORMAT").is_ok_and(|f| f.eq_ignore_ascii_case("json")) {
        tracing_subscriber::fmt()
            .json()
            .with_current_span(true)
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
            .init();
    }

    tracing::info!(version = version(), "starting");
    let config = config::Config::from_env()?;
    tracing::info!(
        versions = config.eas_versions.join(","),
        public_url = config.public_url.as_deref().unwrap_or("(request host)"),
        tuning = ?config.tuning,
        "configuration"
    );
    let shutdown_deadline = config.tuning.shutdown_deadline;
    let (app, state) = tolk::build_app(&config).await?;
    tolk::maintenance::spawn(
        state.clone(),
        tolk::maintenance::Backups {
            dir: config.backup_dir.clone(),
            keep: config.backup_keep,
        },
    );

    if let Some(addr) = config.metrics_addr {
        let metrics = tolk::metrics_app(state.clone(), &config.tuning);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "serving metrics");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, metrics).await {
                tracing::warn!(error = %e, "metrics server stopped");
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, jmap = %config.jmap_url, "listening");

    let (stopping_tx, stopping) = tokio::sync::watch::channel(false);
    let mut stopping_wait = stopping.clone();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stopping_wait.wait_for(|s| *s).await;
    });
    tokio::spawn(async move {
        stop_signal().await;
        tracing::info!("shutting down: answering pending Pings");
        // Pings would otherwise hold the graceful shutdown for minutes.
        state.push.shut_down();
        let _ = stopping_tx.send(true);
    });
    let mut deadline = stopping.clone();
    tokio::select! {
        result = server => result?,
        _ = async {
            let _ = deadline.wait_for(|s| *s).await;
            tokio::time::sleep(shutdown_deadline).await;
        } => tracing::warn!("in-flight requests did not finish in time"),
    }
    Ok(())
}

/// `tolk devices list|show|block|unblock|wipe [--full]|forget|resync
/// [<collection>]|clear-quarantine [<user> <device>]` on the state database
/// (`STATE_DB`), next to the running service.
fn devices(args: &[String]) -> anyhow::Result<()> {
    use tolk::store::Store;
    let path = std::env::var("STATE_DB").unwrap_or_else(|_| "state.sqlite".to_owned());
    let store = Store::open(std::path::Path::new(&path))?;
    let rows = store.list_devices()?;
    let usage = "usage: tolk devices list \
                 | show|block|unblock|forget|clear-quarantine <user> <device> \
                 | wipe <user> <device> [--full] | resync <user> <device> [<collection>]";
    let Some(action) = args.first() else {
        anyhow::bail!(usage);
    };
    if action == "list" {
        println!(
            "{:<28} {:<24} {:<10} {:<20} {:<5} {:<12} LAST SEEN",
            "USER", "DEVICE", "TYPE", "MODEL", "EAS", "STATUS"
        );
        for d in &rows {
            let last = time(d.last_seen);
            println!(
                "{:<28} {:<24} {:<10} {:<20} {:<5} {:<12} {last}",
                d.login.as_deref().unwrap_or(&d.user),
                d.device_id,
                d.device_type.as_deref().unwrap_or("-"),
                d.model.as_deref().unwrap_or("-"),
                d.version
                    .map(|v| format!("{}.{}", v / 10, v % 10))
                    .unwrap_or_else(|| "-".to_owned()),
                d.status,
            );
        }
        return Ok(());
    }
    let (Some(who), Some(device_id)) = (args.get(1), args.get(2)) else {
        anyhow::bail!(usage);
    };
    // The user may be given as the login or as the JMAP account id.
    let device = rows
        .iter()
        .find(|d| {
            d.device_id == *device_id
                && (d.user == *who
                    || d.login
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(who)))
        })
        .ok_or_else(|| anyhow::anyhow!("no device {device_id} for {who}"))?;
    let full = args.iter().any(|a| a == "--full");
    let status = match action.as_str() {
        "show" => return show_device(&store, device),
        "resync" => {
            match args.get(3) {
                Some(collection) => {
                    anyhow::ensure!(
                        store.resync_collection(&device.user, device_id, collection)?,
                        "{device_id} has no sync state for collection {collection}"
                    );
                    println!(
                        "{device_id}: collection {collection} starts over at its next Sync \
                         (invalid SyncKey)"
                    );
                }
                None => {
                    store.resync_device(&device.user, device_id)?;
                    println!(
                        "{device_id}: folders and collections start over at its next request \
                         (invalid SyncKey)"
                    );
                }
            }
            return Ok(());
        }
        "clear-quarantine" => {
            let cleared = store.clear_quarantine(&device.user, device_id)?;
            println!("{device_id}: {cleared} quarantined items are sent again");
            return Ok(());
        }
        "block" => "blocked",
        "unblock" => "allowed",
        "wipe" if full => "wipe",
        "wipe" => {
            anyhow::ensure!(
                device.version.is_some_and(|v| v >= 161),
                "{device_id} speaks EAS {}; only 16.1 can wipe just the account. \
                 Use `block`, or `wipe --full` to erase the whole device.",
                device
                    .version
                    .map(|v| format!("{}.{}", v / 10, v % 10))
                    .unwrap_or_else(|| "?".to_owned())
            );
            "account-wipe"
        }
        "forget" => {
            store.forget_device(&device.user, device_id)?;
            println!("forgot {device_id}; it starts over if it connects again");
            return Ok(());
        }
        _ => anyhow::bail!(usage),
    };
    store.set_device_status(&device.user, device_id, status)?;
    println!("{device_id}: {status}");
    Ok(())
}

fn time(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// `devices show`: a device's sync state, Ping parameters, quarantined
/// items and last error.
fn show_device(store: &tolk::store::Store, d: &tolk::store::DeviceRow) -> anyhow::Result<()> {
    let version = d
        .version
        .map(|v| format!("{}.{}", v / 10, v % 10))
        .unwrap_or_else(|| "-".to_owned());
    println!("{:<12} {}", "DEVICE", d.device_id);
    println!(
        "{:<12} {} (account {})",
        "USER",
        d.login.as_deref().unwrap_or(&d.user),
        d.user
    );
    println!(
        "{:<12} {} / {}",
        "TYPE",
        d.device_type.as_deref().unwrap_or("-"),
        d.model.as_deref().unwrap_or("-")
    );
    println!("{:<12} {version}, {}", "EAS", d.status);
    println!(
        "{:<12} first {}, last {}",
        "SEEN",
        time(d.first_seen),
        time(d.last_seen)
    );
    let hierarchy = store.folder_sync_state(&d.user, &d.device_id)?;
    println!(
        "{:<12} {}",
        "FOLDERSYNC",
        hierarchy.map_or("not synced".to_owned(), |h| format!(
            "SyncKey {}",
            h.sync_key
        ))
    );
    let ping = store
        .ping_params(&d.user, &d.device_id)?
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    println!(
        "{:<12} {}",
        "PING",
        ping.map_or("-".to_owned(), |p| format!(
            "heartbeat {} s, folders {}",
            p["heartbeat"],
            p["folders"]
                .as_array()
                .map(|f| f
                    .iter()
                    .filter_map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default()
        ))
    );
    println!(
        "{:<12} {}",
        "LAST ERROR",
        d.last_error
            .as_ref()
            .map_or("-".to_owned(), |(e, at)| format!("{} {e}", time(*at)))
    );

    println!();
    println!(
        "{:<11} {:<36} {:<8} {:<7} {:<17} OPTIONS",
        "COLLECTION", "FOLDER", "SYNCKEY", "ITEMS", "LAST SYNC"
    );
    for c in store.collection_summaries(&d.user, &d.device_id)? {
        let folder = match c.collection_id.parse() {
            Ok(id) => store.mailbox_for_folder(&d.user, id)?,
            Err(_) => None,
        };
        let options = c
            .options
            .as_deref()
            .and_then(|o| serde_json::from_str::<serde_json::Value>(o).ok())
            .map(|o| {
                let mut parts = vec![format!("filter {}", o["filter_type"])];
                let bodies: Vec<String> = o["body_preferences"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|b| b["body_type"].to_string())
                    .collect();
                if !bodies.is_empty() {
                    parts.push(format!("body {}", bodies.join(",")));
                }
                if let Some(supported) = o["supported"].as_array() {
                    parts.push(format!("supported {}", supported.len()));
                }
                parts.join(", ")
            })
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{:<11} {:<36} {:<8} {:<7} {:<17} {options}",
            c.collection_id,
            folder.as_deref().unwrap_or("(gone)"),
            c.sync_key,
            c.items,
            c.updated_at.map(time).unwrap_or_else(|| "-".to_owned()),
        );
    }

    let quarantined = store.quarantine_list(&d.user, &d.device_id)?;
    println!();
    if quarantined.is_empty() {
        println!("No quarantined items.");
    } else {
        println!("{:<11} {:<36} SINCE", "COLLECTION", "QUARANTINED ITEM");
        for q in quarantined {
            println!(
                "{:<11} {:<36} {}",
                q.collection_id,
                q.object_id,
                time(q.created_at)
            );
        }
    }
    Ok(())
}

/// Exit code 0 when this host's server answers `/healthz`.
async fn healthcheck() -> i32 {
    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let port = addr.rsplit(':').next().unwrap_or("8080");
    let url = format!("http://127.0.0.1:{port}/healthz");
    match tolk::http_client()
        .build()
        .expect("HTTP client")
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => 0,
        _ => 1,
    }
}

async fn stop_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}
