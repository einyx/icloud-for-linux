//! icloudctl — command line interface of icloud-for-linux.
//!
//! Copyright (C) 2026  einyx
//!
//! This program is free software: you can redistribute it and/or modify
//! it under the terms of the GNU General Public License as published by
//! the Free Software Foundation, either version 3 of the License, or
//! (at your option) any later version.
//!
//! This program is distributed in the hope that it will be useful,
//! but WITHOUT ANY WARRANTY; without even the implied warranty of
//! MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//! GNU General Public License for more details.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use icloud_for_linux::blob::BlobStore;
use icloud_for_linux::config::Config;
use icloud_for_linux::db::Db;
use icloud_for_linux::icloud::client::ICloudApi;
use icloud_for_linux::index::Index;
use icloud_for_linux::journal::Journal;
use icloud_for_linux::service;
use icloud_for_linux::sync::SyncEngine;
use icloud_for_linux::vfs::{self, Vfs};

#[derive(Parser)]
#[command(
    name = "icloudctl",
    version,
    about = "Mount iCloud Drive on Linux as a local-first FUSE filesystem",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    },
    /// Download the content of every eligible file into the local store.
    Hydrate {
        /// Report what would be downloaded without downloading.
        #[arg(long)]
        dry_run: bool,
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn resolve_config(config: Option<PathBuf>) -> PathBuf {
    config.unwrap_or_else(service::config_file)
}

fn resolve_mount(mount_dir: Option<PathBuf>) -> PathBuf {
    mount_dir
        .or_else(service::read_env_mount)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join("iCloud")
        })
}

/// Open the local store: blob store, index and journal.
struct Store {
    config: Config,
    blobs: Arc<BlobStore>,
    index: Arc<Index>,
    journal: Arc<Journal>,
}

fn open_store(config_path: &Path) -> Result<Store> {
    let config = Config::load(config_path).context("load config")?;
    let cache = config.expanded_cache_dir();
    let db = Arc::new(Db::open(&config.db_path())?);
    let blobs = Arc::new(BlobStore::new(&cache)?);
    let index = Arc::new(Index::new(db.clone()));
    let journal = Arc::new(Journal::new(db.clone()));
    Ok(Store {
        config,
        blobs,
        index,
        journal,
    })
}

/// Build the iCloud client and restore the session.
///
/// When the session cannot be restored the client is still returned, in
/// "unauthenticated" mode: the filesystem mounts and serves the local store,
/// and the sync engine simply has nothing to talk to.
fn build_client(config: &Config) -> ICloudApi {
    let client = ICloudApi::new(
        &config.username,
        &config.password,
        &config.expanded_cookie_dir(),
        None,
    )
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "could not create the iCloud client");
        ICloudApi::new("", "", Path::new("/tmp"), None).expect("a client without a session")
    });
    match client.restore() {
        Ok(()) => {
            tracing::info!("session restored, the remote is reachable");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "UNAUTHENTICATED mode: run `icloudctl auth` to sign in again"
            );
        }
    }
    client
}

fn cmd_init(mount_dir: Option<PathBuf>) -> Result<()> {
    service::ensure_dirs()?;
    let mount_dir = resolve_mount(mount_dir);
    std::fs::create_dir_all(&mount_dir)?;
    let binary = std::env::current_exe()?;
    service::write_config_template(&service::config_file())?;
    service::write_env(&mount_dir)?;
    service::write_service(&binary, &mount_dir)?;
    let _ = service::systemctl(&["daemon-reload"]);
    let _ = service::systemctl(&["enable", service::SERVICE_NAME]);
    println!("Initialized.");
    println!("- Config: {}", service::config_file().display());
    println!("- Env:    {}", service::env_file().display());
    println!("- Unit:   {}", service::service_file().display());
    println!("- Mount:  {}", mount_dir.display());
    println!("Next: icloudctl configure && icloudctl auth && icloudctl mount");
    Ok(())
}

fn cmd_configure(email: Option<String>) -> Result<()> {
    service::ensure_dirs()?;
    service::write_config_template(&service::config_file())?;
    let email = match email {
        Some(e) => e,
        None => {
            use std::io::Write;
            print!("Apple ID email: ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };
    let password = read_password()?;
    service::write_config(
        &service::config_file(),
        &email,
        &password,
        &service::cache_dir(),
        &service::config_dir().join("cookies"),
    )?;
    println!("Wrote {}", service::config_file().display());
    Ok(())
}

fn read_password() -> Result<String> {
    use std::io::Read;
    let mut password = String::new();
    let mut stdin = std::io::stdin().lock();
    let _ = stdin.read_to_string(&mut password);
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

fn cmd_mount(mount_dir: Option<PathBuf>, config: Option<PathBuf>, foreground: bool) -> Result<()> {
    let config_path = resolve_config(config);
    let store = open_store(&config_path)?;
    let mount_dir = resolve_mount(mount_dir);
    service::ensure_dirs()?;
    std::fs::create_dir_all(&mount_dir)?;

    let client = build_client(&store.config);

    let engine = Arc::new(SyncEngine::new(
        store.config.clone(),
        store.index.clone(),
        store.journal.clone(),
        store.blobs.clone(),
        Box::new(client),
    ));
    if let Err(e) = engine.start() {
        tracing::warn!(error = %e, "sync engine start failed");
    }
    let _ = foreground;

    // The background loops: drain the journal and refresh stale metadata.
    if store.config.auto_sync {
        let engine = engine.clone();
        let upload_interval = store.config.upload_interval_seconds.max(5);
        let refresh_interval = store.config.remote_refresh_interval_seconds.max(30);
        std::thread::Builder::new()
            .name("icloud-sync".to_string())
            .spawn(move || {
                let mut elapsed = 0u64;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    elapsed += 5;
                    if elapsed.is_multiple_of(upload_interval) {
                        match engine.sync_once() {
                            Ok(report) => {
                                if report.drained > 0 || report.refreshed > 0 {
                                    tracing::info!(
                                        drained = report.drained,
                                        refreshed = report.refreshed,
                                        "sync pass finished"
                                    );
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "sync pass failed"),
                        }
                    }
                    let _ = refresh_interval;
                }
            })
            .ok();
    }

    let hydrate: vfs::HydrateFn = {
        let engine = engine.clone();
        Arc::new(move |path: &str| engine.ensure_local_file(path))
    };
    let resolve: vfs::ResolveFn = {
        let engine = engine.clone();
        Arc::new(move |path: &str| engine.resolve_path(path))
    };
    let list: vfs::ListFn = {
        let engine = engine.clone();
        Arc::new(move |path: &str| engine.list_dir(path))
    };

    let staging = store.config.expanded_cache_dir().join("staging");
    let fs = Vfs::new(vfs::VfsParts {
        index: store.index.clone(),
        journal: store.journal.clone(),
        blobs: store.blobs.clone(),
        staging_dir: staging,
        hydrate: Some(hydrate),
        resolve: Some(resolve),
        list: Some(list),
        read_only: store.config.fuse_options.ro,
    });
    let opts = vfs::mount_options(&store.config.fuse_options);
    tracing::info!(mount = %mount_dir.display(), "mounting iCloud Drive");
    vfs::mount(fs, &mount_dir, opts)
}

fn cmd_unmount(mount_dir: Option<PathBuf>) -> Result<()> {
    let mount_dir = resolve_mount(mount_dir);
    let status = std::process::Command::new("fusermount3")
        .arg("-uz")
        .arg(&mount_dir)
        .status();
    match status {
        Ok(s) if s.success() => println!("Unmounted {}", mount_dir.display()),
        Ok(_) => println!("fusermount3 reported a failure for {}", mount_dir.display()),
        Err(e) => tracing::warn!(error = %e, "fusermount3 is not available"),
    }
    Ok(())
}

fn cmd_sync(dry_run: bool) -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let client = build_client(&store.config);
    let engine = SyncEngine::new(
        store.config.clone(),
        store.index.clone(),
        store.journal.clone(),
        store.blobs.clone(),
        Box::new(client),
    );

    let ops = store.journal.pending()?;
    if dry_run {
        println!("Pending operations: {}", ops.len());
        for op in ops {
            println!(
                "  {:<7} {} -> {}",
                op.op.as_str(),
                op.path,
                op.target_path.as_deref().unwrap_or("-")
            );
        }
        let stale = store
            .index
            .stale_entries(store.config.index_ttl_seconds, 512)?;
        println!("Stale index entries: {}", stale.len());
        return Ok(());
    }

    let report = engine.sync_once()?;
    println!(
        "Sync complete: {} operation(s) applied, {} entry/entries refreshed",
        report.drained, report.refreshed
    );
    Ok(())
}

fn cmd_hydrate(dry_run: bool) -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let client = build_client(&store.config);
    let engine = SyncEngine::new(
        store.config.clone(),
        store.index.clone(),
        store.journal.clone(),
        store.blobs.clone(),
        Box::new(client),
    );
    let (pending, done) = engine.hydrate_all(dry_run)?;
    let sync_paths = store
        .config
        .sync_paths
        .as_ref()
        .map(|v| v.join(", "))
        .unwrap_or_else(|| "(all)".to_string());
    let exclude_paths = if store.config.exclude_paths.is_empty() {
        "(none)".to_string()
    } else {
        store.config.exclude_paths.join(", ")
    };
    println!("icloud-for-linux hydrate");
    println!("  Config:        {}", config_path.display());
    println!("  sync_paths:    {}", sync_paths);
    println!("  exclude_paths: {}", exclude_paths);
    println!();
    println!("  Files needing download: {}", pending);
    println!("  Downloaded:              {}", done);
    if dry_run {
        println!("(dry run: nothing downloaded)");
    }
    Ok(())
}

fn cmd_export(path: String, dest: PathBuf) -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let path = icloud_for_linux::config::normalize_icloud_path(&path);
    let nodes = store.index.all()?;
    let mut exported = 0;
    for node in nodes {
        if !node.path.starts_with(&path) {
            continue;
        }
        let Some(digest) = node.digest.as_ref() else {
            continue;
        };
        let relative = node.path.trim_start_matches(&path).trim_start_matches('/');
        let target = dest.join(relative);
        store.blobs.link_to(digest, &target)?;
        exported += 1;
    }
    println!("Exported {exported} file(s) to {}", dest.display());
    Ok(())
}

fn cmd_verify() -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let digests = store.blobs.list()?;
    let mut bad = 0;
    for digest in &digests {
        if !store.blobs.verify(digest)? {
            println!("CORRUPT: {digest}");
            bad += 1;
        }
    }
    println!(
        "Checked {} blob(s), {} corrupt, {} byte(s) on disk",
        digests.len(),
        bad,
        store.blobs.total_size()?
    );
    if bad > 0 {
        anyhow::bail!("{} corrupt blob(s)", bad);
    }
    Ok(())
}

fn cmd_gc() -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let client = build_client(&store.config);
    let engine = SyncEngine::new(
        store.config.clone(),
        store.index.clone(),
        store.journal.clone(),
        store.blobs.clone(),
        Box::new(client),
    );
    let live = engine.live_digests()?;
    let removed = store.blobs.gc(&live)?;
    println!(
        "Removed {removed} unreferenced blob(s), {} kept",
        live.len()
    );
    Ok(())
}

fn cmd_index() -> Result<()> {
    let config_path = resolve_config(None);
    let store = open_store(&config_path)?;
    let (total, hydrated, dirty) = store.index.counts()?;
    let (pending, failed, conflicts) = store.journal.counts()?;
    println!("icloud-for-linux index");
    println!("  Config:       {}", config_path.display());
    println!("  Entries:      {}", total);
    println!("  Hydrated:     {}", hydrated);
    println!("  Pending sync: {}", dirty);
    println!("  Journal:      {} pending, {} failed", pending, failed);
    println!("  Conflicts:    {}", conflicts);
    Ok(())
}

fn cmd_status() -> Result<()> {
    let active = service::service_is_active();
    let pid = service::service_main_pid();
    println!("service: {}", if active { "active" } else { "inactive" });
    println!(
        "pid:     {}",
        pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    );
    println!("mount:   {}", resolve_mount(None).display());
    println!("store:   {}", service::cache_dir().display());
    Ok(())
}

fn cmd_logs() -> Result<()> {
    let status = std::process::Command::new("journalctl")
        .args(["--user", "-u", service::SERVICE_NAME, "-f"])
        .status();
    if let Err(e) = status {
        anyhow::bail!("journalctl failed: {e}");
    }
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    println!("== icloud-for-linux doctor ==");
    for (label, ok) in [
        ("systemctl", which("systemctl").is_some()),
        (
            "fusermount3",
            which("fusermount3").is_some() || which("fusermount").is_some(),
        ),
        ("/dev/fuse", std::path::Path::new("/dev/fuse").exists()),
        ("config", service::config_file().exists()),
        ("env", service::env_file().exists()),
        ("unit", service::service_file().exists()),
        ("store", service::cache_dir().exists()),
    ] {
        println!("{}: {}", if ok { "OK" } else { "MISSING" }, label);
    }
    let mount = resolve_mount(None);
    println!(
        "mount point: {} ({})",
        mount.display(),
        if mount.exists() { "exists" } else { "missing" }
    );
    println!(
        "service: {}",
        if service::service_is_active() {
            "active"
        } else {
            "inactive"
        }
    );
    Ok(())
}

fn cmd_clear_cache() -> Result<()> {
    let cache = service::cache_dir();
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }
    std::fs::create_dir_all(&cache)?;
    println!("Cleared the local store under {}", cache.display());
    Ok(())
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(cmd))
        .find(|c| c.is_file())
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Init { mount_dir } => cmd_init(mount_dir),
        Command::Configure { email } => cmd_configure(email),
        Command::Auth {
            force_sms,
            debug,
            accept_terms,
            trust_token,
        } => icloud_for_linux::auth::run_auth(
            &resolve_config(None),
            force_sms,
            debug,
            accept_terms,
            trust_token,
        ),
        Command::Mount {
            mount_dir,
            config,
            foreground,
        } => cmd_mount(mount_dir, config, foreground),
        Command::Unmount { mount_dir } => cmd_unmount(mount_dir),
        Command::Sync { dry_run } => cmd_sync(dry_run),
        Command::Hydrate { dry_run } => cmd_hydrate(dry_run),
        Command::Export { path, dest } => cmd_export(path, dest),
        Command::Verify => cmd_verify(),
        Command::Gc => cmd_gc(),
        Command::Index => cmd_index(),
        Command::Status => cmd_status(),
        Command::Logs => cmd_logs(),
        Command::Doctor => cmd_doctor(),
        Command::ClearCache => cmd_clear_cache(),
    }
}
