use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PACKAGE_MANIFEST_FILE: &str = "inauguration.package";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub targets: BTreeMap<String, bool>,
    pub dependencies: BTreeMap<String, PackageDependency>,
    pub capabilities: Vec<String>,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PackageDependency {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_path: Option<String>,
}

impl PackageDependency {
    pub fn resolved_source_path(&self) -> Option<PathBuf> {
        if let Some(source) = self.source.as_deref().filter(|value| !value.is_empty())
            && (self.kind.as_deref() == Some("path")
                || self.version.strip_prefix("path:").is_some())
        {
            return Some(PathBuf::from(source));
        }
        self.version.strip_prefix("path:").map(PathBuf::from)
    }

    pub fn resolved_install_path(&self, package_root: &Path) -> Option<PathBuf> {
        self.install_path
            .as_ref()
            .map(|path| package_root.join(path))
            .filter(|path| path.is_dir())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageCompileContext {
    pub name: String,
    pub entry: Option<String>,
    pub dependency_search_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRoot {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageTargetSelection {
    pub requested: Vec<String>,
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub unknown: Vec<String>,
}

impl PackageTargetSelection {
    pub fn target_enabled(&self, target: &str) -> bool {
        if self.disabled.iter().any(|d| d == target) {
            return false;
        }
        if self.enabled.is_empty() {
            return true;
        }
        self.enabled.iter().any(|e| e == target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPolicyValidation {
    pub valid: bool,
    pub allowed: Vec<String>,
    pub required: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageReport {
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: PackageManifest,
    pub target_selection: PackageTargetSelection,
    pub capability_policy: CapabilityPolicyValidation,
    pub graph: PackageGraphReport,
    pub source_identity: Option<PackageSourceIdentity>,
    pub semantic_imports: Vec<PackageSemanticImport>,
    pub semantic_bindings: Vec<PackageSemanticBinding>,
    pub symbol_index: Vec<PackageSymbolIndexEntry>,
    pub diagnostics: Vec<PackageDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageGraphReport {
    pub package_id: String,
    pub nodes: Vec<PackageGraphNode>,
    pub edges: Vec<PackageGraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageGraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSourceIdentity {
    pub package: Option<String>,
    pub module: Option<String>,
    pub manifest_name: Option<String>,
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSemanticImport {
    pub import: String,
    pub dependency: Option<String>,
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSemanticBinding {
    pub import: String,
    pub alias: String,
    pub dependency: Option<String>,
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSymbolIndexEntry {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub source_import: String,
    pub dependency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDiagnostic {
    pub code: String,
    pub severity: String,
    pub import: String,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Targets,
    Dependencies,
    Capabilities,
    Extensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencySubsection {
    Targets,
    Capabilities,
    Build,
}

pub fn load_package_manifest(path: &Path) -> Result<PackageManifest, String> {
    let manifest_path = if path.is_dir() {
        path.join(PACKAGE_MANIFEST_FILE)
    } else {
        path.to_path_buf()
    };
    let source = fs::read_to_string(&manifest_path)
        .map_err(|err| format!("failed to read {}: {err}", manifest_path.display()))?;
    parse_package_manifest(&source)
}

pub fn write_package_manifest(path: &Path, manifest: &PackageManifest) -> Result<(), String> {
    let source = format_package_manifest(manifest);
    fs::write(path, source).map_err(|err| format!("write {}: {err}", path.display()))
}

pub fn format_package_manifest(manifest: &PackageManifest) -> String {
    let mut out = String::new();
    out.push_str(&format!("name: {}\n", manifest.name));
    out.push_str(&format!("version: {}\n", manifest.version));
    if let Some(entry) = &manifest.entry {
        out.push_str(&format!("entry: {entry}\n"));
    }
    if !manifest.targets.is_empty() {
        out.push_str("targets:\n");
        for (target, enabled) in &manifest.targets {
            out.push_str(&format!("  {target}: {enabled}\n"));
        }
    }
    if !manifest.dependencies.is_empty() {
        out.push_str("dependencies:\n");
        for (key, dependency) in &manifest.dependencies {
            out.push_str(&format!("  {key}:\n"));
            out.push_str(&format!("    version: {}\n", dependency.version));
            if let Some(kind) = &dependency.kind {
                out.push_str(&format!("    kind: {kind}\n"));
            }
            if let Some(source) = &dependency.source {
                out.push_str(&format!("    source: {source}\n"));
            }
            if let Some(rev) = &dependency.rev {
                out.push_str(&format!("    rev: {rev}\n"));
            }
            if let Some(checksum) = &dependency.checksum {
                out.push_str(&format!("    checksum: {checksum}\n"));
            }
            if let Some(install_path) = &dependency.install_path {
                out.push_str(&format!("    install_path: {install_path}\n"));
            }
        }
    }
    if !manifest.capabilities.is_empty() {
        out.push_str("capabilities:\n");
        for capability in &manifest.capabilities {
            out.push_str(&format!("  - {capability}\n"));
        }
    }
    if !manifest.extensions.is_empty() {
        out.push_str("extensions:\n");
        for extension in &manifest.extensions {
            out.push_str(&format!("  - {extension}\n"));
        }
    }
    out
}

pub fn resolve_dependency_install_path(
    package_root: &Path,
    dependency_key: &str,
    dependency: &PackageDependency,
    lock: Option<&crate::package_lock::PackageLock>,
) -> Option<PathBuf> {
    let locked = lock.and_then(|lock| lock.dependencies.get(dependency_key));
    locked
        .and_then(|dep| dep.resolved_install_path(package_root))
        .or_else(|| dependency.resolved_install_path(package_root))
        .or_else(|| {
            dependency
                .resolved_source_path()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        package_root.join(path)
                    }
                })
                .filter(|path| path.is_dir())
        })
}

pub fn discover_package_root(path: &Path) -> Option<PackageRoot> {
    let mut current = if path
        .file_name()
        .is_some_and(|name| name == PACKAGE_MANIFEST_FILE)
    {
        path.parent()
    } else if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };

    while let Some(dir) = current {
        let manifest_path = dir.join(PACKAGE_MANIFEST_FILE);
        if manifest_path.is_file() {
            return Some(PackageRoot {
                root: dir.to_path_buf(),
                manifest_path,
            });
        }
        current = dir.parent();
    }

    None
}

pub fn load_package_manifest_from_source(
    source_path: &Path,
) -> Result<(PackageRoot, PackageManifest), String> {
    let root = discover_package_root(source_path).ok_or_else(|| {
        format!(
            "could not find {PACKAGE_MANIFEST_FILE} for {}",
            source_path.display()
        )
    })?;
    let manifest = load_package_manifest(&root.manifest_path)?;
    Ok((root, manifest))
}

pub fn compile_context_in_dir(dir: &Path) -> Option<PackageCompileContext> {
    discover_package_root(dir).and_then(|root| compile_context_at_root(&root.root))
}

pub fn compile_context_for_source(source_path: &Path) -> Option<PackageCompileContext> {
    discover_package_root(source_path).and_then(|root| compile_context_at_root(&root.root))
}

pub fn compile_context_at_root(package_root: &Path) -> Option<PackageCompileContext> {
    let manifest_path = package_root.join(PACKAGE_MANIFEST_FILE);
    if !manifest_path.is_file() {
        return None;
    }
    let manifest = load_package_manifest(&manifest_path).ok()?;
    let lock = crate::package_lock::discover_package_lock(package_root)
        .and_then(|root| crate::package_lock::load_package_lock(&root.lock_path).ok());
    let dependency_search_paths =
        dependency_search_paths_for_root(package_root, &manifest, lock.as_ref());
    Some(PackageCompileContext {
        name: manifest.name,
        entry: manifest.entry,
        dependency_search_paths,
    })
}

fn dependency_search_paths_for_root(
    package_root: &Path,
    manifest: &PackageManifest,
    lock: Option<&crate::package_lock::PackageLock>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (key, dependency) in &manifest.dependencies {
        let locked = lock.and_then(|lock| lock.dependencies.get(key));
        let candidate = locked
            .and_then(|dep| dep.resolved_install_path(package_root))
            .or_else(|| dependency.resolved_install_path(package_root))
            .or_else(|| {
                dependency
                    .resolved_source_path()
                    .map(|path| {
                        if path.is_absolute() {
                            path
                        } else {
                            package_root.join(path)
                        }
                    })
                    .filter(|path| path.is_dir())
            });
        if let Some(path) = candidate {
            let key = path.display().to_string();
            if seen.insert(key) {
                paths.push(path);
            }
        }
    }
    paths
}

pub fn load_package_report_from_source<I, S, J, T>(
    source_path: &Path,
    requested_targets: I,
    required_capabilities: J,
) -> Result<PackageReport, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    J: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let (root, manifest) = load_package_manifest_from_source(source_path)?;
    let mut report = package_report(root, manifest, requested_targets, required_capabilities);
    let semantic_imports = source_semantic_imports(source_path).unwrap_or_default();
    let semantic_bindings = source_semantic_bindings(source_path).unwrap_or_default();
    report.source_identity = Some(source_identity_for_path(
        source_path,
        Some(&report.manifest.name),
    ));
    report.semantic_imports = resolve_semantic_imports(&semantic_imports, Some(&report.manifest));
    report.semantic_bindings =
        resolve_semantic_bindings(&semantic_bindings, &report.semantic_imports);
    let lock = crate::package_lock::discover_package_lock(&report.root)
        .and_then(|root| crate::package_lock::load_package_lock(&root.lock_path).ok());
    report.symbol_index = symbol_index_for_semantic_imports_with_context(
        &report.semantic_imports,
        Some(&report.root),
        Some(&report.manifest),
        lock.as_ref(),
    );
    report
        .symbol_index
        .extend(symbol_index_for_semantic_bindings(
            &report.semantic_bindings,
        ));
    report.diagnostics = diagnostics_for_semantic_imports(&report.semantic_imports);
    Ok(report)
}

pub fn package_report<I, S, J, T>(
    root: PackageRoot,
    manifest: PackageManifest,
    requested_targets: I,
    required_capabilities: J,
) -> PackageReport
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    J: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let target_selection = manifest.select_targets(requested_targets);
    let capability_policy = manifest.validate_capability_policy(required_capabilities);
    let graph = package_graph_report(&manifest);
    PackageReport {
        root: root.root,
        manifest_path: root.manifest_path,
        manifest,
        target_selection,
        capability_policy,
        graph,
        source_identity: None,
        semantic_imports: Vec::new(),
        semantic_bindings: Vec::new(),
        symbol_index: Vec::new(),
        diagnostics: Vec::new(),
    }
}

fn source_semantic_imports(source_path: &Path) -> Result<Vec<String>, String> {
    if source_path.extension().and_then(|ext| ext.to_str()) != Some("in") {
        return Ok(Vec::new());
    }
    let source = fs::read_to_string(source_path)
        .map_err(|err| format!("failed to read {}: {err}", source_path.display()))?;
    let surface = crate::in_lang_parse::parse_in_surface_info(&source)?;
    Ok(surface.semantic_imports)
}

fn source_semantic_bindings(
    source_path: &Path,
) -> Result<Vec<crate::in_lang_parse::InSemanticBinding>, String> {
    if source_path.extension().and_then(|ext| ext.to_str()) != Some("in") {
        return Ok(Vec::new());
    }
    let source = fs::read_to_string(source_path).map_err(|err| err.to_string())?;
    let surface = crate::in_lang_parse::parse_in_surface_info(&source)?;
    Ok(surface.semantic_bindings)
}

pub fn semantic_imports_for_source_path(
    source_path: &Path,
    manifest: Option<&PackageManifest>,
) -> Result<Vec<PackageSemanticImport>, String> {
    let imports = source_semantic_imports(source_path)?;
    Ok(resolve_semantic_imports(&imports, manifest))
}

pub fn resolve_semantic_imports(
    imports: &[String],
    manifest: Option<&PackageManifest>,
) -> Vec<PackageSemanticImport> {
    imports
        .iter()
        .map(|import| resolve_semantic_import(import, manifest))
        .collect()
}

pub fn resolve_semantic_bindings(
    bindings: &[crate::in_lang_parse::InSemanticBinding],
    imports: &[PackageSemanticImport],
) -> Vec<PackageSemanticBinding> {
    bindings
        .iter()
        .map(|binding| {
            let resolved = imports
                .iter()
                .find(|import| import.import == binding.import && import.status == "resolved");
            if let Some(import) = resolved {
                PackageSemanticBinding {
                    import: binding.import.clone(),
                    alias: binding.alias.clone(),
                    dependency: import.dependency.clone(),
                    status: "resolved".to_string(),
                    reason: "semantic-import-resolved".to_string(),
                }
            } else {
                PackageSemanticBinding {
                    import: binding.import.clone(),
                    alias: binding.alias.clone(),
                    dependency: None,
                    status: "unresolved".to_string(),
                    reason: "semantic-import-unresolved".to_string(),
                }
            }
        })
        .collect()
}

fn resolve_semantic_import(
    import: &str,
    manifest: Option<&PackageManifest>,
) -> PackageSemanticImport {
    let Some(manifest) = manifest else {
        return PackageSemanticImport {
            import: import.to_string(),
            dependency: None,
            status: "unresolved".to_string(),
            reason: "package-manifest-missing".to_string(),
        };
    };
    if manifest.dependencies.contains_key(import) {
        return PackageSemanticImport {
            import: import.to_string(),
            dependency: Some(import.to_string()),
            status: "resolved".to_string(),
            reason: dependency_resolution_reason(import).to_string(),
        };
    }
    if let Some(package_ref) = crate::package_ref::parse_package_ref(import) {
        for (key, dependency) in &manifest.dependencies {
            if let Some(dep_ref) = crate::package_ref::package_ref_for_dependency(key, dependency)
                && dep_ref == package_ref
            {
                return PackageSemanticImport {
                    import: import.to_string(),
                    dependency: Some(key.clone()),
                    status: "resolved".to_string(),
                    reason: "dependency-ecosystem-match".to_string(),
                };
            }
        }
    }
    let suffix = import.rsplit('.').next().unwrap_or(import);
    if suffix != import && manifest.dependencies.contains_key(suffix) {
        return PackageSemanticImport {
            import: import.to_string(),
            dependency: Some(suffix.to_string()),
            status: "resolved".to_string(),
            reason: "dependency-suffix-match".to_string(),
        };
    }
    PackageSemanticImport {
        import: import.to_string(),
        dependency: None,
        status: "unresolved".to_string(),
        reason: "dependency-not-declared".to_string(),
    }
}

fn dependency_resolution_reason(import: &str) -> &'static str {
    if crate::package_ref::parse_package_ref(import).is_some() {
        "dependency-ecosystem-exact-match"
    } else {
        "dependency-exact-match"
    }
}

pub fn parse_package_manifest_source(source: &str) -> Result<PackageManifest, String> {
    parse_package_manifest(source)
}

pub fn symbol_index_for_semantic_imports(
    imports: &[PackageSemanticImport],
) -> Vec<PackageSymbolIndexEntry> {
    symbol_index_for_semantic_imports_with_context(imports, None, None, None)
}

pub fn symbol_index_for_semantic_bindings(
    bindings: &[PackageSemanticBinding],
) -> Vec<PackageSymbolIndexEntry> {
    bindings
        .iter()
        .filter(|binding| binding.status == "resolved")
        .filter_map(|binding| {
            let dependency = binding.dependency.as_ref()?;
            Some(PackageSymbolIndexEntry {
                id: format!("symbol:binding:{}", binding.alias),
                kind: "binding".to_string(),
                name: binding.alias.clone(),
                source_import: binding.import.clone(),
                dependency: dependency.clone(),
            })
        })
        .collect()
}

pub fn symbol_index_for_semantic_imports_with_context(
    imports: &[PackageSemanticImport],
    package_root: Option<&Path>,
    manifest: Option<&PackageManifest>,
    lock: Option<&crate::package_lock::PackageLock>,
) -> Vec<PackageSymbolIndexEntry> {
    let mut entries = Vec::new();
    for import in imports {
        let dependency = match import.dependency.as_ref() {
            Some(dependency) if import.status == "resolved" => dependency,
            _ => continue,
        };
        entries.push(PackageSymbolIndexEntry {
            id: format!("symbol:dependency:{dependency}"),
            kind: "dependency".to_string(),
            name: dependency.clone(),
            source_import: import.import.clone(),
            dependency: dependency.clone(),
        });
        if let (Some(root), Some(manifest)) = (package_root, manifest) {
            let exports = crate::package_extern::export_symbols_for_resolved_import(
                import, root, manifest, lock,
            );
            for export in exports {
                entries.push(PackageSymbolIndexEntry {
                    id: format!("symbol:export:{dependency}:{export}"),
                    kind: "export".to_string(),
                    name: export,
                    source_import: import.import.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
    }
    entries
}

pub fn diagnostics_for_semantic_imports(
    imports: &[PackageSemanticImport],
) -> Vec<PackageDiagnostic> {
    imports
        .iter()
        .filter(|import| import.status != "resolved")
        .map(|import| PackageDiagnostic {
            code: "INPKG001".to_string(),
            severity: "warning".to_string(),
            import: import.import.clone(),
            reason: import.reason.clone(),
            message: format!(
                "semantic import `{}` is not declared in the nearest inauguration.package",
                import.import
            ),
        })
        .collect()
}

pub fn source_identity_for_path(
    source_path: &Path,
    manifest_name: Option<&str>,
) -> PackageSourceIdentity {
    if source_path.extension().and_then(|ext| ext.to_str()) != Some("in") {
        return PackageSourceIdentity {
            package: None,
            module: None,
            manifest_name: manifest_name.map(str::to_string),
            status: "not_in_source".to_string(),
            reason: "source-not-inlang".to_string(),
        };
    }

    let source = match fs::read_to_string(source_path) {
        Ok(source) => source,
        Err(e) => {
            return PackageSourceIdentity {
                package: None,
                module: None,
                manifest_name: manifest_name.map(str::to_string),
                status: "unavailable".to_string(),
                reason: format!("source-read-failed: {e}"),
            };
        }
    };
    let surface = match crate::in_lang_parse::parse_in_surface_info(&source) {
        Ok(surface) => surface,
        Err(e) => {
            return PackageSourceIdentity {
                package: None,
                module: None,
                manifest_name: manifest_name.map(str::to_string),
                status: "unavailable".to_string(),
                reason: format!("surface-parse-failed: {e}"),
            };
        }
    };
    source_identity_for_surface(surface.package, surface.module, manifest_name)
}

pub fn source_identity_for_surface(
    package: Option<String>,
    module: Option<String>,
    manifest_name: Option<&str>,
) -> PackageSourceIdentity {
    let manifest_name = manifest_name.map(str::to_string);
    let (status, reason) = match (
        package.as_deref(),
        module.as_deref(),
        manifest_name.as_deref(),
    ) {
        (None, None, _) => ("not_declared", "source-identity-not-declared"),
        (_, _, None) => ("missing_manifest", "package-manifest-missing"),
        (Some(package), _, Some(manifest)) if package != manifest => {
            ("mismatch", "package-mismatch")
        }
        (Some(package), Some(module), Some(_))
            if module != package && !module.starts_with(&format!("{package}.")) =>
        {
            ("mismatch", "module-outside-package")
        }
        (None, Some(module), Some(manifest))
            if module != manifest && !module.starts_with(&format!("{manifest}.")) =>
        {
            ("mismatch", "module-outside-package")
        }
        _ => ("match", "package-module-match"),
    };
    PackageSourceIdentity {
        package,
        module,
        manifest_name,
        status: status.to_string(),
        reason: reason.to_string(),
    }
}

pub fn package_graph_report(manifest: &PackageManifest) -> PackageGraphReport {
    let package_id = format!("package:{}", manifest.name);
    let mut nodes = vec![PackageGraphNode {
        id: package_id.clone(),
        kind: "package".to_string(),
        label: format!("{}@{}", manifest.name, manifest.version),
    }];
    let mut edges = Vec::new();

    for target in manifest.targets.keys() {
        let node_id = format!("target:{target}");
        nodes.push(PackageGraphNode {
            id: node_id.clone(),
            kind: "target".to_string(),
            label: target.clone(),
        });
        edges.push(PackageGraphEdge {
            from: package_id.clone(),
            to: node_id,
            kind: "targets".to_string(),
        });
    }

    for dependency in manifest.dependencies.keys() {
        let node_id = format!("dependency:{dependency}");
        nodes.push(PackageGraphNode {
            id: node_id.clone(),
            kind: "dependency".to_string(),
            label: dependency.clone(),
        });
        edges.push(PackageGraphEdge {
            from: package_id.clone(),
            to: node_id,
            kind: "depends-on".to_string(),
        });
    }

    for capability in &manifest.capabilities {
        let node_id = format!("capability:{capability}");
        nodes.push(PackageGraphNode {
            id: node_id.clone(),
            kind: "capability".to_string(),
            label: capability.clone(),
        });
        edges.push(PackageGraphEdge {
            from: package_id.clone(),
            to: node_id,
            kind: "allows-capability".to_string(),
        });
    }

    for extension in &manifest.extensions {
        let node_id = format!("extension:{extension}");
        nodes.push(PackageGraphNode {
            id: node_id.clone(),
            kind: "extension".to_string(),
            label: extension.clone(),
        });
        edges.push(PackageGraphEdge {
            from: package_id.clone(),
            to: node_id,
            kind: "uses-extension".to_string(),
        });
    }

    PackageGraphReport {
        package_id,
        nodes,
        edges,
    }
}

impl PackageManifest {
    pub fn enabled_targets(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|(_, enabled)| **enabled)
            .map(|(target, _)| target.clone())
            .collect()
    }

    pub fn disabled_targets(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|(_, enabled)| !**enabled)
            .map(|(target, _)| target.clone())
            .collect()
    }

    pub fn target_enabled(&self, target: &str) -> bool {
        self.targets.get(target).copied().unwrap_or(false)
    }

    pub fn select_targets<I, S>(&self, requested_targets: I) -> PackageTargetSelection
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let requested = unique_strings(requested_targets);
        if requested.is_empty() {
            return PackageTargetSelection {
                requested,
                enabled: self.enabled_targets(),
                disabled: Vec::new(),
                unknown: Vec::new(),
            };
        }

        let mut enabled = Vec::new();
        let mut disabled = Vec::new();
        let mut unknown = Vec::new();
        for target in &requested {
            match self.targets.get(target.as_str()) {
                Some(true) => enabled.push(target.clone()),
                Some(false) => disabled.push(target.clone()),
                None => unknown.push(target.clone()),
            }
        }

        PackageTargetSelection {
            requested,
            enabled,
            disabled,
            unknown,
        }
    }

    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }

    pub fn validate_capability_policy<I, S>(
        &self,
        required_capabilities: I,
    ) -> CapabilityPolicyValidation
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = unique_strings(self.capabilities.iter().map(String::as_str));
        let required = unique_strings(required_capabilities);
        let allowed_set: BTreeSet<&str> = allowed.iter().map(String::as_str).collect();
        let missing = required
            .iter()
            .filter(|capability| !allowed_set.contains(capability.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        CapabilityPolicyValidation {
            valid: missing.is_empty(),
            allowed,
            required,
            missing,
        }
    }
}

fn unique_strings<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for item in items {
        let item = item.as_ref().trim();
        if !item.is_empty() && seen.insert(item.to_string()) {
            unique.push(item.to_string());
        }
    }
    unique
}

fn parse_package_manifest(source: &str) -> Result<PackageManifest, String> {
    let mut manifest = PackageManifest {
        name: String::new(),
        version: String::new(),
        entry: None,
        targets: BTreeMap::new(),
        dependencies: BTreeMap::new(),
        capabilities: Vec::new(),
        extensions: Vec::new(),
    };
    let mut section = None;
    let mut dependency_name: Option<String> = None;
    let mut dependency_subsection: Option<DependencySubsection> = None;

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        process_manifest_line(
            raw_line,
            line_number,
            &mut manifest,
            &mut section,
            &mut dependency_name,
            &mut dependency_subsection,
        )?;
    }

    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn process_manifest_line(
    raw_line: &str,
    line_number: usize,
    manifest: &mut PackageManifest,
    section: &mut Option<Section>,
    dependency_name: &mut Option<String>,
    dependency_subsection: &mut Option<DependencySubsection>,
) -> Result<(), String> {
    if raw_line.trim().is_empty() {
        return Ok(());
    }
    if raw_line.contains('\t') {
        return Err(format!(
            "line {line_number}: tabs are not valid indentation in inauguration.package"
        ));
    }

    let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
    let line = raw_line.trim_start_matches(' ');

    match indent {
        0 => {
            *dependency_name = None;
            *dependency_subsection = None;
            *section = parse_top_level(line, line_number, manifest)?;
        }
        2 => match *section {
            Some(Section::Targets) => {
                *dependency_name = None;
                *dependency_subsection = None;
                parse_target(line, line_number, manifest)?;
            }
            Some(Section::Dependencies) => {
                *dependency_subsection = None;
                *dependency_name = Some(parse_dependency_header(line, line_number, manifest)?);
            }
            Some(Section::Capabilities) => {
                *dependency_name = None;
                *dependency_subsection = None;
                parse_list_item(
                    line,
                    line_number,
                    "capabilities",
                    &mut manifest.capabilities,
                )?;
            }
            Some(Section::Extensions) => {
                *dependency_name = None;
                *dependency_subsection = None;
                parse_list_item(line, line_number, "extensions", &mut manifest.extensions)?;
            }
            None => {
                return Err(format!(
                    "line {line_number}: indentation is only valid inside a section"
                ));
            }
        },
        4 => {
            if *section != Some(Section::Dependencies) {
                return Err(format!(
                    "line {line_number}: indentation is only valid for dependency metadata"
                ));
            }
            let name = dependency_name.as_deref().ok_or_else(|| {
                format!("line {line_number}: dependency metadata requires a dependency name")
            })?;
            *dependency_subsection = parse_dependency_field(line, line_number, name, manifest)?;
        }
        6 => {
            if *section != Some(Section::Dependencies) {
                return Err(format!(
                    "line {line_number}: indentation is only valid for dependency metadata"
                ));
            }
            let name = dependency_name.as_deref().ok_or_else(|| {
                format!("line {line_number}: dependency metadata requires a dependency name")
            })?;
            parse_dependency_nested_field(
                line,
                line_number,
                name,
                *dependency_subsection,
                manifest,
            )?;
        }
        _ => {
            return Err(format!(
                "line {line_number}: malformed indentation; use 0, 2, 4, or 6 spaces"
            ));
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &PackageManifest) -> Result<(), String> {
    if manifest.name.is_empty() {
        return Err("missing required field `name`".into());
    }
    if manifest.version.is_empty() {
        return Err("missing required field `version`".into());
    }
    for (name, dependency) in &manifest.dependencies {
        if dependency.version.is_empty() {
            return Err(format!(
                "dependency `{name}` is missing required field `version`"
            ));
        }
    }
    Ok(())
}

fn parse_top_level(
    line: &str,
    line_number: usize,
    manifest: &mut PackageManifest,
) -> Result<Option<Section>, String> {
    let (key, value) = split_field(line, line_number)?;
    match key {
        "name" => {
            manifest.name = required_scalar(value, line_number, "name")?.to_string();
            Ok(None)
        }
        "version" => {
            manifest.version = required_scalar(value, line_number, "version")?.to_string();
            Ok(None)
        }
        "entry" => {
            manifest.entry = Some(required_scalar(value, line_number, "entry")?.to_string());
            Ok(None)
        }
        "targets" => parse_section_header(value, line_number, "targets", Section::Targets),
        "dependencies" => {
            parse_section_header(value, line_number, "dependencies", Section::Dependencies)
        }
        "capabilities" => {
            parse_section_header(value, line_number, "capabilities", Section::Capabilities)
        }
        "extensions" => parse_section_header(value, line_number, "extensions", Section::Extensions),
        other => Err(format!(
            "line {line_number}: unknown top-level field `{other}`"
        )),
    }
}

fn parse_section_header(
    value: &str,
    line_number: usize,
    name: &str,
    section: Section,
) -> Result<Option<Section>, String> {
    if value.is_empty() {
        Ok(Some(section))
    } else {
        Err(format!(
            "line {line_number}: section `{name}` must not have an inline value"
        ))
    }
}

fn parse_target(
    line: &str,
    line_number: usize,
    manifest: &mut PackageManifest,
) -> Result<(), String> {
    let (key, value) = split_field(line, line_number)?;
    let enabled = match required_scalar(value, line_number, key)? {
        "true" => true,
        "false" => false,
        other => {
            return Err(format!(
                "line {line_number}: target `{key}` expects true or false, got `{other}`"
            ));
        }
    };
    if manifest.targets.insert(key.to_string(), enabled).is_some() {
        return Err(format!("line {line_number}: duplicate target `{key}`"));
    }
    Ok(())
}

fn parse_dependency_header(
    line: &str,
    line_number: usize,
    manifest: &mut PackageManifest,
) -> Result<String, String> {
    let key = crate::package_ref::split_dependency_header(line, line_number)?;
    if manifest
        .dependencies
        .insert(key.clone(), PackageDependency::default())
        .is_some()
    {
        return Err(format!("line {line_number}: duplicate dependency `{key}`"));
    }
    Ok(key)
}

fn parse_dependency_field(
    line: &str,
    line_number: usize,
    dependency_name: &str,
    manifest: &mut PackageManifest,
) -> Result<Option<DependencySubsection>, String> {
    let (key, value) = split_field(line, line_number)?;
    let dependency = manifest
        .dependencies
        .get_mut(dependency_name)
        .ok_or_else(|| format!("line {line_number}: unknown dependency `{dependency_name}`"))?;
    match key {
        "version" => {
            let version = required_scalar(value, line_number, "version")?;
            if !dependency.version.is_empty() {
                return Err(format!(
                    "line {line_number}: duplicate version for dependency `{dependency_name}`"
                ));
            }
            dependency.version = version.to_string();
            Ok(None)
        }
        "kind" => {
            let kind = required_scalar(value, line_number, "kind")?;
            if dependency.kind.is_some() {
                return Err(format!(
                    "line {line_number}: duplicate kind for dependency `{dependency_name}`"
                ));
            }
            dependency.kind = Some(kind.to_string());
            Ok(None)
        }
        "source" => {
            let source = required_scalar(value, line_number, "source")?;
            if dependency.source.is_some() {
                return Err(format!(
                    "line {line_number}: duplicate source for dependency `{dependency_name}`"
                ));
            }
            dependency.source = Some(source.to_string());
            Ok(None)
        }
        "rev" => {
            let rev = required_scalar(value, line_number, "rev")?;
            if dependency.rev.is_some() {
                return Err(format!(
                    "line {line_number}: duplicate rev for dependency `{dependency_name}`"
                ));
            }
            dependency.rev = Some(rev.to_string());
            Ok(None)
        }
        "checksum" => {
            let checksum = required_scalar(value, line_number, "checksum")?;
            if dependency.checksum.is_some() {
                return Err(format!(
                    "line {line_number}: duplicate checksum for dependency `{dependency_name}`"
                ));
            }
            dependency.checksum = Some(checksum.to_string());
            Ok(None)
        }
        "install_path" => {
            let install_path = required_scalar(value, line_number, "install_path")?;
            if dependency.install_path.is_some() {
                return Err(format!(
                    "line {line_number}: duplicate install_path for dependency `{dependency_name}`"
                ));
            }
            dependency.install_path = Some(install_path.to_string());
            Ok(None)
        }
        "targets" => parse_dependency_subsection_header(
            value,
            line_number,
            "targets",
            DependencySubsection::Targets,
        ),
        "capabilities" => parse_dependency_subsection_header(
            value,
            line_number,
            "capabilities",
            DependencySubsection::Capabilities,
        ),
        "build" => parse_dependency_subsection_header(
            value,
            line_number,
            "build",
            DependencySubsection::Build,
        ),
        other => Err(format!(
            "line {line_number}: unknown dependency field `{other}` for `{dependency_name}`"
        )),
    }
}

fn parse_dependency_subsection_header(
    value: &str,
    line_number: usize,
    name: &str,
    subsection: DependencySubsection,
) -> Result<Option<DependencySubsection>, String> {
    if value.is_empty() {
        Ok(Some(subsection))
    } else {
        Err(format!(
            "line {line_number}: dependency subsection `{name}` must not have an inline value"
        ))
    }
}

fn parse_dependency_nested_field(
    line: &str,
    line_number: usize,
    dependency_name: &str,
    subsection: Option<DependencySubsection>,
    manifest: &mut PackageManifest,
) -> Result<(), String> {
    let subsection = subsection.ok_or_else(|| {
        format!("line {line_number}: nested dependency metadata requires a subsection")
    })?;
    let dependency = manifest
        .dependencies
        .get_mut(dependency_name)
        .ok_or_else(|| format!("line {line_number}: unknown dependency `{dependency_name}`"))?;
    match subsection {
        DependencySubsection::Targets => parse_list_item(
            line,
            line_number,
            "dependency targets",
            &mut dependency.targets,
        ),
        DependencySubsection::Capabilities => parse_list_item(
            line,
            line_number,
            "dependency capabilities",
            &mut dependency.capabilities,
        ),
        DependencySubsection::Build => {
            let (key, value) = split_field(line, line_number)?;
            let value = required_scalar(value, line_number, key)?;
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

fn parse_list_item(
    line: &str,
    line_number: usize,
    section: &str,
    values: &mut Vec<String>,
) -> Result<(), String> {
    let Some(value) = line.strip_prefix("- ") else {
        return Err(format!(
            "line {line_number}: section `{section}` only supports list items"
        ));
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "line {line_number}: section `{section}` contains an empty list item"
        ));
    }
    values.push(value.to_string());
    Ok(())
}

fn split_field(line: &str, line_number: usize) -> Result<(&str, &str), String> {
    let Some((key, value)) = line.split_once(':') else {
        return Err(format!("line {line_number}: expected `key: value`"));
    };
    let key = key.trim();
    if key.is_empty() {
        return Err(format!("line {line_number}: empty key"));
    }
    Ok((key, value.trim()))
}

fn required_scalar<'a>(value: &'a str, line_number: usize, field: &str) -> Result<&'a str, String> {
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
                "inauguration-package-manifest-{}-{}-{}",
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

    fn parse_text(source: &str) -> Result<PackageManifest, String> {
        let temp = TempDirGuard::new();
        let manifest_path = temp.path.join("inauguration.package");
        fs::write(&manifest_path, source).expect("write manifest");
        load_package_manifest(&manifest_path)
    }

    #[test]
    fn test_package_target_selection_target_enabled() {
        // Target is present in `disabled` (returns `false`)
        let selection1 = PackageTargetSelection {
            requested: vec![],
            enabled: vec!["macos".to_string()],
            disabled: vec!["linux".to_string()],
            unknown: vec![],
        };
        assert!(!selection1.target_enabled("linux"));

        // Target is not in `disabled` and `enabled` is empty (returns `true`)
        let selection2 = PackageTargetSelection {
            requested: vec![],
            enabled: vec![],
            disabled: vec!["windows".to_string()],
            unknown: vec![],
        };
        assert!(selection2.target_enabled("linux"));

        // Target is not in `disabled`, `enabled` is non-empty, and target is in `enabled` (returns `true`)
        let selection3 = PackageTargetSelection {
            requested: vec![],
            enabled: vec!["linux".to_string(), "macos".to_string()],
            disabled: vec!["windows".to_string()],
            unknown: vec![],
        };
        assert!(selection3.target_enabled("linux"));

        // Target is not in `disabled`, `enabled` is non-empty, and target is not in `enabled` (returns `false`)
        let selection4 = PackageTargetSelection {
            requested: vec![],
            enabled: vec!["macos".to_string()],
            disabled: vec!["windows".to_string()],
            unknown: vec![],
        };
        assert!(!selection4.target_enabled("linux"));
    }

    #[test]
    fn test_parse_package_manifest_source_comprehensive() {
        let manifest = parse_package_manifest_source(
            r#"name: comprehensive_test
version: 1.0.0
entry: src/main.in
targets:
  macos: true
  linux: false
dependencies:
  dep1:
    version: ^1.2.3
    targets:
      - macos
  dep2:
    version: latest
capabilities:
  - network
  - filesystem.read
extensions:
  - distributed-workers
  - preview-host
"#,
        )
        .expect("parse manifest source comprehensively");

        assert_eq!(manifest.name, "comprehensive_test");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.entry, Some("src/main.in".to_string()));
        assert_eq!(manifest.targets.get("macos").copied(), Some(true));
        assert_eq!(manifest.targets.get("linux").copied(), Some(false));
        let dep1 = manifest.dependencies.get("dep1").expect("should have dep1");
        assert_eq!(dep1.version, "^1.2.3");
        assert_eq!(dep1.targets, vec!["macos".to_string()]);
        let dep2 = manifest.dependencies.get("dep2").expect("should have dep2");
        assert_eq!(dep2.version, "latest");
        assert_eq!(
            manifest.capabilities,
            vec!["network".to_string(), "filesystem.read".to_string()]
        );
        assert_eq!(
            manifest.extensions,
            vec![
                "distributed-workers".to_string(),
                "preview-host".to_string()
            ]
        );
    }

    #[test]
    fn test_parse_package_manifest_source_error() {
        let result = parse_package_manifest_source("name: error_test\n\tversion: 1.0.0");
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("tabs are not valid indentation"),
            "Unexpected error: {err_msg}"
        );
    }

    #[test]
    fn parses_package_manifest_source_directly() {
        let manifest = parse_package_manifest_source(
            r#"name: direct_test
version: 0.2.0
targets:
  macos: true
"#,
        )
        .expect("parse manifest source directly");

        assert_eq!(manifest.name, "direct_test");
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.targets.get("macos").copied(), Some(true));
    }

    #[test]
    fn loads_package_manifest_from_file() {
        let manifest = parse_text(
            r#"name: hyperchat
version: 0.1.0
targets:
  linux: true
  macos: true
  web: true
dependencies:
  postgres:
    version: ^1.0.0
  redis:
    version: latest
capabilities:
  - filesystem.read
  - filesystem.write
  - network.http
extensions:
  - postgres-driver
  - distributed-workers
  - gpu-optimizer
"#,
        )
        .expect("parse package manifest");

        assert_eq!(manifest.name, "hyperchat");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.targets.get("linux"), Some(&true));
        assert_eq!(manifest.targets.get("macos"), Some(&true));
        assert_eq!(manifest.targets.get("web"), Some(&true));
        assert_eq!(
            manifest.dependencies.get("postgres"),
            Some(&PackageDependency {
                version: "^1.0.0".into(),
                ..Default::default()
            })
        );
        assert_eq!(
            manifest.dependencies.get("redis"),
            Some(&PackageDependency {
                version: "latest".into(),
                ..Default::default()
            })
        );
        assert_eq!(
            manifest.capabilities,
            vec![
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
                "network.http".to_string()
            ]
        );
        assert_eq!(
            manifest.extensions,
            vec![
                "postgres-driver".to_string(),
                "distributed-workers".to_string(),
                "gpu-optimizer".to_string()
            ]
        );
    }

    #[test]
    fn loads_package_manifest_from_directory() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("inauguration.package"),
            r#"name: sample
version: 1.2.3
"#,
        )
        .expect("write manifest");

        let manifest = load_package_manifest(&temp.path).expect("load manifest from directory");

        assert_eq!(manifest.name, "sample");
        assert_eq!(manifest.version, "1.2.3");
        assert!(manifest.targets.is_empty());
        assert!(manifest.dependencies.is_empty());
        assert!(manifest.capabilities.is_empty());
        assert!(manifest.extensions.is_empty());
    }

    #[test]
    fn loads_optional_package_entry() {
        let manifest = parse_text(
            r#"name: entrypoint
version: 0.1.0
entry: start
"#,
        )
        .expect("parse manifest");

        assert_eq!(manifest.entry.as_deref(), Some("start"));
    }

    #[test]
    fn loads_extended_dependency_metadata() {
        let manifest = parse_text(
            r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
    kind: registry
    source: https://registry.inauguration.dev
    rev: main
    checksum: sha256:abc123
    targets:
      - macos
      - linux
    capabilities:
      - network.http
    build:
      profile: release
      features: gpu
"#,
        )
        .expect("parse extended dependency metadata");

        let postgres = manifest
            .dependencies
            .get("postgres")
            .expect("postgres dependency");
        assert_eq!(postgres.version, "^1.0.0");
        assert_eq!(postgres.kind.as_deref(), Some("registry"));
        assert_eq!(
            postgres.source.as_deref(),
            Some("https://registry.inauguration.dev")
        );
        assert_eq!(postgres.rev.as_deref(), Some("main"));
        assert_eq!(postgres.checksum.as_deref(), Some("sha256:abc123"));
        assert_eq!(
            postgres.targets,
            vec!["macos".to_string(), "linux".to_string()]
        );
        assert_eq!(postgres.capabilities, vec!["network.http".to_string()]);
        assert_eq!(
            postgres.build.get("profile").map(String::as_str),
            Some("release")
        );
        assert_eq!(
            postgres.build.get("features").map(String::as_str),
            Some("gpu")
        );
    }

    #[test]
    fn resolves_path_dependency_from_kind_and_source() {
        let dependency = PackageDependency {
            version: "0.1.0".into(),
            kind: Some("path".into()),
            source: Some("../local".into()),
            ..Default::default()
        };

        assert_eq!(
            dependency.resolved_source_path(),
            Some(PathBuf::from("../local"))
        );
    }

    #[test]
    fn discovers_compile_context_from_manifest() {
        let temp = TempDirGuard::new();
        fs::create_dir_all(temp.path.join("vendor/local")).expect("vendor dir");
        fs::write(
            temp.path.join("inauguration.package"),
            r#"name: compile-context
version: 0.1.0
entry: boot
dependencies:
  local:
    version: path:vendor/local
"#,
        )
        .expect("write manifest");

        let context = compile_context_in_dir(&temp.path).expect("discover package compile context");

        assert_eq!(context.name, "compile-context");
        assert_eq!(context.entry.as_deref(), Some("boot"));
        assert_eq!(context.dependency_search_paths.len(), 1);
        assert!(context.dependency_search_paths[0].ends_with("vendor/local"));
    }

    #[test]
    fn rejects_bad_indentation() {
        let err = parse_text(
            r#"name: bad
version: 0.1.0
targets:
 linux: true
"#,
        )
        .expect_err("reject malformed indentation");

        assert!(err.contains("line 4"), "{err}");
        assert!(err.contains("indentation"), "{err}");
    }

    #[test]
    fn rejects_unknown_section_shape() {
        let err = parse_text(
            r#"name: bad
version: 0.1.0
capabilities:
  filesystem.read: true
"#,
        )
        .expect_err("reject map-shaped capabilities section");

        assert!(err.contains("line 4"), "{err}");
        assert!(err.contains("capabilities"), "{err}");
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let err = parse_text(
            r#"name: bad
version: 0.1.0
scripts:
  test: in test
"#,
        )
        .expect_err("reject unknown top-level field");

        assert!(err.contains("line 3"), "{err}");
        assert!(err.contains("scripts"), "{err}");
    }

    #[test]
    fn discovers_package_root_from_nested_source_path() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("inauguration.package"),
            r#"name: rooted
version: 0.1.0
"#,
        )
        .expect("write manifest");
        let source_path = temp.path.join("Sources").join("App").join("main.in");
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("create sources");
        fs::write(&source_path, "fn main() -> void { return; }\n").expect("write source");

        let root = discover_package_root(&source_path).expect("discover package root");

        assert_eq!(root.root, temp.path);
        assert_eq!(root.manifest_path, temp.path.join("inauguration.package"));
    }

    #[test]
    fn selects_enabled_disabled_and_unknown_targets() {
        let manifest = parse_text(
            r#"name: targets
version: 0.1.0
targets:
  macos: true
  linux: false
  web: true
"#,
        )
        .expect("parse manifest");

        let selection = manifest.select_targets(["web", "linux", "ios"]);

        assert_eq!(manifest.enabled_targets(), vec!["macos", "web"]);
        assert_eq!(selection.enabled, vec!["web"]);
        assert_eq!(selection.disabled, vec!["linux"]);
        assert_eq!(selection.unknown, vec!["ios"]);
    }

    #[test]
    fn validates_required_capabilities_against_package_policy() {
        let manifest = parse_text(
            r#"name: caps
version: 0.1.0
capabilities:
  - fs.read
  - process.stdout
"#,
        )
        .expect("parse manifest");

        let validation = manifest.validate_capability_policy(["process.stdout", "network.http"]);

        assert!(!validation.valid);
        assert_eq!(validation.allowed, vec!["fs.read", "process.stdout"]);
        assert_eq!(validation.required, vec!["process.stdout", "network.http"]);
        assert_eq!(validation.missing, vec!["network.http"]);
    }

    #[test]
    fn validates_all_capabilities_allowed() {
        let manifest = parse_text(
            r#"name: caps
version: 0.1.0
capabilities:
  - fs.read
  - process.stdout
  - network.http
"#,
        )
        .expect("parse manifest");

        let validation = manifest.validate_capability_policy(["process.stdout", "network.http"]);

        assert!(validation.valid);
        assert_eq!(
            validation.allowed,
            vec!["fs.read", "process.stdout", "network.http"]
        );
        assert_eq!(validation.required, vec!["process.stdout", "network.http"]);
        assert_eq!(validation.missing, Vec::<String>::new());
    }

    #[test]
    fn validates_no_required_capabilities() {
        let manifest = parse_text(
            r#"name: caps
version: 0.1.0
capabilities:
  - fs.read
"#,
        )
        .expect("parse manifest");

        let validation = manifest.validate_capability_policy(Vec::<&str>::new());

        assert!(validation.valid);
        assert_eq!(validation.allowed, vec!["fs.read"]);
        assert_eq!(validation.required, Vec::<String>::new());
        assert_eq!(validation.missing, Vec::<String>::new());
    }

    #[test]
    fn validates_no_allowed_capabilities_but_requires_some() {
        let manifest = parse_text(
            r#"name: caps
version: 0.1.0
"#,
        )
        .expect("parse manifest");

        let validation = manifest.validate_capability_policy(["network.http"]);

        assert!(!validation.valid);
        assert_eq!(validation.allowed, Vec::<String>::new());
        assert_eq!(validation.required, vec!["network.http"]);
        assert_eq!(validation.missing, vec!["network.http"]);
    }

    #[test]
    fn validates_capability_policy_with_duplicates() {
        let manifest = parse_text(
            r#"name: caps
version: 0.1.0
capabilities:
  - fs.read
  - fs.read
"#,
        )
        .expect("parse manifest");

        let validation =
            manifest.validate_capability_policy(["fs.read", "fs.read", "network.http"]);

        assert!(!validation.valid);
        assert_eq!(validation.allowed, vec!["fs.read"]);
        assert_eq!(validation.required, vec!["fs.read", "network.http"]);
        assert_eq!(validation.missing, vec!["network.http"]);
    }

    #[test]
    fn builds_package_report_with_graph_nodes_and_edges() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("inauguration.package"),
            r#"name: graphable
version: 0.1.0
targets:
  macos: true
dependencies:
  corelib:
    version: ^1.2.0
capabilities:
  - process.stdout
extensions:
  - preview-host
"#,
        )
        .expect("write manifest");
        let source_path = temp.path.join("src").join("main.in");
        fs::create_dir_all(source_path.parent().expect("source parent"))
            .expect("create source dir");
        fs::write(&source_path, "capability process.stdout;\n").expect("write source");

        let report = load_package_report_from_source(
            &source_path,
            ["macos", "web"],
            ["process.stdout", "fs.read"],
        )
        .expect("load package report");

        assert_eq!(report.root, temp.path);
        assert_eq!(report.manifest.name, "graphable");
        assert_eq!(report.target_selection.enabled, vec!["macos"]);
        assert_eq!(report.target_selection.unknown, vec!["web"]);
        assert_eq!(report.capability_policy.missing, vec!["fs.read"]);
        assert!(
            report
                .graph
                .nodes
                .iter()
                .any(|node| node.id == "package:graphable")
        );
        assert!(
            report
                .graph
                .edges
                .iter()
                .any(|edge| edge.from == "package:graphable" && edge.to == "dependency:corelib")
        );
    }

    #[test]
    fn source_package_report_carries_identity_status() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("inauguration.package"),
            "name: agents.sample\nversion: 0.1.0\n",
        )
        .expect("write manifest");
        let source_path = temp.path.join("main.in");
        fs::write(
            &source_path,
            "package agents.sample;\nmodule agents.sample.main;\nfn main() -> void { return; }\n",
        )
        .expect("write source");

        let report = load_package_report_from_source(
            &source_path,
            std::iter::empty::<&str>(),
            std::iter::empty::<&str>(),
        )
        .expect("load source package report");

        let identity = report.source_identity.expect("source identity");
        assert_eq!(identity.manifest_name.as_deref(), Some("agents.sample"));
        assert_eq!(identity.status, "match");
        assert_eq!(identity.reason, "package-module-match");
    }

    #[test]
    fn source_package_report_carries_semantic_symbol_index_and_diagnostics() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("inauguration.package"),
            r#"name: agents.sample
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
        )
        .expect("write manifest");
        let source_path = temp.path.join("main.in");
        fs::write(
            &source_path,
            "package agents.sample;\nuse database.postgres;\nbind database.postgres as postgres;\nuse cache.redis;\nfn main() -> void { return; }\n",
        )
        .expect("write source");

        let report = load_package_report_from_source(
            &source_path,
            std::iter::empty::<&str>(),
            std::iter::empty::<&str>(),
        )
        .expect("load source package report");

        assert_eq!(report.semantic_bindings.len(), 1);
        assert_eq!(report.semantic_bindings[0].alias, "postgres");
        assert_eq!(report.semantic_bindings[0].status, "resolved");
        assert_eq!(report.symbol_index.len(), 2);
        assert_eq!(report.symbol_index[0].id, "symbol:dependency:postgres");
        assert_eq!(report.symbol_index[0].source_import, "database.postgres");
        assert!(
            report
                .symbol_index
                .iter()
                .any(|entry| entry.id == "symbol:binding:postgres"
                    && entry.kind == "binding"
                    && entry.name == "postgres")
        );
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].code, "INPKG001");
        assert_eq!(report.diagnostics[0].import, "cache.redis");
    }

    #[test]
    fn reports_source_identity_match_and_mismatch() {
        let matching = source_identity_for_surface(
            Some("graphable".into()),
            Some("graphable.main".into()),
            Some("graphable"),
        );
        assert_eq!(matching.status, "match");
        assert_eq!(matching.reason, "package-module-match");

        let package_mismatch = source_identity_for_surface(
            Some("other".into()),
            Some("other.main".into()),
            Some("graphable"),
        );
        assert_eq!(package_mismatch.status, "mismatch");
        assert_eq!(package_mismatch.reason, "package-mismatch");

        let missing_manifest = source_identity_for_surface(Some("graphable".into()), None, None);
        assert_eq!(missing_manifest.status, "missing_manifest");
        assert_eq!(missing_manifest.reason, "package-manifest-missing");

        let module_mismatch =
            source_identity_for_surface(None, Some("other.main".into()), Some("graphable"));
        assert_eq!(module_mismatch.status, "mismatch");
        assert_eq!(module_mismatch.reason, "module-outside-package");
    }

    #[test]
    fn ecosystem_semantic_imports_resolve_against_manifest_dependencies() {
        let manifest = parse_text(
            r#"name: ecosystem-demo
version: 0.1.0
dependencies:
  cargo:crepuscularity:
    version: path:vendor/cargo/crepuscularity
    kind: cargo
  npm:hono:
    version: path:vendor/npm/hono
    kind: npm
"#,
        )
        .expect("parse manifest");

        let imports = resolve_semantic_imports(
            &[
                "cargo:crepuscularity".to_string(),
                "npm:hono".to_string(),
                "cargo:missing".to_string(),
            ],
            Some(&manifest),
        );
        assert_eq!(imports[0].status, "resolved");
        assert_eq!(
            imports[0].dependency.as_deref(),
            Some("cargo:crepuscularity")
        );
        assert_eq!(imports[0].reason, "dependency-ecosystem-exact-match");
        assert_eq!(imports[1].status, "resolved");
        assert_eq!(imports[1].dependency.as_deref(), Some("npm:hono"));
        assert_eq!(imports[2].status, "unresolved");
    }

    #[test]
    fn semantic_imports_resolve_against_manifest_dependencies() {
        let manifest = parse_text(
            r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
        )
        .expect("parse manifest");

        let imports = resolve_semantic_imports(
            &["database.postgres".to_string(), "cache.redis".to_string()],
            Some(&manifest),
        );

        assert_eq!(imports[0].status, "resolved");
        assert_eq!(imports[0].dependency.as_deref(), Some("postgres"));
        assert_eq!(imports[0].reason, "dependency-suffix-match");
        assert_eq!(imports[1].status, "unresolved");
        assert_eq!(imports[1].reason, "dependency-not-declared");

        let symbols = symbol_index_for_semantic_imports(&imports);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].id, "symbol:dependency:postgres");
        assert_eq!(symbols[0].source_import, "database.postgres");

        let diagnostics = diagnostics_for_semantic_imports(&imports);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "INPKG001");
        assert_eq!(diagnostics[0].severity, "warning");
        assert_eq!(diagnostics[0].import, "cache.redis");
    }

    #[test]
    fn test_diagnostics_for_missing_dependencies_format_error() {
        let imports = vec![
            PackageSemanticImport {
                import: "foo.bar".to_string(),
                dependency: None,
                status: "resolved".to_string(),
                reason: "core".to_string(),
            },
            PackageSemanticImport {
                import: "some.missing.dep".to_string(),
                dependency: None,
                status: "unresolved".to_string(),
                reason: "dependency-not-declared".to_string(),
            },
            PackageSemanticImport {
                import: "another.missing".to_string(),
                dependency: None,
                status: "error".to_string(),
                reason: "parse-failed".to_string(),
            },
        ];

        let diagnostics = diagnostics_for_semantic_imports(&imports);
        assert_eq!(diagnostics.len(), 2);

        let diag1 = &diagnostics[0];
        assert_eq!(diag1.code, "INPKG001");
        assert_eq!(diag1.severity, "warning");
        assert_eq!(diag1.import, "some.missing.dep");
        assert_eq!(diag1.reason, "dependency-not-declared");
        assert_eq!(
            diag1.message,
            "semantic import `some.missing.dep` is not declared in the nearest inauguration.package"
        );

        let diag2 = &diagnostics[1];
        assert_eq!(diag2.code, "INPKG001");
        assert_eq!(diag2.severity, "warning");
        assert_eq!(diag2.import, "another.missing");
        assert_eq!(diag2.reason, "parse-failed");
        assert_eq!(
            diag2.message,
            "semantic import `another.missing` is not declared in the nearest inauguration.package"
        );
    }

    #[test]
    fn test_diagnostics_for_semantic_imports_formatting() {
        let imports = vec![
            PackageSemanticImport {
                import: "std.io".to_string(),
                dependency: None,
                status: "resolved".to_string(),
                reason: "core".to_string(),
            },
            PackageSemanticImport {
                import: "missing.pkg".to_string(),
                dependency: None,
                status: "unresolved".to_string(),
                reason: "dependency-not-declared".to_string(),
            },
        ];

        let diagnostics = diagnostics_for_semantic_imports(&imports);

        assert_eq!(diagnostics.len(), 1);

        let diag = &diagnostics[0];
        assert_eq!(diag.code, "INPKG001");
        assert_eq!(diag.severity, "warning");
        assert_eq!(diag.import, "missing.pkg");
        assert_eq!(diag.reason, "dependency-not-declared");
        assert_eq!(
            diag.message,
            "semantic import `missing.pkg` is not declared in the nearest inauguration.package"
        );
    }

    #[test]
    fn compile_context_at_root_returns_none_if_no_manifest() {
        let temp = TempDirGuard::new();
        assert!(compile_context_at_root(&temp.path).is_none());
    }

    #[test]
    fn compile_context_in_dir_returns_none_if_no_manifest() {
        let temp = TempDirGuard::new();
        assert!(compile_context_in_dir(&temp.path).is_none());
    }

    #[test]
    fn compile_context_for_source_returns_none_if_no_manifest() {
        let temp = TempDirGuard::new();
        let source_file = temp.path.join("main.in");
        fs::write(&source_file, "fn main() -> void {}").expect("write dummy source");
        assert!(compile_context_for_source(&source_file).is_none());
    }

    #[test]
    fn compile_context_at_root_returns_context_if_manifest_exists() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join(PACKAGE_MANIFEST_FILE),
            r#"name: test_pkg
version: 0.1.0
"#,
        )
        .expect("write manifest");
        let context = compile_context_at_root(&temp.path).expect("load context");
        assert_eq!(context.name, "test_pkg");
        assert_eq!(context.entry, None);
        assert_eq!(context.dependency_search_paths.len(), 0);
    }

    #[test]
    fn compile_context_honors_legacy_target_in_packages_install_path() {
        let temp = TempDirGuard::new();
        let install_rel = "target/in/packages/npm/hono/4.0.0";
        fs::create_dir_all(temp.path.join(install_rel)).expect("legacy install tree");
        fs::write(
            temp.path.join(PACKAGE_MANIFEST_FILE),
            "name: legacy\nversion: 0.1.0\ndependencies:\n  hono:\n    version: 4.0.0\n",
        )
        .expect("write manifest");
        fs::write(
            temp.path.join(crate::package_lock::PACKAGE_LOCK_FILE),
            format!(
                "lock-version: 1\nname: legacy\nversion: 0.1.0\ndependencies:\n  hono:\n    version: 4.0.0\n    install_path: {install_rel}\n"
            ),
        )
        .expect("write lock");
        let context = compile_context_at_root(&temp.path).expect("load context");
        assert_eq!(context.dependency_search_paths.len(), 1);
        assert!(context.dependency_search_paths[0].ends_with("hono/4.0.0"));
    }

    #[test]
    fn compile_context_at_root_returns_context_with_entry() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join(PACKAGE_MANIFEST_FILE),
            r#"name: test_pkg_entry
version: 0.1.0
entry: src/main.in
"#,
        )
        .expect("write manifest");
        let context = compile_context_at_root(&temp.path).expect("load context");
        assert_eq!(context.name, "test_pkg_entry");
        assert_eq!(context.entry.as_deref(), Some("src/main.in"));
        assert_eq!(context.dependency_search_paths.len(), 0);
    }
}
