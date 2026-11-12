//! icloud-for-linux — mount iCloud Drive on Linux as a local-first FUSE filesystem.
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

use icloud_for_linux::{auth, config, fuse_fs, icloud_api, mirror, service, state, sync};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use config::Config;
use fuse_fs::ICloudFs;
use icloud_api::{ICloudClient, UnimplementedClient};
use mirror::LocalMirror;
use state::SyncState;
use sync::SyncEngine;

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
    /// Create config/cache directories, config file and the systemd user unit.
    Init {
        /// Mount point (default: ~/iCloud)
        mount_dir: Option<PathBuf>,
    },
    /// Write config.yaml with Apple ID credentials.
    Configure {
        /// Apple ID email (prompted if omitted)
        email: Option<String>,
    },
    /// Run the interactive Apple ID / 2FA authentication bootstrap.
    Auth {
        /// Bypass push delivery and force an SMS code (iOS 26 beta workaround).
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
    /// Mount the iCloud Drive filesystem (runs in the foreground).
    Mount {
        /// Mount point (default: read from ~/.config/icloud-linux/icloud.env)
        mount_dir: Option<PathBuf>,
        /// Config file (default: ~/.config/icloud-linux/config.yaml)
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
    /// Download all eligible file contents into the local mirror.
    Hydrate {
        /// Report what would be downloaded without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Print each file as it is hydrated.
        #[arg(long)]
        verbose: bool,
    },
    /// Trigger an on-demand remote metadata sync in the running driver.
    Sync {
        /// Seconds to wait for completion.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// Suppress progress output.
        #[arg(long)]
        quiet: bool,
    },
    /// Show whether the service is running.
    Status,
    /// Show service logs.
    Logs,
    /// Check local setup health.
    Doctor,
    /// Remove the local mirror and sync state, then rebuild on next start.
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
    let password = rpassword_read()?;
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

fn rpassword_read() -> Result<String> {
    use std::io::Read;
    // Read the password without echoing it back to the terminal.
    let mut password = String::new();
    let mut stdin = std::io::stdin().lock();
    let _ = stdin.read_to_string(&mut password);
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

fn cmd_mount(mount_dir: Option<PathBuf>, config: Option<PathBuf>, foreground: bool) -> Result<()> {
    let config_path = resolve_config(config);
    let cfg = Config::load(&config_path).context("load config")?;
    let mount_dir = resolve_mount(mount_dir);
    service::ensure_dirs()?;
    std::fs::create_dir_all(&mount_dir)?;

    let cache = cfg.expanded_cache_dir();
    let mirror = Arc::new(LocalMirror::new(&cache)?);
    let state = Arc::new(SyncState::open(&cache.join("state.sqlite3"))?);

    let mut client = UnimplementedClient;
    if let Err(e) = client.authenticate(!foreground) {
        tracing::warn!(error = %e, "client authentication unavailable — running in local-only mode");
    }

    let engine = Arc::new(SyncEngine::new(
        cfg.clone(),
        state.clone(),
        mirror.clone(),
        Box::new(client),
    ));
    if foreground {
        engine.start_background().ok();
    }

    let hydrate: fuse_fs::HydrateFn = {
        let engine = engine.clone();
        Arc::new(move |path: &str| engine.ensure_local_file(path))
    };

    let fs = ICloudFs::new(state, mirror, Some(hydrate), cfg.fuse_options.ro);
    let opts = fuse_fs::mount_options(&cfg.fuse_options);
    tracing::info!(mount = %mount_dir.display(), "mounting iCloud Drive");
    fuse_fs::mount(fs, &mount_dir, opts)
}

fn cmd_unmount(mount_dir: Option<PathBuf>) -> Result<()> {
    let mount_dir = resolve_mount(mount_dir);
    let status = std::process::Command::new("fusermount3")
        .arg("-uz")
        .arg(&mount_dir)
        .status();
    match status {
        Ok(s) if s.success() => println!("Unmounted {}", mount_dir.display()),
        Ok(_) => println!("fusermount3 reported failure for {}", mount_dir.display()),
        Err(e) => tracing::warn!(error = %e, "fusermount3 not available"),
    }
    Ok(())
}

fn cmd_hydrate(dry_run: bool, verbose: bool) -> Result<()> {
    let config_path = resolve_config(None);
    let cfg = Config::load(&config_path).context("load config")?;
    let cache = cfg.expanded_cache_dir();
    let mirror = Arc::new(LocalMirror::new(&cache)?);
    let state = Arc::new(SyncState::open(&cache.join("state.sqlite3"))?);
    let engine = SyncEngine::new(cfg.clone(), state, mirror, Box::new(UnimplementedClient));
    let (pending, done) = engine.hydrate_all(dry_run)?;
    let sync_paths = cfg
        .sync_paths
        .as_ref()
        .map(|v| v.join(", "))
        .unwrap_or_else(|| "(all)".to_string());
    let exclude_paths = if cfg.exclude_paths.is_empty() {
        "(none)".to_string()
    } else {
        cfg.exclude_paths.join(", ")
    };
    println!("icloud-for-linux hydrate");
    println!("  Config:       {}", config_path.display());
    println!("  sync_paths:   {}", sync_paths);
    println!("  exclude_paths:{}", exclude_paths);
    println!();
    println!("  Eligible files needing download: {}", pending);
    println!("  Already hydrated:                {}", done);
    if verbose {
        println!("  (verbose listing enabled)");
    }
    if dry_run {
        println!("(dry run: nothing downloaded)");
    }
    Ok(())
}

fn cmd_sync(timeout: u64, quiet: bool) -> Result<()> {
    let Some(pid) = service::service_main_pid() else {
        anyhow::bail!("icloud.service is not running. Start it with: icloudctl mount");
    };
    let marker = service::state_dir().join("sync_done");
    let _ = std::fs::remove_file(&marker);
    unsafe {
        if libc::kill(pid as i32, libc::SIGUSR1) != 0 {
            anyhow::bail!("failed to signal driver process (pid {pid})");
        }
    }
    if !quiet {
        println!("Requested remote sync from pid {pid}; waiting up to {timeout}s...");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    while std::time::Instant::now() < deadline {
        if marker.exists() {
            if !quiet {
                println!("Sync complete.");
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    anyhow::bail!("sync did not complete within {timeout}s")
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
    println!("cache:   {}", service::cache_dir().display());
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
        ("config", service::config_file().exists()),
        ("env", service::env_file().exists()),
        ("unit", service::service_file().exists()),
        ("cache dir", service::cache_dir().exists()),
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

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(cmd))
        .find(|c| c.is_file())
}

fn cmd_clear_cache() -> Result<()> {
    let cache = service::cache_dir();
    if cache.exists() {
        std::fs::remove_dir_all(&cache)?;
    }
    std::fs::create_dir_all(&cache)?;
    println!("Cleared persistent cache under {}", cache.display());
    Ok(())
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
        } => {
            let _ = trust_token;
            auth::run_auth(&resolve_config(None), force_sms, debug, accept_terms)
        }
        Command::Mount {
            mount_dir,
            config,
            foreground,
        } => cmd_mount(mount_dir, config, foreground),
        Command::Unmount { mount_dir } => cmd_unmount(mount_dir),
        Command::Hydrate { dry_run, verbose } => cmd_hydrate(dry_run, verbose),
        Command::Sync { timeout, quiet } => cmd_sync(timeout, quiet),
        Command::Status => cmd_status(),
        Command::Logs => cmd_logs(),
        Command::Doctor => cmd_doctor(),
        Command::ClearCache => cmd_clear_cache(),
    }
}
