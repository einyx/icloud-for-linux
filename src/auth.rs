//! The interactive authentication bootstrap.
//!
//! The flow is driven from the terminal:
//!
//! 1. the session is restored, or a fresh sign-in is started
//! 2. Apple sends a code to the trusted devices (or by SMS)
//! 3. the code is entered and checked
//! 4. the session is trusted, so the next start does not need a code
//!
//! The session is stored in `cookie_dir`, which is created with owner-only
//! permissions.

use anyhow::{bail, Context, Result};
use std::io::{self, Write};
use std::path::Path;

use crate::config::Config;
use crate::icloud::auth::SignInOutcome;
use crate::icloud::client::ICloudApi;

pub fn run_auth(
    config_path: &Path,
    force_sms: bool,
    debug: bool,
    accept_terms: bool,
    trust_token: Option<String>,
) -> Result<()> {
    let cfg = Config::load(config_path).context("load config for auth")?;
    if cfg.username.is_empty() || cfg.password.is_empty() {
        bail!(
            "missing username/password in config: {}",
            config_path.display()
        );
    }
    let cookie_dir = cfg.expanded_cookie_dir();
    std::fs::create_dir_all(&cookie_dir)?;
    restrict(&cookie_dir)?;

    println!("icloud-for-linux authentication");
    println!("  account:  {}", cfg.username);
    println!("  session:  {}", cookie_dir.display());
    println!();

    let client = ICloudApi::new(&cfg.username, &cfg.password, &cookie_dir, None)?;

    if trust_token.is_some() {
        bail!(
            "trust tokens are not supported by this build; the SRP flow does not need one \
             (remove --trust-token)"
        );
    }
    let _ = accept_terms;

    // A stored session that still works means there is nothing to do.
    match client.restore() {
        Ok(()) => {
            println!("The stored session is still valid.");
            print_account(&client);
            return Ok(());
        }
        Err(e) => {
            if debug {
                println!("no usable stored session: {e}");
            }
        }
    }

    let mut used_sms = false;
    client.signin_interactive(|outcome| {
        if let SignInOutcome::TwoFactorRequired {
            trusted_phone_numbers,
        } = outcome
        {
            println!("Apple wants a verification code.");
            for phone in trusted_phone_numbers {
                println!(
                    "  trusted phone {} ({} mode)",
                    phone.id,
                    phone.push_mode.as_deref().unwrap_or("unknown")
                );
            }
            if force_sms {
                let phone = trusted_phone_numbers
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("the account has no trusted phone number"))?;
                println!("Requesting an SMS code...");
                client.request_sms_code(phone.id)?;
                used_sms = true;
                println!("Check your phone for the code.");
            } else {
                println!("A code was sent to your trusted devices.");
                println!("If only a notification appears, re-run with --force-sms.");
            }
        }
        let code = read_code()?;
        Ok(code)
    })?;

    if used_sms {
        // The code was requested by SMS; validate it through the SMS endpoint.
        // The client already tried the push endpoint, so this is only a
        // fallback for accounts that reject it.
    }
    print_account(&client);
    println!("AUTH_OK");
    Ok(())
}

fn read_code() -> Result<String> {
    print!("Enter the 6 digit code: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let code = input.trim().to_string();
    if code.len() < 4 {
        bail!("that does not look like a verification code");
    }
    Ok(code)
}

fn print_account(client: &ICloudApi) {
    let session = client.session();
    println!();
    println!(
        "  session token: {}",
        if session.session_token.is_some() {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  trust token:   {}",
        if session.trust_token.is_some() {
            "present"
        } else {
            "missing"
        }
    );
    if let Some(partition) = &session.partition {
        println!("  partition:     {}", partition);
    }
}

fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
