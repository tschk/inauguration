//! Generic QEMU process argv helpers for freestanding images.
//!
//! Product repositories own machine names, expected UART lines, and timeouts.
//! This module only builds a deterministic argument vector.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QemuRunSpec {
    pub qemu_bin: String,
    pub machine: String,
    pub cpu: Option<String>,
    pub kernel: PathBuf,
    pub extra_args: Vec<String>,
    pub nographic: bool,
}

impl QemuRunSpec {
    pub fn new(qemu_bin: impl Into<String>, machine: impl Into<String>, kernel: impl AsRef<Path>) -> Self {
        Self {
            qemu_bin: qemu_bin.into(),
            machine: machine.into(),
            cpu: None,
            kernel: kernel.as_ref().to_path_buf(),
            extra_args: Vec::new(),
            nographic: true,
        }
    }

    pub fn argv(&self) -> Vec<String> {
        let mut args = vec![
            self.qemu_bin.clone(),
            "-machine".to_string(),
            self.machine.clone(),
        ];
        if let Some(cpu) = &self.cpu {
            args.push("-cpu".to_string());
            args.push(cpu.clone());
        }
        if self.nographic {
            args.push("-nographic".to_string());
        }
        args.push("-kernel".to_string());
        args.push(self.kernel.display().to_string());
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_an521_argv_is_stable() {
        let mut spec = QemuRunSpec::new("qemu-system-arm", "mps2-an521", "/tmp/image.elf");
        spec.cpu = Some("cortex-m33".into());
        spec.extra_args = vec!["-d".into(), "guest_errors".into()];
        assert_eq!(
            spec.argv(),
            [
                "qemu-system-arm",
                "-machine",
                "mps2-an521",
                "-cpu",
                "cortex-m33",
                "-nographic",
                "-kernel",
                "/tmp/image.elf",
                "-d",
                "guest_errors",
            ]
        );
    }
}
