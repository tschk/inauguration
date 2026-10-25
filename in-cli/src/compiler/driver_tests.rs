#[cfg(test)]
mod tests {
    use crate::compiler::driver::{compile_in_file, spec_from_cli};
    use crate::compiler::metadata::ArtifactKind;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let count = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "in-driver-test-{}-{}-{}",
            name,
            std::process::id(),
            count
        ))
    }

    #[test]
    fn test_compile_in_file_metadata_only() {
        let dir = temp_dir("metadata-only");
        fs::create_dir_all(&dir).unwrap();

        let source_path = dir.join("test.in");
        fs::write(&source_path, "fn main() -> Int { return 0; }").unwrap();

        let spec = spec_from_cli(
            "test",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let (output_path, metadata_path) = compile_in_file(&source_path, &spec, true).unwrap();

        assert!(output_path.exists());
        assert!(metadata_path.is_some());
        assert!(metadata_path.unwrap().exists());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_compile_in_file_no_metadata() {
        let dir = temp_dir("no-metadata");
        fs::create_dir_all(&dir).unwrap();

        let source_path = dir.join("test.in");
        fs::write(&source_path, "fn main() -> Int { return 0; }").unwrap();

        let spec = spec_from_cli(
            "test",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let (output_path, metadata_path) = compile_in_file(&source_path, &spec, false).unwrap();

        assert!(output_path.exists());
        assert!(metadata_path.is_none());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_compile_in_file_invalid_syntax() {
        let dir = temp_dir("invalid-syntax");
        fs::create_dir_all(&dir).unwrap();

        let source_path = dir.join("invalid.in");
        fs::write(&source_path, "fn main() -> Int { return ;").unwrap();

        let spec = spec_from_cli(
            "invalid",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let result = compile_in_file(&source_path, &spec, false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("compilation: frontend error"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_compile_in_file_missing_source() {
        let dir = temp_dir("missing-source");
        fs::create_dir_all(&dir).unwrap();

        let source_path = dir.join("missing.in");

        let spec = spec_from_cli(
            "missing",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let result = compile_in_file(&source_path, &spec, false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("read source"));

        fs::remove_dir_all(dir).unwrap();
    }
}
