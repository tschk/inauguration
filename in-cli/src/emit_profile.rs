//! Emit profiles controlling IR optimization and native codegen shape.
//!
//! - [`EmitProfile::Default`] — standard optimize + conventional emit.
//! - [`EmitProfile::Harden`] — anti-decomp transforms and unusual codegen shapes
//!   (intentional fingerprint avoidance vs Ghidra/Hex-Rays heuristics).
//! - [`EmitProfile::Lean`] — aggressive inlining / shortest internal calls.
//!
//! Dual-emit (`--dual-emit` / `--harden-out`) produces a runtime artifact
//! (default or lean) plus a separate harden sample. Profiles stay orthogonal:
//! the runtime path must not run harden IR/codegen transforms.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// Codegen / optimization profile selected by `--profile` / `--harden` / `--lean`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmitProfile {
    #[default]
    Default,
    Harden,
    Lean,
}

impl EmitProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Harden => "harden",
            Self::Lean => "lean",
        }
    }

    /// Resolve CLI convenience flags. Explicit `profile` wins when not default;
    /// otherwise `--harden` / `--lean` apply (mutually exclusive).
    pub fn resolve(profile: Self, harden: bool, lean: bool) -> Result<Self, String> {
        if harden && lean {
            return Err("cannot combine --harden and --lean".to_string());
        }
        if profile != Self::Default {
            if harden && profile != Self::Harden {
                return Err(format!(
                    "--harden conflicts with --profile {}",
                    profile.as_str()
                ));
            }
            if lean && profile != Self::Lean {
                return Err(format!(
                    "--lean conflicts with --profile {}",
                    profile.as_str()
                ));
            }
            return Ok(profile);
        }
        if harden {
            return Ok(Self::Harden);
        }
        if lean {
            return Ok(Self::Lean);
        }
        Ok(Self::Default)
    }

    /// Resolve single-emit vs dual-emit (runtime + separate harden sample).
    ///
    /// `--harden-out` together with `--out` implies dual-emit even without
    /// `--dual-emit`. Dual-emit rejects `--harden` / `--profile harden` because
    /// the runtime artifact must be default or lean.
    pub fn resolve_dual_emit(
        profile: Self,
        harden: bool,
        lean: bool,
        dual_emit: bool,
        out: Option<&str>,
        harden_out: Option<&str>,
    ) -> Result<ResolvedEmit, String> {
        let harden_out = harden_out.filter(|s| !s.is_empty());
        let wants_dual = dual_emit || harden_out.is_some();
        if !wants_dual {
            return Ok(ResolvedEmit::Single(Self::resolve(profile, harden, lean)?));
        }

        let runtime_out = out
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "--out is required with --dual-emit / --harden-out".to_string())?;

        if harden || profile == Self::Harden {
            return Err(
                "cannot combine --dual-emit / --harden-out with --harden / --profile harden \
                 (runtime artifact must be default or lean; harden is the other emit)"
                    .to_string(),
            );
        }

        let runtime = Self::resolve(profile, false, lean)?;

        let harden_path = match harden_out {
            Some(path) => path.to_string(),
            None => derive_harden_out_path(runtime_out),
        };
        if harden_path == runtime_out {
            return Err("--harden-out must differ from --out".to_string());
        }

        Ok(ResolvedEmit::Dual {
            runtime,
            harden_out: harden_path,
        })
    }
}

/// Result of [`EmitProfile::resolve_dual_emit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedEmit {
    /// One artifact using the resolved profile.
    Single(EmitProfile),
    /// Runtime artifact (default|lean) plus a separate harden sample.
    Dual {
        runtime: EmitProfile,
        harden_out: String,
    },
}

/// Insert `-harden` before the file extension (`foo.o` → `foo-harden.o`).
/// Paths with no extension become `{name}-harden`.
pub fn derive_harden_out_path(out: &str) -> String {
    let path = Path::new(out);
    match path.extension() {
        Some(ext) => {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let file_name = format!("{stem}-harden");
            let file = Path::new(&file_name).with_extension(ext);
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    parent.join(file).to_string_lossy().into_owned()
                }
                _ => file.to_string_lossy().into_owned(),
            }
        }
        None => format!("{out}-harden"),
    }
}

impl fmt::Display for EmitProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EmitProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "harden" => Ok(Self::Harden),
            "lean" => Ok(Self::Lean),
            other => Err(format!(
                "unknown emit profile `{other}` (expected default|harden|lean)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profiles() {
        assert_eq!(
            "harden".parse::<EmitProfile>().unwrap(),
            EmitProfile::Harden
        );
        assert_eq!("LEAN".parse::<EmitProfile>().unwrap(), EmitProfile::Lean);
        assert!("nope".parse::<EmitProfile>().is_err());
    }

    #[test]
    fn resolve_flags() {
        assert_eq!(
            EmitProfile::resolve(EmitProfile::Default, true, false).unwrap(),
            EmitProfile::Harden
        );
        assert_eq!(
            EmitProfile::resolve(EmitProfile::Lean, false, false).unwrap(),
            EmitProfile::Lean
        );
        assert!(EmitProfile::resolve(EmitProfile::Default, true, true).is_err());
        assert!(EmitProfile::resolve(EmitProfile::Lean, true, false).is_err());
    }

    #[test]
    fn derive_harden_out_path_inserts_suffix() {
        assert_eq!(derive_harden_out_path("foo.o"), "foo-harden.o");
        assert_eq!(derive_harden_out_path("foo"), "foo-harden");
        assert_eq!(
            derive_harden_out_path("/tmp/sample.o"),
            "/tmp/sample-harden.o"
        );
        assert_eq!(derive_harden_out_path("./a.out"), "./a-harden.out");
    }

    #[test]
    fn resolve_dual_emit_default_runtime() {
        let plan = EmitProfile::resolve_dual_emit(
            EmitProfile::Default,
            false,
            false,
            true,
            Some("foo.o"),
            None,
        )
        .unwrap();
        assert_eq!(
            plan,
            ResolvedEmit::Dual {
                runtime: EmitProfile::Default,
                harden_out: "foo-harden.o".into(),
            }
        );
    }

    #[test]
    fn resolve_dual_emit_lean_runtime() {
        let via_flag = EmitProfile::resolve_dual_emit(
            EmitProfile::Default,
            false,
            true,
            true,
            Some("foo"),
            None,
        )
        .unwrap();
        let via_profile = EmitProfile::resolve_dual_emit(
            EmitProfile::Lean,
            false,
            false,
            true,
            Some("foo"),
            None,
        )
        .unwrap();
        assert_eq!(
            via_flag,
            ResolvedEmit::Dual {
                runtime: EmitProfile::Lean,
                harden_out: "foo-harden".into(),
            }
        );
        assert_eq!(via_flag, via_profile);
    }

    #[test]
    fn resolve_dual_emit_harden_out_implies_dual() {
        let plan = EmitProfile::resolve_dual_emit(
            EmitProfile::Default,
            false,
            false,
            false,
            Some("a.o"),
            Some("b.o"),
        )
        .unwrap();
        assert_eq!(
            plan,
            ResolvedEmit::Dual {
                runtime: EmitProfile::Default,
                harden_out: "b.o".into(),
            }
        );
    }

    #[test]
    fn resolve_dual_emit_rejects_harden_runtime() {
        assert!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Harden,
                false,
                false,
                true,
                Some("foo.o"),
                None,
            )
            .is_err()
        );
        assert!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Default,
                true,
                false,
                true,
                Some("foo.o"),
                None,
            )
            .is_err()
        );
        assert!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Harden,
                false,
                false,
                false,
                Some("foo.o"),
                Some("foo-harden.o"),
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_dual_emit_requires_out() {
        assert!(
            EmitProfile::resolve_dual_emit(EmitProfile::Default, false, false, true, None, None)
                .is_err()
        );
        assert!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Default,
                false,
                false,
                false,
                None,
                Some("foo-harden"),
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_dual_emit_rejects_same_out_paths() {
        assert!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Default,
                false,
                false,
                true,
                Some("foo.o"),
                Some("foo.o"),
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_dual_emit_single_unchanged() {
        assert_eq!(
            EmitProfile::resolve_dual_emit(
                EmitProfile::Default,
                true,
                false,
                false,
                Some("foo.o"),
                None,
            )
            .unwrap(),
            ResolvedEmit::Single(EmitProfile::Harden)
        );
        assert_eq!(
            EmitProfile::resolve_dual_emit(EmitProfile::Lean, false, false, false, None, None)
                .unwrap(),
            ResolvedEmit::Single(EmitProfile::Lean)
        );
    }
}
