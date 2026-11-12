use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const SERVICE_NAME: &str = "icloud.service";

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("icloud-linux")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/state"))
        .join("icloud-linux")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("~/.cache"))
        .join("icloud-linux")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.yaml")
}

pub fn env_file() -> PathBuf {
    config_dir().join("icloud.env")
}

pub fn service_file() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd")
        .join("user")
        .join(SERVICE_NAME)
}

pub fn log_file() -> PathBuf {
    state_dir().join("icloud.log")
}

pub fn ensure_dirs() -> Result<()> {
    for d in [
        config_dir(),
        state_dir(),
        cache_dir(),
        service_file().parent().unwrap().to_path_buf(),
    ] {
        std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

fn systemd_quote(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!("value must not contain newlines: {}", value);
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    Ok(format!("\"{}\"", escaped))
}

pub fn write_env(mount_dir: &Path) -> Result<()> {
    ensure_dirs()?;
    let contents = format!(
        "ICLOUD_CONFIG={}\nICLOUD_MOUNT={}\n",
        systemd_quote(&config_file().display().to_string())?,
        systemd_quote(&mount_dir.display().to_string())?
    );
    std::fs::write(env_file(), contents)?;
    restrict_permissions(&env_file())?;
    Ok(())
}

pub fn read_env_mount() -> Option<PathBuf> {
    let content = std::fs::read_to_string(env_file()).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("ICLOUD_MOUNT=") {
            let v = rest.trim().trim_matches('"');
            return Some(PathBuf::from(v));
        }
    }
    None
}

pub fn write_service(binary: &Path, mount_dir: &Path) -> Result<()> {
    ensure_dirs()?;
    let mount_q = systemd_quote(&mount_dir.display().to_string())?;
    let bin_q = systemd_quote(&binary.display().to_string())?;
    let unit = format!(
        r#"[Unit]
Description=iCloud Drive FUSE filesystem (user)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=-/usr/bin/fusermount3 -uz {mount}
ExecStop=-/usr/bin/fusermount3 -uz {mount}
ExecStart={bin} mount --foreground {mount}
Restart=on-failure
RestartSec=15
TimeoutStopSec=10
KillMode=control-group
ExecStopPost=-/usr/bin/fusermount3 -uz {mount}
Environment=ICLOUD_LOG_PATH={log}

[Install]
WantedBy=default.target
"#,
        mount = mount_q,
        bin = bin_q,
        log = systemd_quote(&log_file().display().to_string())?,
    );
    std::fs::write(service_file(), unit)?;
    Ok(())
}

pub fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

pub fn write_config_template(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let template = include_str!("../config.example.yaml");
    std::fs::write(path, template)?;
    restrict_permissions(path)?;
    Ok(())
}

pub fn write_config(
    path: &Path,
    username: &str,
    password: &str,
    cache: &Path,
    cookie: &Path,
) -> Result<()> {
    let contents = format!(
        "username: \"{}\"\npassword: \"{}\"\ncache_dir: \"{}\"\ncookie_dir: \"{}\"\nwarmup_mode: \"background\"\nconflict_mode: \"copy\"\nupload_interval_seconds: 30\nremote_refresh_interval_seconds: 300\nwarmup_workers: 1\nauto_sync: true\nfuse_options:\n  allow_other: false\n  ro: false\n  nonempty: true\n",
        username.replace('"', "\\\""),
        password.replace('"', "\\\""),
        cache.display(),
        cookie.display()
    );
    std::fs::write(path, contents)?;
    restrict_permissions(path)?;
    Ok(())
}

pub fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    Ok(status)
}

pub fn service_is_active() -> bool {
    matches!(systemctl(&["is-active", "--quiet", SERVICE_NAME]), Ok(out) if out.status.success())
}

pub fn service_main_pid() -> Option<u32> {
    let out = systemctl(&["show", SERVICE_NAME, "--property=MainPID", "--value"]).ok()?;
    let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    if pid > 0 {
        Some(pid)
    } else {
        None
    }
}
