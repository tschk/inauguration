//! Environment-based configuration for the `in` CLI.
//!
//! All `IN_*` environment variable reads live here so the surface is
//! discoverable and documented instead of scattered through business logic.
//! This is not a general settings layer; values are read once per process
//! and remain static.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Singleton environment config, parsed once per process.
static ENV_CONFIG: OnceLock<EnvConfig> = OnceLock::new();

/// Returns the parsed environment config for this process.
#[must_use]
pub fn env_config() -> &'static EnvConfig {
    ENV_CONFIG.get_or_init(EnvConfig::from_env)
}

/// All environment-controlled knobs and overrides.
#[derive(Debug, Clone)]
pub struct EnvConfig {
    /// `IN_SKIP_VERIFY`: skip Core IR verification in the JIT path.
    pub skip_verify: bool,
    /// `IN_STRICT_LOWERING`: fail the build when the native lowerer replaced any
    /// function with a runtime trap. Off by default because the self-hosting
    /// path intentionally degrades a small number of stdlib functions.
    pub strict_lowering: bool,
    /// `IN_SIL_CALLEE_DRIVEN_HOTRELOAD`: enable experimental callee-driven hotreload.
    pub sil_callee_driven_hotreload: bool,
    /// `IN_NATIVE_SWIFT_SIL`: when present and not equal to `only`, allow native Swift SIL.
    pub native_swift_sil_enabled: bool,
    /// `IN_PARSER`: force the Core IR front parser (`in` or `icore`) for any file path.
    pub parser_override: Option<String>,
    /// `IN_DAEMON_SOCKET`: custom Unix socket path for the compiler daemon.
    pub daemon_socket: Option<PathBuf>,
    /// `IN_INSTALL_DIR`: override the cargo install root for `in update`.
    pub install_dir: Option<String>,
    /// `IN_REPO`: override the GitHub repository slug for remote installs.
    pub repo_slug: Option<String>,
}

impl EnvConfig {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            skip_verify: std::env::var("IN_SKIP_VERIFY").is_ok(),
            strict_lowering: std::env::var("IN_STRICT_LOWERING")
                .ok()
                .is_some_and(|v| parse_env_bool(&v)),
            sil_callee_driven_hotreload: std::env::var("IN_SIL_CALLEE_DRIVEN_HOTRELOAD")
                .ok()
                .is_some_and(|v| parse_env_bool(&v)),
            native_swift_sil_enabled: std::env::var("IN_NATIVE_SWIFT_SIL")
                .map(|v| v.to_lowercase() != "only")
                .unwrap_or(false),
            parser_override: std::env::var("IN_PARSER").ok(),
            daemon_socket: std::env::var("IN_DAEMON_SOCKET").ok().map(PathBuf::from),
            install_dir: std::env::var("IN_INSTALL_DIR").ok(),
            repo_slug: std::env::var("IN_REPO").ok(),
        }
    }

    /// Cargo `--root` directory for `in update`, derived from `IN_INSTALL_DIR`.
    /// Returns the parent of the supplied bin directory if it is non-empty.
    #[must_use]
    pub fn install_root(&self) -> Option<PathBuf> {
        let trimmed = self.install_dir.as_deref()?.trim();
        if trimmed.is_empty() {
            return None;
        }
        PathBuf::from(trimmed).parent().map(PathBuf::from)
    }

    /// Validated GitHub `owner/repo` slug for remote installs, or the default.
    #[must_use]
    pub fn github_repo_slug(&self) -> String {
        const DEFAULT: &str = "tschk/inauguration";
        let raw = self.repo_slug.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            return DEFAULT.to_string();
        }
        let ok = raw.contains('/')
            && raw.matches('/').count() == 1
            && !raw.starts_with('/')
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/');
        if ok {
            raw.to_string()
        } else {
            eprintln!("warning: IN_REPO is not a valid owner/repo slug; using {DEFAULT}");
            DEFAULT.to_string()
        }
    }
}

/// Parse typical truthy env-var strings: `1`, `true`, `yes`, `on`.
/// Empty, `0`, `false`, `no`, and `off` are treated as false.
#[must_use]
fn parse_env_bool(value: &str) -> bool {
    let v = value.trim().to_lowercase();
    !v.is_empty() && v != "0" && v != "false" && v != "no" && v != "off"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_bool_accepts_typical_truthy_values() {
        assert!(parse_env_bool("1"));
        assert!(parse_env_bool("true"));
        assert!(parse_env_bool("yes"));
        assert!(parse_env_bool("on"));
        assert!(parse_env_bool(" TRUE "));
    }

    #[test]
    fn parse_env_bool_rejects_typical_falsey_values() {
        assert!(!parse_env_bool(""));
        assert!(!parse_env_bool("0"));
        assert!(!parse_env_bool("false"));
        assert!(!parse_env_bool("no"));
        assert!(!parse_env_bool("off"));
    }
}
