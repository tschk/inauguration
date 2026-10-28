use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::package_manifest::{PACKAGE_MANIFEST_FILE, PackageDependency, PackageManifest};

pub const PACKAGE_LOCK_FILE: &str = "inauguration.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLock {
    pub lock_version: String,
    pub name: String,
    pub version: String,
    pub dependencies: BTreeMap<String, PackageDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLockValidation {
    pub valid: bool,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
    pub mismatched: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLockRoot {
    pub root: PathBuf,
    pub lock_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockSection {
    Dependencies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockDependencySubsection {
    Targets,
    Capabilities,
    Build,
}

pub fn load_package_lock(path: &Path) -> Result<PackageLock, String> {
    let lock_path = if path.is_dir() {
        path.join(PACKAGE_LOCK_FILE)
    } else {
        path.to_path_buf()
    };
    let source = fs::read_to_string(&lock_path)
        .map_err(|err| format!("failed to read {}: {err}", lock_path.display()))?;
    parse_package_lock(&source)
}

pub fn discover_package_lock(path: &Path) -> Option<PackageLockRoot> {
    let mut current = if path
        .file_name()
        .is_some_and(|name| name == PACKAGE_LOCK_FILE)
    {
        path.parent()
    } else if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };

    while let Some(dir) = current {
        let lock_path = dir.join(PACKAGE_LOCK_FILE);
        if lock_path.is_file() {
            return Some(PackageLockRoot {
                root: dir.to_path_buf(),
                lock_path,
            });
        }
        current = dir.parent();
    }

    None
}

pub fn load_package_lock_from_source(
    source_path: &Path,
) -> Result<(PackageLockRoot, PackageLock), String> {
    let root = discover_package_lock(source_path).ok_or_else(|| {
        format!(
            "could not find {PACKAGE_LOCK_FILE} for {}",
            source_path.display()
        )
    })?;
    let lock = load_package_lock(&root.lock_path)?;
    Ok((root, lock))
}

pub fn resolve_package_lock(manifest: &PackageManifest) -> PackageLock {
    PackageLock {
        lock_version: "1".to_string(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        dependencies: manifest.dependencies.clone(),
    }
}

pub fn format_package_lock(lock: &PackageLock) -> String {
    let mut lines = vec![
        format!("lock-version: {}", lock.lock_version),
        format!("name: {}", lock.name),
        format!("version: {}", lock.version),
    ];
    if !lock.dependencies.is_empty() {
        lines.push("dependencies:".to_string());
        for (name, dependency) in &lock.dependencies {
            lines.push(format!("  {name}:"));
            lines.push(format!("    version: {}", dependency.version));
            if let Some(kind) = &dependency.kind {
                lines.push(format!("    kind: {kind}"));
            }
            if let Some(source) = &dependency.source {
                lines.push(format!("    source: {source}"));
            }
            if let Some(rev) = &dependency.rev {
                lines.push(format!("    rev: {rev}"));
            }
            if let Some(checksum) = &dependency.checksum {
                lines.push(format!("    checksum: {checksum}"));
            }
            if let Some(install_path) = &dependency.install_path {
                lines.push(format!("    install_path: {install_path}"));
            }
            if !dependency.targets.is_empty() {
                lines.push("    targets:".to_string());
                for target in &dependency.targets {
                    lines.push(format!("      - {target}"));
                }
            }
            if !dependency.capabilities.is_empty() {
                lines.push("    capabilities:".to_string());
                for capability in &dependency.capabilities {
                    lines.push(format!("      - {capability}"));
                }
            }
            if !dependency.build.is_empty() {
                lines.push("    build:".to_string());
                for (key, value) in &dependency.build {
                    lines.push(format!("      {key}: {value}"));
                }
            }
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn write_package_lock(path: &Path, lock: &PackageLock) -> Result<(), String> {
    let lock_path = if path.is_dir() {
        path.join(PACKAGE_LOCK_FILE)
    } else {
        path.to_path_buf()
    };
    fs::write(&lock_path, format_package_lock(lock))
        .map_err(|err| format!("failed to write {}: {err}", lock_path.display()))
}

pub fn validate_package_lock(
    manifest: &PackageManifest,
    lock: &PackageLock,
) -> PackageLockValidation {
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for (name, dependency) in &manifest.dependencies {
        match lock.dependencies.get(name) {
            None => missing.push(name.clone()),
            Some(locked) if !dependency_matches(dependency, locked) => {
                mismatched.push(name.clone());
            }
            Some(_) => {}
        }
    }
    let extra = lock
        .dependencies
        .keys()
        .filter(|name| !manifest.dependencies.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    PackageLockValidation {
        valid: missing.is_empty() && mismatched.is_empty() && extra.is_empty(),
        missing,
        extra,
        mismatched,
    }
}

pub fn load_manifest_and_lock(
    path: &Path,
) -> Result<(PackageManifest, Option<PackageLock>, PackageLockValidation), String> {
    let manifest_path = if path.is_dir() {
        path.join(PACKAGE_MANIFEST_FILE)
    } else if path
        .file_name()
        .is_some_and(|name| name == PACKAGE_MANIFEST_FILE)
    {
        path.to_path_buf()
    } else {
        return Err(format!(
            "expected directory, {PACKAGE_MANIFEST_FILE}, or {PACKAGE_LOCK_FILE}"
        ));
    };
    let manifest = crate::package_manifest::load_package_manifest(&manifest_path)?;
    let lock = discover_package_lock(&manifest_path)
        .and_then(|root| load_package_lock(&root.lock_path).ok());
    let validation = lock
        .as_ref()
        .map(|entry| validate_package_lock(&manifest, entry))
        .unwrap_or(PackageLockValidation {
            valid: manifest.dependencies.is_empty(),
            missing: manifest.dependencies.keys().cloned().collect(),
            extra: Vec::new(),
            mismatched: Vec::new(),
        });
    Ok((manifest, lock, validation))
}

fn dependency_matches(manifest: &PackageDependency, lock: &PackageDependency) -> bool {
    if manifest.version != lock.version {
        return false;
    }
    if manifest.kind != lock.kind {
        return false;
    }
    if manifest.source != lock.source {
        return false;
    }
    if manifest.rev != lock.rev {
        return false;
    }
    if manifest.checksum.is_some() && manifest.checksum != lock.checksum {
        return false;
    }
    if !manifest.targets.is_empty() && manifest.targets != lock.targets {
        return false;
    }
    if !manifest.capabilities.is_empty() && manifest.capabilities != lock.capabilities {
        return false;
    }
    if !manifest.build.is_empty() && manifest.build != lock.build {
        return false;
    }
    true
}

pub fn parse_package_lock(source: &str) -> Result<PackageLock, String> {
    let mut lock = PackageLock {
        lock_version: String::new(),
        name: String::new(),
        version: String::new(),
        dependencies: BTreeMap::new(),
    };
    let mut section = None;
    let mut dependency_name: Option<String> = None;
    let mut dependency_subsection: Option<LockDependencySubsection> = None;

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        if raw_line.trim().is_empty() {
            continue;
        }
        if raw_line.contains('\t') {
            return Err(format!(
                "line {line_number}: tabs are not valid indentation in inauguration.lock"
            ));
        }

        let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
        let line = raw_line.trim_start_matches(' ');

        match indent {
            0 => {
                dependency_name = None;
                dependency_subsection = None;
                section = parse_lock_top_level(line, line_number, &mut lock)?;
            }
            2 => match section {
                Some(LockSection::Dependencies) => {
                    dependency_subsection = None;
                    dependency_name =
                        Some(parse_lock_dependency_header(line, line_number, &mut lock)?);
                }
                None => {
                    return Err(format!(
                        "line {line_number}: indentation is only valid inside a section"
                    ));
                }
            },
            4 => {
                if section != Some(LockSection::Dependencies) {
                    return Err(format!(
                        "line {line_number}: indentation is only valid for dependency metadata"
                    ));
                }
                let name = dependency_name.as_deref().ok_or_else(|| {
                    format!("line {line_number}: dependency metadata requires a dependency name")
                })?;
                dependency_subsection =
                    parse_lock_dependency_field(line, line_number, name, &mut lock)?;
            }
            6 => {
                if section != Some(LockSection::Dependencies) {
                    return Err(format!(
                        "line {line_number}: indentation is only valid for dependency metadata"
                    ));
                }
                let name = dependency_name.as_deref().ok_or_else(|| {
                    format!("line {line_number}: dependency metadata requires a dependency name")
                })?;
                parse_lock_dependency_nested_field(
                    line,
                    line_number,
                    name,
                    dependency_subsection,
                    &mut lock,
                )?;
            }
            _ => {
                return Err(format!(
                    "line {line_number}: malformed indentation; use 0, 2, 4, or 6 spaces"
                ));
            }
        }
    }

    if lock.lock_version.is_empty() {
        return Err("missing required field `lock-version`".into());
    }
    if lock.name.is_empty() {
        return Err("missing required field `name`".into());
    }
    if lock.version.is_empty() {
        return Err("missing required field `version`".into());
    }
    for (name, dependency) in &lock.dependencies {
        if dependency.version.is_empty() {
            return Err(format!(
                "dependency `{name}` is missing required field `version`"
            ));
        }
    }

    Ok(lock)
}

fn parse_lock_top_level(
    line: &str,
    line_number: usize,
    lock: &mut PackageLock,
) -> Result<Option<LockSection>, String> {
    let (key, value) = split_lock_field(line, line_number)?;
    match key {
        "lock-version" => {
            lock.lock_version =
                required_lock_scalar(value, line_number, "lock-version")?.to_string();
            Ok(None)
        }
        "name" => {
            lock.name = required_lock_scalar(value, line_number, "name")?.to_string();
            Ok(None)
        }
        "version" => {
            lock.version = required_lock_scalar(value, line_number, "version")?.to_string();
            Ok(None)
        }
        "dependencies" => parse_lock_section_header(
            value,
            line_number,
            "dependencies",
            LockSection::Dependencies,
        ),
        other => Err(format!(
            "line {line_number}: unknown top-level field `{other}`"
        )),
    }
}

fn parse_lock_section_header(
    value: &str,
    line_number: usize,
    name: &str,
    section: LockSection,
) -> Result<Option<LockSection>, String> {
    if value.is_empty() {
        Ok(Some(section))
    } else {
        Err(format!(
            "line {line_number}: section `{name}` must not have an inline value"
        ))
    }
}

fn parse_lock_dependency_header(
    line: &str,
    line_number: usize,
    lock: &mut PackageLock,
) -> Result<String, String> {
    let key = crate::package_ref::split_dependency_header(line, line_number)?;
    if lock
        .dependencies
        .insert(key.clone(), PackageDependency::default())
        .is_some()
    {
        return Err(format!("line {line_number}: duplicate dependency `{key}`"));
    }
    Ok(key)
}

fn parse_lock_dependency_field(
    line: &str,
    line_number: usize,
    dependency_name: &str,
    lock: &mut PackageLock,
) -> Result<Option<LockDependencySubsection>, String> {
    let (key, value) = split_lock_field(line, line_number)?;
    let dependency = lock
        .dependencies
        .get_mut(dependency_name)
        .ok_or_else(|| format!("line {line_number}: unknown dependency `{dependency_name}`"))?;
    match key {
        "version" => {
            let version = required_lock_scalar(value, line_number, "version")?;
            if !dependency.version.is_empty() {
                return Err(format!(
                    "line {line_number}: duplicate version for dependency `{dependency_name}`"
                ));
            }
            dependency.version = version.to_string();
            Ok(None)
        }
        "kind" => assign_optional_dependency_field(
            dependency,
            line_number,
            dependency_name,
            "kind",
            required_lock_scalar(value, line_number, "kind")?,
            |dependency, value| dependency.kind = Some(value.to_string()),
        ),
        "source" => assign_optional_dependency_field(
            dependency,
            line_number,
            dependency_name,
            "source",
            required_lock_scalar(value, line_number, "source")?,
            |dependency, value| dependency.source = Some(value.to_string()),
        ),
        "rev" => assign_optional_dependency_field(
            dependency,
            line_number,
            dependency_name,
            "rev",
            required_lock_scalar(value, line_number, "rev")?,
            |dependency, value| dependency.rev = Some(value.to_string()),
        ),
        "checksum" => assign_optional_dependency_field(
            dependency,
            line_number,
            dependency_name,
            "checksum",
            required_lock_scalar(value, line_number, "checksum")?,
            |dependency, value| dependency.checksum = Some(value.to_string()),
        ),
        "install_path" => assign_optional_dependency_field(
            dependency,
            line_number,
            dependency_name,
            "install_path",
            required_lock_scalar(value, line_number, "install_path")?,
            |dependency, value| dependency.install_path = Some(value.to_string()),
        ),
        "targets" => parse_lock_dependency_subsection_header(
            value,
            line_number,
            "targets",
            LockDependencySubsection::Targets,
        ),
        "capabilities" => parse_lock_dependency_subsection_header(
            value,
            line_number,
            "capabilities",
            LockDependencySubsection::Capabilities,
        ),
        "build" => parse_lock_dependency_subsection_header(
            value,
            line_number,
            "build",
            LockDependencySubsection::Build,
        ),
        other => Err(format!(
            "line {line_number}: unknown dependency field `{other}` for `{dependency_name}`"
        )),
    }
}

fn assign_optional_dependency_field(
    dependency: &mut PackageDependency,
    line_number: usize,
    dependency_name: &str,
    field: &str,
    value: &str,
    assign: impl FnOnce(&mut PackageDependency, &str),
) -> Result<Option<LockDependencySubsection>, String> {
    match field {
        "kind" if dependency.kind.is_some() => {
            return Err(format!(
                "line {line_number}: duplicate kind for dependency `{dependency_name}`"
            ));
        }
        "source" if dependency.source.is_some() => {
            return Err(format!(
                "line {line_number}: duplicate source for dependency `{dependency_name}`"
            ));
        }
        "rev" if dependency.rev.is_some() => {
            return Err(format!(
                "line {line_number}: duplicate rev for dependency `{dependency_name}`"
            ));
        }
        "checksum" if dependency.checksum.is_some() => {
            return Err(format!(
                "line {line_number}: duplicate checksum for dependency `{dependency_name}`"
            ));
        }
        "install_path" if dependency.install_path.is_some() => {
            return Err(format!(
                "line {line_number}: duplicate install_path for dependency `{dependency_name}`"
            ));
        }
        _ => {}
    }
    assign(dependency, value);
    Ok(None)
}

fn parse_lock_dependency_subsection_header(
    value: &str,
    line_number: usize,
    name: &str,
    subsection: LockDependencySubsection,
) -> Result<Option<LockDependencySubsection>, String> {
    if value.is_empty() {
        Ok(Some(subsection))
    } else {
        Err(format!(
            "line {line_number}: dependency subsection `{name}` must not have an inline value"
        ))
    }
}

fn parse_lock_dependency_nested_field(
    line: &str,
    line_number: usize,
    dependency_name: &str,
    subsection: Option<LockDependencySubsection>,
    lock: &mut PackageLock,
) -> Result<(), String> {
    let subsection = subsection.ok_or_else(|| {
        format!("line {line_number}: nested dependency metadata requires a subsection")
    })?;
    let dependency = lock
        .dependencies
        .get_mut(dependency_name)
        .ok_or_else(|| format!("line {line_number}: unknown dependency `{dependency_name}`"))?;
    match subsection {
        LockDependencySubsection::Targets => parse_lock_list_item(
            line,
            line_number,
            "dependency targets",
            &mut dependency.targets,
        ),
        LockDependencySubsection::Capabilities => parse_lock_list_item(
            line,
            line_number,
            "dependency capabilities",
            &mut dependency.capabilities,
        ),
        LockDependencySubsection::Build => {
            let (key, value) = split_lock_field(line, line_number)?;
            let value = required_lock_scalar(value, line_number, key)?;
            if dependency
                .build
                .insert(key.to_string(), value.to_string())
                .is_some()
            {
                return Err(format!(
                    "line {line_number}: duplicate build field `{key}` for dependency `{dependency_name}`"
                ));
            }
            Ok(())
        }
    }
}

fn parse_lock_list_item(
    line: &str,
    line_number: usize,
    section: &str,
    values: &mut Vec<String>,
) -> Result<(), String> {
    let value = if let Some(rest) = line.strip_prefix("- ") {
        rest.trim()
    } else if line.trim() == "-" {
        ""
    } else {
        return Err(format!(
            "line {line_number}: section `{section}` only supports list items"
        ));
    };
    if value.is_empty() {
        return Err(format!(
            "line {line_number}: section `{section}` contains an empty list item"
        ));
    }
    values.push(value.to_string());
    Ok(())
}

fn split_lock_field(line: &str, line_number: usize) -> Result<(&str, &str), String> {
    let Some((key, value)) = line.split_once(':') else {
        return Err(format!("line {line_number}: expected `key: value`"));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("line {line_number}: empty key"));
    }
    Ok((key, value.trim()))
}

fn required_lock_scalar<'a>(
    value: &'a str,
    line_number: usize,
    field: &str,
) -> Result<&'a str, String> {
    if value.is_empty() {
        Err(format!(
            "line {line_number}: field `{field}` requires a value"
        ))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before UNIX_EPOCH")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "inauguration-package-lock-{}-{}-{}",
                std::process::id(),
                unique,
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn parse_text(source: &str) -> Result<PackageLock, String> {
        parse_package_lock(source)
    }

    #[test]
    fn parse_package_lock_empty_string() {
        let result = parse_package_lock("");
        assert!(result.is_err());
    }
    #[test]
    fn reports_error_for_empty_lock_file() {
        let result = parse_text("");
        assert_eq!(result, Err("missing required field `lock-version`".into()));
    }

    #[test]
    fn resolves_lock_from_manifest() {
        let manifest = crate::package_manifest::load_package_manifest_from_source(Path::new("."))
            .ok()
            .map(|(_, manifest)| manifest);
        let manifest = manifest.unwrap_or_else(|| PackageManifest {
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            entry: None,
            targets: BTreeMap::new(),
            dependencies: BTreeMap::from([(
                "postgres".into(),
                PackageDependency {
                    version: "^1.0.0".into(),
                    kind: Some("registry".into()),
                    source: Some("https://registry.inauguration.dev".into()),
                    ..Default::default()
                },
            )]),
            capabilities: Vec::new(),
            extensions: Vec::new(),
        });

        let lock = resolve_package_lock(&manifest);

        assert_eq!(lock.lock_version, "1");
        assert_eq!(lock.name, "hyperchat");
        assert_eq!(lock.version, "0.1.0");
        assert_eq!(
            lock.dependencies
                .get("postgres")
                .map(|dep| dep.version.as_str()),
            Some("^1.0.0")
        );
    }

    #[test]
    fn formats_package_lock_correctly() {
        let lock_empty = PackageLock {
            lock_version: "1".into(),
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::new(),
        };

        let formatted = format_package_lock(&lock_empty);
        let expected = "\
lock-version: 1
name: hyperchat
version: 0.1.0
";
        assert_eq!(formatted, expected);

        let lock_full = PackageLock {
            lock_version: "1".into(),
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::from([(
                "postgres".into(),
                PackageDependency {
                    version: "1.2.3".into(),
                    kind: Some("registry".into()),
                    source: Some("https://registry.inauguration.dev".into()),
                    rev: Some("abc123".into()),
                    checksum: Some("sha256:deadbeef".into()),
                    targets: vec!["macos".into(), "linux".into()],
                    capabilities: vec!["network.http".into()],
                    build: BTreeMap::from([("profile".into(), "release".into())]),
                    install_path: Some("local_dep".into()),
                },
            )]),
        };

        let formatted_full = format_package_lock(&lock_full);
        let expected_full = "\
lock-version: 1
name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: 1.2.3
    kind: registry
    source: https://registry.inauguration.dev
    rev: abc123
    checksum: sha256:deadbeef
    install_path: local_dep
    targets:
      - macos
      - linux
    capabilities:
      - network.http
    build:
      profile: release
";
        assert_eq!(formatted_full, expected_full);
    }

    #[test]
    fn round_trips_package_lock_file() {
        let lock = PackageLock {
            lock_version: "1".into(),
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::from([(
                "postgres".into(),
                PackageDependency {
                    version: "1.2.3".into(),
                    kind: Some("registry".into()),
                    source: Some("https://registry.inauguration.dev".into()),
                    rev: Some("abc123".into()),
                    checksum: Some("sha256:deadbeef".into()),
                    targets: vec!["macos".into()],
                    capabilities: vec!["network.http".into()],
                    build: BTreeMap::from([("profile".into(), "release".into())]),
                    install_path: None,
                },
            )]),
        };

        let parsed = parse_text(&format_package_lock(&lock)).expect("parse formatted lock");

        assert_eq!(parsed, lock);
    }

    #[test]
    fn writes_and_loads_package_lock_from_directory() {
        let temp = TempDirGuard::new();
        let lock = PackageLock {
            lock_version: "1".into(),
            name: "sample".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::new(),
        };

        write_package_lock(&temp.path, &lock).expect("write lock");
        let loaded = load_package_lock(&temp.path).expect("load lock");

        assert_eq!(loaded, lock);
    }

    #[test]
    fn validates_lock_against_manifest() {
        let manifest = PackageManifest {
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            entry: None,
            targets: BTreeMap::new(),
            dependencies: BTreeMap::from([(
                "postgres".into(),
                PackageDependency {
                    version: "^1.0.0".into(),
                    kind: Some("registry".into()),
                    ..Default::default()
                },
            )]),
            capabilities: Vec::new(),
            extensions: Vec::new(),
        };
        let lock = PackageLock {
            lock_version: "1".into(),
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::from([(
                "postgres".into(),
                PackageDependency {
                    version: "^1.0.0".into(),
                    kind: Some("registry".into()),
                    ..Default::default()
                },
            )]),
        };

        let validation = validate_package_lock(&manifest, &lock);

        assert!(validation.valid);
        assert!(validation.missing.is_empty());
        assert!(validation.extra.is_empty());
        assert!(validation.mismatched.is_empty());
    }

    #[test]
    fn reports_missing_and_mismatched_lock_entries() {
        let manifest = PackageManifest {
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            entry: None,
            targets: BTreeMap::new(),
            dependencies: BTreeMap::from([
                (
                    "postgres".into(),
                    PackageDependency {
                        version: "^1.0.0".into(),
                        ..Default::default()
                    },
                ),
                (
                    "redis".into(),
                    PackageDependency {
                        version: "latest".into(),
                        ..Default::default()
                    },
                ),
            ]),
            capabilities: Vec::new(),
            extensions: Vec::new(),
        };
        let lock = PackageLock {
            lock_version: "1".into(),
            name: "hyperchat".into(),
            version: "0.1.0".into(),
            dependencies: BTreeMap::from([
                (
                    "postgres".into(),
                    PackageDependency {
                        version: "1.0.0".into(),
                        ..Default::default()
                    },
                ),
                (
                    "cache".into(),
                    PackageDependency {
                        version: "0.2.0".into(),
                        ..Default::default()
                    },
                ),
            ]),
        };

        let validation = validate_package_lock(&manifest, &lock);

        assert!(!validation.valid);
        assert_eq!(validation.missing, vec!["redis".to_string()]);
        assert_eq!(validation.extra, vec!["cache".to_string()]);
        assert_eq!(validation.mismatched, vec!["postgres".to_string()]);
    }

    #[test]
    fn loads_manifest_and_lock_together() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join(PACKAGE_MANIFEST_FILE),
            r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
        )
        .expect("write manifest");
        let manifest =
            crate::package_manifest::load_package_manifest(&temp.path).expect("load manifest");
        let lock = resolve_package_lock(&manifest);
        write_package_lock(&temp.path, &lock).expect("write lock");

        let (loaded_manifest, loaded_lock, validation) =
            load_manifest_and_lock(&temp.path).expect("load manifest and lock");

        assert_eq!(loaded_manifest.name, "hyperchat");
        assert_eq!(
            loaded_lock.as_ref().map(|entry| entry.name.as_str()),
            Some("hyperchat")
        );
        assert!(validation.valid);
    }

    #[test]
    fn parse_valid_complex_lock() {
        let source = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.2.3
    kind: registry
    source: https://registry.com
    rev: abc
    checksum: sha256:123
    install_path: /opt/dep1
    targets:
      - linux
      - macos
    capabilities:
      - fs
    build:
      mode: release
";
        let lock = parse_text(source).expect("valid parse");
        assert_eq!(lock.lock_version, "1");
        assert_eq!(lock.name, "my-app");
        assert_eq!(lock.version, "1.0.0");
        let dep = lock.dependencies.get("dep1").unwrap();
        assert_eq!(dep.version, "1.2.3");
        assert_eq!(dep.kind.as_deref(), Some("registry"));
        assert_eq!(dep.source.as_deref(), Some("https://registry.com"));
        assert_eq!(dep.rev.as_deref(), Some("abc"));
        assert_eq!(dep.checksum.as_deref(), Some("sha256:123"));
        assert_eq!(dep.install_path.as_deref(), Some("/opt/dep1"));
        assert_eq!(dep.targets, vec!["linux", "macos"]);
        assert_eq!(dep.capabilities, vec!["fs"]);
        assert_eq!(dep.build.get("mode").map(|s| s.as_str()), Some("release"));
    }

    #[test]
    fn parse_invalid_indentation() {
        let tabs = "\
lock-version: 1
\tname: my-app
";
        assert!(
            parse_text(tabs)
                .unwrap_err()
                .contains("tabs are not valid indentation")
        );

        let odd_spaces = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
   dep1:
";
        assert!(
            parse_text(odd_spaces)
                .unwrap_err()
                .contains("malformed indentation")
        );

        let indent_outside_section = "\
lock-version: 1
  name: my-app
";
        assert!(
            parse_text(indent_outside_section)
                .unwrap_err()
                .contains("indentation is only valid inside a section")
        );

        let deep_indent_outside_deps = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
        extra:
";
        assert!(
            parse_text(deep_indent_outside_deps)
                .unwrap_err()
                .contains("malformed indentation")
        );
    }

    #[test]
    fn parse_missing_required_fields() {
        let no_lock_version = "\
name: my-app
version: 1.0.0
";
        assert!(
            parse_text(no_lock_version)
                .unwrap_err()
                .contains("missing required field `lock-version`")
        );

        let no_name = "\
lock-version: 1
version: 1.0.0
";
        assert!(
            parse_text(no_name)
                .unwrap_err()
                .contains("missing required field `name`")
        );

        let no_version = "\
lock-version: 1
name: my-app
";
        assert!(
            parse_text(no_version)
                .unwrap_err()
                .contains("missing required field `version`")
        );

        let no_dep_version = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    kind: registry
";
        assert!(
            parse_text(no_dep_version)
                .unwrap_err()
                .contains("missing required field `version`")
        );
    }

    #[test]
    fn parse_duplicates() {
        let dup_dep = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
  dep1:
    version: 2.0.0
";
        assert!(
            parse_text(dup_dep)
                .unwrap_err()
                .contains("duplicate dependency `dep1`")
        );

        let dup_version = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    version: 2.0.0
";
        assert!(
            parse_text(dup_version)
                .unwrap_err()
                .contains("duplicate version")
        );

        let dup_build = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    build:
      mode: release
      mode: debug
";
        assert!(
            parse_text(dup_build)
                .unwrap_err()
                .contains("duplicate build field `mode`")
        );

        let dup_kind = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    kind: git
    kind: registry
";
        assert!(parse_text(dup_kind).unwrap_err().contains("duplicate kind"));
    }

    #[test]
    fn parse_invalid_section_headers() {
        let inline_deps = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies: { dep1: 1.0.0 }
";
        assert!(
            parse_text(inline_deps)
                .unwrap_err()
                .contains("must not have an inline value")
        );

        let inline_build = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    build: mode
";
        assert!(
            parse_text(inline_build)
                .unwrap_err()
                .contains("must not have an inline value")
        );
    }

    #[test]
    fn formats_complex_package_lock() {
        let lock = PackageLock {
            lock_version: "1".into(),
            name: "my-app".into(),
            version: "1.0.0".into(),
            dependencies: BTreeMap::from([(
                "dep1".into(),
                crate::package_manifest::PackageDependency {
                    version: "1.2.3".into(),
                    kind: Some("registry".into()),
                    source: Some("https://registry.com".into()),
                    rev: Some("abc".into()),
                    checksum: Some("sha256:123".into()),
                    targets: vec!["linux".into(), "macos".into()],
                    capabilities: vec!["fs".into()],
                    build: BTreeMap::from([("mode".into(), "release".into())]),
                    install_path: Some("/opt/dep1".into()),
                },
            )]),
        };

        let formatted = format_package_lock(&lock);
        let expected = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.2.3
    kind: registry
    source: https://registry.com
    rev: abc
    checksum: sha256:123
    install_path: /opt/dep1
    targets:
      - linux
      - macos
    capabilities:
      - fs
    build:
      mode: release
";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn parse_invalid_list_items() {
        let empty_item = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    targets:
      -
";
        assert!(
            parse_text(empty_item)
                .unwrap_err()
                .contains("empty list item")
        );

        let not_item = "\
lock-version: 1
name: my-app
version: 1.0.0
dependencies:
  dep1:
    version: 1.0.0
    targets:
      linux
";
        assert!(
            parse_text(not_item)
                .unwrap_err()
                .contains("only supports list items")
        );
    }
}
