//! Prometheus metrics, served on their own address (`METRICS_ADDR`) so
//! they are not reachable through the public EAS route.

use crate::LockExt;
use std::{
    collections::BTreeMap,
    fmt::Write,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::AppState;

#[derive(Default)]
pub struct Metrics {
    /// (command, HTTP status) to (count, total seconds).
    requests: Mutex<BTreeMap<(String, u16), (u64, f64)>>,
    login_failures: AtomicU64,
}

impl Metrics {
    pub fn record_request(&self, cmd: &str, status: u16, took: Duration) {
        // Commands come from the request line: only known names are kept.
        let cmd = if cmd.len() <= 32 && cmd.chars().all(|c| c.is_ascii_alphabetic()) {
            cmd
        } else {
            "other"
        };
        let mut requests = self.requests.locked();
        let entry = requests.entry((cmd.to_owned(), status)).or_default();
        entry.0 += 1;
        entry.1 += took.as_secs_f64();
    }

    pub fn record_login_failure(&self) {
        self.login_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// The metrics in Prometheus text format.
pub fn render(state: &AppState, max_pings: usize, max_commands: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# TYPE tolk_build_info gauge");
    let _ = writeln!(
        out,
        "tolk_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    let requests = state.metrics.requests.locked();
    let _ = writeln!(out, "# TYPE tolk_requests_total counter");
    for ((cmd, status), (count, _)) in requests.iter() {
        let _ = writeln!(
            out,
            "tolk_requests_total{{cmd=\"{cmd}\",status=\"{status}\"}} {count}"
        );
    }
    let _ = writeln!(out, "# TYPE tolk_request_seconds_total counter");
    for ((cmd, status), (_, seconds)) in requests.iter() {
        let _ = writeln!(
            out,
            "tolk_request_seconds_total{{cmd=\"{cmd}\",status=\"{status}\"}} {seconds:.3}"
        );
    }
    drop(requests);
    let _ = writeln!(out, "# TYPE tolk_login_failures_total counter");
    let _ = writeln!(
        out,
        "tolk_login_failures_total {}",
        state.metrics.login_failures.load(Ordering::Relaxed)
    );
    let _ = writeln!(out, "# TYPE tolk_pings_waiting gauge");
    let _ = writeln!(
        out,
        "tolk_pings_waiting {}",
        max_pings - state.concurrency.pings.available_permits()
    );
    let _ = writeln!(out, "# TYPE tolk_commands_running gauge");
    let _ = writeln!(
        out,
        "tolk_commands_running {}",
        max_commands - state.concurrency.commands.available_permits()
    );
    if let Ok(counts) = state.store.row_counts() {
        let _ = writeln!(out, "# TYPE tolk_state_rows gauge");
        for (table, rows) in counts {
            let _ = writeln!(out, "tolk_state_rows{{table=\"{table}\"}} {rows}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_command_labels_tame() {
        let metrics = Metrics::default();
        metrics.record_request("Sync", 200, Duration::from_millis(30));
        metrics.record_request("Sync", 200, Duration::from_millis(10));
        metrics.record_request("x\"}evil", 400, Duration::ZERO);
        let requests = metrics.requests.locked();
        assert_eq!(requests[&("Sync".to_owned(), 200)].0, 2);
        assert!(requests.contains_key(&("other".to_owned(), 400)));
    }
}
