//! Emit profiles controlling IR optimization and native codegen shape.
//!
//! - [`EmitProfile::Default`] — standard optimize + conventional emit.
//! - [`EmitProfile::Harden`] — anti-decomp transforms and unusual codegen shapes
//!   (intentional fingerprint avoidance vs Ghidra/Hex-Rays heuristics).
//! - [`EmitProfile::Lean`] — aggressive inlining / shortest internal calls.

use serde::{Deserialize, Serialize};
use std::fmt;
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
    fn as_str_mapping() {
        assert_eq!(EmitProfile::Default.as_str(), "default");
        assert_eq!(EmitProfile::Harden.as_str(), "harden");
        assert_eq!(EmitProfile::Lean.as_str(), "lean");
    }
}
