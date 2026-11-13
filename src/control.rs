//! The control loop of a running driver.
//!
//! While the filesystem is mounted, one thread keeps the local state in step
//! with the remote:
//!
//! * `SIGUSR1` triggers a single pass (this is what `icloudctl sync` uses)
//! * with `auto_sync` the journal is drained and stale metadata refreshed on
//!   the configured schedule
//! * with `hydration: background` the eligible files are downloaded ahead of
//!   time by a small pool of workers
//!
//! The workers are deliberately few: iCloud throttles aggressive parallel
//! downloads, and a throttled account is a locked account.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::config::Config;
use crate::signal;
use crate::sync::SyncEngine;

/// How often the control loop wakes up.
const TICK: Duration = Duration::from_millis(500);

pub struct Control {
    engine: Arc<SyncEngine>,
    config: Config,
    marker: PathBuf,
}

impl Control {
    pub fn new(engine: Arc<SyncEngine>, config: Config, marker: PathBuf) -> Self {
        Self {
            engine,
            config,
            marker,
        }
    }

    /// Start the control loop and the hydration workers.
    pub fn spawn(self) {
        signal::install_sync_handler();

        if self.config.hydration == "background" {
            let workers = self.config.hydration_workers.max(1);
            for worker in 0..workers {
                let engine = self.engine.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("icloud-hydrate-{worker}"))
                    .spawn(move || hydration_worker(engine));
            }
            info!("background hydration started with {workers} worker(s)");
        }

        let engine = self.engine.clone();
        let config = self.config.clone();
        let marker = self.marker.clone();
        let _ = std::thread::Builder::new()
            .name("icloud-control".to_string())
            .spawn(move || loop {
                std::thread::sleep(TICK);

                // A one-shot request wins over the schedule.
                if signal::take_sync_request() {
                    let report = engine.sync_once();
                    write_marker(&marker);
                    match report {
                        Ok(report) => info!(
                            drained = report.drained,
                            refreshed = report.refreshed,
                            "sync requested over SIGUSR1"
                        ),
                        Err(e) => warn!(error = %e, "the requested sync failed"),
                    }
                    continue;
                }

                if !config.auto_sync {
                    continue;
                }
                let mut state = schedule_state();
                if state.elapsed >= config.upload_interval_seconds {
                    state.elapsed = 0;
                    match engine.sync_once() {
                        Ok(report) => {
                            if report.drained > 0 || report.refreshed > 0 {
                                info!(
                                    drained = report.drained,
                                    refreshed = report.refreshed,
                                    "sync pass finished"
                                );
                            }
                        }
                        Err(e) => warn!(error = %e, "sync pass failed"),
                    }
                }
            });
    }
}

/// The state of the automatic schedule.
fn schedule_state() -> ScheduleState {
    ScheduleState {
        started: Instant::now(),
        elapsed: 0,
    }
}

struct ScheduleState {
    started: Instant,
    elapsed: u64,
}

/// One hydration worker: it claims files and downloads them.
fn hydration_worker(engine: Arc<SyncEngine>) {
    loop {
        let targets = match engine.claim_hydration_targets(4) {
            Ok(targets) => targets,
            Err(e) => {
                warn!(error = %e, "cannot list the hydration targets");
                Vec::new()
            }
        };
        if targets.is_empty() {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        for path in targets {
            match engine.hydrate_one(&path) {
                Ok(()) => info!(path = %path, "hydrated"),
                Err(e) => {
                    warn!(path = %path, error = %e, "hydration failed");
                    engine.release_claim(&path);
                }
            }
        }
    }
}

/// Write the completion marker the on-demand sync waits for.
pub fn write_marker(path: &PathBuf) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let _ = std::fs::write(path, format!("{stamp}"));
}

/// Wait for the completion marker of an on-demand sync.
///
/// The marker carries the time of the completed pass, so a stale marker from
/// an earlier run is not mistaken for a fresh one.
pub fn wait_for_marker(path: &PathBuf, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(stamp) = content.trim().parse::<f64>() {
                let age = crate::clock::now_seconds() - stamp;
                if age >= 0.0 && age < timeout.as_secs_f64() + 5.0 {
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "the sync did not complete within {}s",
        timeout.as_secs()
    ))
}
