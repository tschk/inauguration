use crate::util::run_cmd;
use crate::{InError, Result};
use std::path::Path;
use std::process::{Command, Stdio};
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
    let install_root = inauguration::config::env_config().install_root();
    if let Some(root_dir) = &install_root {
        cmd.arg("--root").arg(root_dir);
    }
    let output = cmd
        .arg("--message-format=json-render-diagnostics")
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(InError::Message(format!(
            "cargo install failed with status {}",
            output.status
        )));
    }
    verify_update_artifact(&output.stdout, install_root.as_deref())?;

    println!(
        "`in` updated in {:.1}s (same version as in-cli/Cargo.toml).",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn verify_update_artifact(messages: &[u8], install_root: Option<&Path>) -> Result<()> {
    let artifact = String::from_utf8_lossy(messages)
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("reason")?.as_str()? != "compiler-artifact"
                || value.get("target")?.get("name")?.as_str()? != "in"
            {
                return None;
            }
            let version = value
                .get("package_id")?
                .as_str()?
                .rsplit('#')
                .next()?
                .strip_prefix("inauguration@")?;
            let executable = value.get("executable")?.as_str()?;
            Some((std::path::PathBuf::from(executable), version.to_string()))
        })
        .next_back()
        .ok_or_else(|| {
            InError::Message(
                "cargo install reported success without an `in` executable artifact".into(),
            )
        })?;
    let expected = format!("in {}", artifact.1);
    let installed = install_root.map(|root| {
        root.join("bin")
            .join(if cfg!(windows) { "in.exe" } else { "in" })
    });
    for path in std::iter::once(&artifact.0).chain(installed.as_ref()) {
        let result = Command::new(path)
            .arg("--version")
            .output()
            .map_err(|error| {
                InError::Message(format!(
                    "updated compiler `{}` cannot execute: {error}",
                    path.display()
                ))
            })?;
        if !result.status.success() || String::from_utf8_lossy(&result.stdout).trim() != expected {
            return Err(InError::Message(format!(
                "updated compiler `{}` failed version verification (expected `{expected}`)",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn update_rejects_missing_damaged_and_wrong_version_executables() {
        assert!(verify_update_artifact(b"", None).is_err());
        let path = std::env::temp_dir().join(format!(
            "in-update-verify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let messages = serde_json::json!({"reason":"compiler-artifact", "target":{"name":"in"}, "package_id":"path+file:///fixture#inauguration@9.9.9", "executable":path}).to_string();
        std::fs::write(&path, "damaged output").unwrap();
        assert!(verify_update_artifact(messages.as_bytes(), None).is_err());
        std::fs::write(&path, "#!/bin/sh\nprintf 'in 0.0.0\\n'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_update_artifact(messages.as_bytes(), None).is_err());
        std::fs::write(&path, "#!/bin/sh\nprintf 'in 9.9.9\\n'\n").unwrap();
        assert!(verify_update_artifact(messages.as_bytes(), None).is_ok());
        let root = path.with_extension("root");
        let installed = root.join("bin/in");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "damaged installed copy").unwrap();
        assert!(verify_update_artifact(messages.as_bytes(), Some(&root)).is_err());
        std::fs::copy(&path, &installed).unwrap();
        let result = verify_update_artifact(messages.as_bytes(), Some(&root));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result.is_ok());
    }
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
        let snippet = "set -euo pipefail; tmp=$(mktemp); curl -fsSL \"$1\" -o \"$tmp\"; bash \"$tmp\"; rm -f \"$tmp\"";
        run_cmd(
            Command::new("bash")
                .arg("-c")
                .arg(snippet)
                .arg("--")
                .arg(&url),
        )
    }
    #[cfg(not(unix))]
    {
        Err(InError::Message(
            "`in update` requires Unix for remote install.sh fallback; run from an inauguration checkout on this platform.".to_string(),
        ))
    }
}
