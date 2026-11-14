//! The command line interface.
//!
//! The definition lives in the library so the completion generator and the
//! binary share one source of truth.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "icloudctl",
    version,
    about = "Mount iCloud Drive on Linux as a local-first FUSE filesystem",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create the local store, the config file and the systemd user unit.
    Init {
        /// Mount point (default: ~/iCloud)
        mount_dir: Option<PathBuf>,
    },
    /// Write config.yaml with the Apple ID credentials.
    Configure {
        /// Apple ID email (prompted if omitted)
        email: Option<String>,
    },
    /// Run the interactive Apple ID / 2FA authentication bootstrap.
    Auth {
        /// Bypass push delivery and force an SMS code.
        #[arg(long)]
        force_sms: bool,
        /// Print Apple's reported auth mode for diagnosis.
        #[arg(long)]
        debug: bool,
        /// Accept Apple's updated iCloud terms of service.
        #[arg(long)]
        accept_terms: bool,
        /// Inject a browser trust token instead of running the 2FA flow.
        #[arg(long)]
        trust_token: Option<String>,
    },
    /// Mount the filesystem (runs in the foreground).
    Mount {
        /// Mount point (default: read from icloud.env)
        mount_dir: Option<PathBuf>,
        /// Config file (default: ~/.config/icloud-for-linux/config.yaml)
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// Keep the process in the foreground (used by the systemd unit).
        #[arg(long)]
        foreground: bool,
    },
    /// Unmount the filesystem.
    Unmount {
        /// Mount point (default: read from icloud.env)
        mount_dir: Option<PathBuf>,
    },
    /// Run one sync pass: drain the journal and refresh stale metadata.
    Sync {
        /// Print what would happen without contacting the remote.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait for a running driver to finish.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },
    /// List the conflicts that were preserved.
    Conflicts,
    /// Write the preserved conflict versions into a directory.
    ExportConflicts {
        /// Directory the versions are written to.
        dest: PathBuf,
    },
    /// Download the content of files into the local store.
    Hydrate {
        /// Report what would be downloaded without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Hydrate this subtree of the drive (e.g. /Downloads).
        #[arg(long)]
        path: Option<String>,
    },
    /// Materialise a subtree of the drive as plain files in a directory.
    Export {
        /// iCloud path to export (e.g. /Downloads)
        path: String,
        /// Destination directory
        dest: PathBuf,
    },
    /// Re-hash every blob and report corruption.
    Verify,
    /// Remove blobs that nothing references any more.
    Gc,
    /// Show what the local index knows.
    Index,
    /// Show whether the service is running.
    Status,
    /// Show service logs.
    Logs,
    /// Check the local setup.
    Doctor,
    /// Remove the local store and rebuild it on the next start.
    ClearCache,
}
