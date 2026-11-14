//! Generates the shell completions for `icloudctl`.
//!
//! Usage: `icloudctl-completions <bash|zsh|fish>`

use clap::CommandFactory;
use clap_complete::{generate, shells};

fn main() {
    let shell = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bash".to_string());
    let mut command = icloud_for_linux::cli::Cli::command();
    match shell.as_str() {
        "bash" => generate(
            shells::Bash,
            &mut command,
            "icloudctl",
            &mut std::io::stdout(),
        ),
        "zsh" => generate(
            shells::Zsh,
            &mut command,
            "icloudctl",
            &mut std::io::stdout(),
        ),
        "fish" => generate(
            shells::Fish,
            &mut command,
            "icloudctl",
            &mut std::io::stdout(),
        ),
        "elvish" => generate(
            shells::Elvish,
            &mut command,
            "icloudctl",
            &mut std::io::stdout(),
        ),
        other => {
            eprintln!("unknown shell: {other} (bash, zsh, fish and elvish are supported)");
            std::process::exit(2);
        }
    }
}
