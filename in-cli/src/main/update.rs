use crate::util::run_cmd;
use crate::{InError, Result};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

pub(crate) fn cmd_update(root: &Path) -> Result<()> {
    let in_cli = root.join("in-cli");
    let manifest = in_cli.join("Cargo.toml");
    if !manifest.is_file() {
        return Err(InError::Message(format!(
            "`in update` expected {} (run from inside an inauguration checkout)",
            manifest.display()
        )));
    }

    let start = Instant::now();
    println!("Reinstalling `in` from {} …", in_cli.display());

    let mut cmd = Command::new("cargo");
    cmd.arg("install").arg("--path").arg(&in_cli).arg("--force");
    if in_cli.join("Cargo.lock").is_file() {
        cmd.arg("--locked");
    }
    if let Some(root_dir) = inauguration::config::env_config().install_root() {
        cmd.arg("--root").arg(root_dir);
    }

    run_cmd(&mut cmd)?;

    println!(
        "`in` updated in {:.1}s (same version as in-cli/Cargo.toml).",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

pub(crate) fn github_repo_slug_for_remote_install() -> String {
    inauguration::config::env_config().github_repo_slug()
}

pub(crate) fn cmd_update_remote() -> Result<()> {
    #[cfg(unix)]
    {
        let repo = github_repo_slug_for_remote_install();
        let version = env!("CARGO_PKG_VERSION");
        let url = format!("https://raw.githubusercontent.com/{repo}/v{version}/install.sh");
        println!("No local inauguration checkout found; running remote install.sh ...");
        println!("Fetching: {url}");

        let response = reqwest::blocking::get(&url)
            .map_err(|e| InError::Message(format!("Failed to fetch install.sh: {}", e)))?;

        let status = response.status();
        if !status.is_success() {
            return Err(InError::Message(format!(
                "Failed to fetch install.sh: HTTP {}",
                status
            )));
        }

        let script = response
            .text()
            .map_err(|e| InError::Message(format!("Failed to read install.sh: {}", e)))?;

        let mut tmp_file = tempfile::NamedTempFile::new()
            .map_err(|e| InError::Message(format!("Failed to create temp file: {}", e)))?;

        use std::io::Write;
        tmp_file
            .write_all(script.as_bytes())
            .map_err(|e| InError::Message(format!("Failed to write to temp file: {}", e)))?;

        run_cmd(Command::new("bash").arg(tmp_file.path()))
    }
    #[cfg(not(unix))]
    {
        Err(InError::Message(
            "`in update` requires Unix for remote install.sh fallback; run from an inauguration checkout on this platform.".to_string(),
        ))
    }
}
