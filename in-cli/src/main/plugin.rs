use crate::util::run_cmd;
use crate::{InError, PluginAction, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// A plugin name identifies one `.sh` file directly under a plugin directory.
///
/// `Command::arg` passes it to `bash` without shell evaluation, so this is a
/// path-containment contract rather than a shell-token blacklist.
fn plugin_name(name: &str) -> Result<&str> {
    let path = Path::new(name);
    let mut components = path.components();
    if name.is_empty()
        || name.contains(['/', '\\'])
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(InError::Message("invalid plugin name".to_string()));
    }
    Ok(name)
}

/// Resolve an existing relative target and prove it remains inside `root`.
/// Canonicalization makes symlink escapes observable before the target reaches
/// an installed plugin.
fn workspace_target(root: &Path, target: &str) -> Result<PathBuf> {
    let target_path = Path::new(target);
    if target.contains('\\')
        || !target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
        || target_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(InError::Message(
            "plugin target must stay within the workspace".to_string(),
        ));
    }

    let root = root.canonicalize()?;
    let resolved = root.join(target_path).canonicalize()?;
    if !resolved.starts_with(&root) {
        return Err(InError::Message(
            "plugin target must stay within the workspace".to_string(),
        ));
    }
    Ok(resolved)
}

pub(crate) fn cmd_plugin(root: &Path, action: PluginAction) -> Result<()> {
    match action {
        PluginAction::List => {
            println!("built-in:");
            let reg = plugin_registry_dir(root);
            if reg.exists() {
                for entry in fs::read_dir(reg)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    if let Some(name) = name.to_str()
                        && name.ends_with(".sh")
                    {
                        println!("  {}", name.trim_end_matches(".sh"));
                    }
                }
            }
            let install = plugin_install_dir()?;
            println!("installed:");
            if install.exists() {
                for entry in fs::read_dir(install)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    if let Some(name) = name.to_str()
                        && name.ends_with(".sh")
                    {
                        println!("  {}", name.trim_end_matches(".sh"));
                    }
                }
            }
            Ok(())
        }
        PluginAction::Install { name } => {
            let name = plugin_name(&name)?;
            let src = plugin_registry_dir(root).join(format!("{name}.sh"));
            if !src.exists() {
                return Err(InError::Message(format!("unknown plugin: {name}")));
            }
            let dst_dir = plugin_install_dir()?;
            fs::create_dir_all(&dst_dir)?;
            let dst = dst_dir.join(format!("{name}.sh"));
            fs::copy(&src, &dst)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&dst)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&dst, perms)?;
            }
            println!("installed plugin: {name}");
            Ok(())
        }
        PluginAction::Run { name, target } => {
            let name = plugin_name(&name)?;
            let target = workspace_target(root, &target)?;
            let script = plugin_install_dir()?.join(format!("{name}.sh"));
            if !script.exists() {
                return Err(InError::Message(format!(
                    "plugin not installed: {name} (run `in plugin install {name}`)"
                )));
            }
            run_cmd(
                Command::new("bash")
                    .arg("--")
                    .arg(&script)
                    .arg(target)
                    .arg(root),
            )
        }
    }
}

fn plugin_registry_dir(root: &Path) -> PathBuf {
    root.join("plugins").join("registry")
}

fn plugin_install_dir() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| InError::Message("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("in")
        .join("plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn plugin_name_is_one_non_empty_path_component() {
        assert!(plugin_name("aurorality").is_ok());
        assert!(plugin_name("my-plugin_2").is_ok());
        assert!(plugin_name("semi;colon").is_err());
        assert!(plugin_name("").is_err());
        assert!(plugin_name(".").is_err());
        assert!(plugin_name("..").is_err());
        assert!(plugin_name("nested/plugin").is_err());
        assert!(plugin_name("nested\\plugin").is_err());
        assert!(plugin_name("/absolute").is_err());
    }

    #[test]
    fn target_must_resolve_within_workspace() {
        let root = unique_temp_dir("plugin-target-root");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            workspace_target(&root, ".").unwrap(),
            root.canonicalize().unwrap()
        );
        assert_eq!(
            workspace_target(&root, "nested").unwrap(),
            nested.canonicalize().unwrap()
        );
        assert!(workspace_target(&root, "../plugin-target-outside").is_err());
        assert!(workspace_target(&root, "/tmp").is_err());
        assert!(workspace_target(&root, "missing").is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn target_symlink_outside_workspace_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = unique_temp_dir("plugin-target-root");
        let outside = unique_temp_dir("plugin-target-outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        assert!(workspace_target(&root, "escape").is_err());

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "inauguration-{name}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
}
