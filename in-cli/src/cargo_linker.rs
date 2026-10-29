use crate::boundary_ir::{CompileArtifact, merge_unary_method_bindings};
use crate::compiler::rust_front;
use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Clone)]
pub struct CargoManifest {
    pub package_name: Option<String>,
    pub lib_path: Option<String>,
}

impl CargoManifest {
    pub fn parse(toml: &str) -> Self {
        let mut manifest = CargoManifest::default();
        let mut in_package = false;
        let mut in_lib = false;

        for line in toml.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if t.starts_with('[') {
                in_package = t == "[package]";
                in_lib = t == "[lib]";
                continue;
            }
            if in_package && t.starts_with("name") {
                if let Some(idx) = t.find('=') {
                    let val = t[idx + 1..].trim();
                    let val = val.strip_prefix('"').unwrap_or(val);
                    let val = val.strip_suffix('"').unwrap_or(val);
                    manifest.package_name = Some(val.to_string());
                }
            } else if in_lib && t.starts_with("path") {
                if let Some(idx) = t.find('=') {
                    let val = t[idx + 1..].trim();
                    let val = val.strip_prefix('"').unwrap_or(val);
                    let val = val.strip_suffix('"').unwrap_or(val);
                    manifest.lib_path = Some(val.to_string());
                }
            }
        }
        manifest
    }
}

/// Read current package and library-path declarations; source edits must be
/// visible immediately to the long-lived compiler process.
pub fn get_manifest(crate_dir: &Path) -> Option<Arc<CargoManifest>> {
    let content = std::fs::read_to_string(crate_dir.join("Cargo.toml")).ok()?;
    Some(Arc::new(CargoManifest::parse(&content)))
}

/// Cached cargo metadata (avoids re-running `cargo metadata` every compile).
static METADATA_CACHE: std::sync::LazyLock<
    Mutex<HashMap<PathBuf, (Instant, Arc<serde_json::Value>)>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

const METADATA_CACHE_TTL: Duration = Duration::from_secs(300); // 5 min

fn get_cargo_metadata(project_dir: &Path) -> Option<Arc<serde_json::Value>> {
    let key = project_dir.to_path_buf();

    if let Ok(cache) = METADATA_CACHE.lock() {
        if let Some((timestamp, value)) = cache.get(&key) {
            if timestamp.elapsed() < METADATA_CACHE_TTL {
                return Some(Arc::clone(value));
            }
        }
    }

    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .current_dir(project_dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let metadata: Arc<serde_json::Value> = Arc::new(serde_json::from_slice(&output.stdout).ok()?);

    if let Ok(mut cache) = METADATA_CACHE.lock() {
        cache.insert(key, (Instant::now(), Arc::clone(&metadata)));
    }

    Some(metadata)
}

/// Resolve cargo dependencies for a Rust project and compile their lib.rs files.
/// Collects ALL transitive dependencies from the resolve graph.
/// Returns a Vec of (crate_name, UnifiedModule) for all successfully-compiled dependencies.
pub fn compile_cargo_dependencies(project_dir: &Path) -> Vec<(String, UnifiedModule)> {
    compile_cargo_dependency_artifacts(project_dir)
        .into_iter()
        .map(|(name, artifact)| (name, artifact.into_semantic()))
        .collect()
}

pub fn compile_cargo_dependency_artifacts(project_dir: &Path) -> Vec<(String, CompileArtifact)> {
    compile_cargo_dependency_artifacts_for_target(project_dir, None)
}

pub(crate) fn compile_cargo_dependency_artifacts_for_target(
    project_dir: &Path,
    target: Option<&str>,
) -> Vec<(String, CompileArtifact)> {
    let mut modules = Vec::new();

    let metadata = match get_cargo_metadata(project_dir) {
        Some(m) => m,
        None => return modules,
    };

    let packages = match metadata["packages"].as_array() {
        Some(pkgs) => pkgs,
        None => return modules,
    };

    let resolve = &metadata["resolve"];
    let root_id = resolve["root"].as_str().unwrap_or("");

    let (pkg_manifest, pkg_by_id) = build_pkg_maps(packages);

    let nodes = match resolve["nodes"].as_array() {
        Some(n) => n,
        None => return modules,
    };

    let all_dep_ids = collect_transitive_dependencies(root_id, nodes);
    let target_cfg = rust_target_cfg(target);
    compile_resolved_dependencies(
        &all_dep_ids,
        nodes,
        &pkg_manifest,
        &pkg_by_id,
        target_cfg.as_deref(),
        &mut modules,
    );

    modules
}

pub(crate) fn parse_crate_root_artifact(
    path: &Path,
    project_dir: &Path,
    target: Option<&str>,
) -> Result<CompileArtifact, String> {
    let Some(metadata) = get_cargo_metadata(project_dir) else {
        return rust_front::parse_rust_artifact(path);
    };
    let root = metadata["resolve"]["root"].as_str().unwrap_or("");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    rust_front::parse_rust_artifact_with_cargo_cfg(
        path,
        &resolved_features(nodes, root),
        rust_target_cfg(target).as_deref(),
    )
}

fn resolved_features(nodes: &[serde_json::Value], package: &str) -> Vec<String> {
    nodes
        .iter()
        .find(|node| node["id"].as_str() == Some(package))
        .and_then(|node| node["features"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|feature| feature.as_str().map(str::to_owned))
        .collect()
}

fn rust_target_cfg(target: Option<&str>) -> Option<Vec<String>> {
    let mut command = Command::new("rustc");
    command.args(["--print", "cfg"]);
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    let output = command.output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    })
}

fn build_pkg_maps(
    packages: &[serde_json::Value],
) -> (HashMap<&str, PathBuf>, HashMap<&str, &serde_json::Value>) {
    let mut pkg_manifest = HashMap::new();
    let mut pkg_by_id = HashMap::new();
    for pkg in packages {
        let id = pkg["id"].as_str().unwrap_or("");
        let manifest = pkg["manifest_path"].as_str().unwrap_or("");
        if !manifest.is_empty() {
            pkg_manifest.insert(id, PathBuf::from(manifest));
        }
        pkg_by_id.insert(id, pkg);
    }
    (pkg_manifest, pkg_by_id)
}

fn collect_transitive_dependencies<'a>(
    root_id: &'a str,
    nodes: &'a [serde_json::Value],
) -> Vec<&'a str> {
    let mut all_dep_ids: Vec<&str> = Vec::new();
    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut queue: Vec<&str> = vec![root_id];
    visited.insert(root_id);

    // Build node_id -> [dep_pkg_id] mapping
    let mut node_deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in nodes {
        if let Some(node_id) = node["id"].as_str() {
            let mut deps = Vec::new();
            if let Some(dep_array) = node["deps"].as_array() {
                for dep in dep_array {
                    if let Some(pkg) = dep["pkg"].as_str() {
                        deps.push(pkg);
                    }
                }
            }
            node_deps.insert(node_id, deps);
        }
    }

    while let Some(current) = queue.pop() {
        if let Some(deps) = node_deps.get(current) {
            for dep_id in deps {
                if visited.insert(*dep_id) {
                    queue.push(*dep_id);
                    if *dep_id != root_id {
                        all_dep_ids.push(*dep_id);
                    }
                }
            }
        }
    }
    all_dep_ids
}

fn compile_resolved_dependencies(
    all_dep_ids: &[&str],
    nodes: &[serde_json::Value],
    pkg_manifest: &HashMap<&str, PathBuf>,
    pkg_by_id: &HashMap<&str, &serde_json::Value>,
    target_cfg: Option<&[String]>,
    modules: &mut Vec<(String, CompileArtifact)>,
) {
    let mut already_compiled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut manifest_cache: HashMap<String, bool> = HashMap::new();
    const MAX_DEP_CRATES: usize = 64;
    for dep_id in all_dep_ids {
        if already_compiled.len() >= MAX_DEP_CRATES {
            break;
        }
        if let Some(manifest) = pkg_manifest.get(*dep_id) {
            if let Some(pkg) = pkg_by_id.get(*dep_id) {
                let crate_name = pkg["name"].as_str().unwrap_or("");
                // Skip proc-macro crates (metadata first; narrow manifest
                // fallback — substring match mirrors the prior behavior that
                // Self-host relied on, for manifests metadata under-reports).
                let mut is_proc_macro = pkg["targets"].as_array().map_or(false, |targets| {
                    targets.iter().any(|target| {
                        let kind_hit = target["kind"].as_array().map_or(false, |kinds| {
                            kinds.iter().any(|kind| kind.as_str() == Some("proc-macro"))
                        });
                        let crate_type_hit =
                            target["crate_types"].as_array().map_or(false, |cts| {
                                cts.iter().any(|ct| ct.as_str() == Some("proc-macro"))
                            });
                        kind_hit || crate_type_hit
                    })
                });
                if !is_proc_macro {
                    if let Some(manifest_str) = pkg["manifest_path"].as_str() {
                        if let Some(&cached_is_proc_macro) = manifest_cache.get(manifest_str) {
                            is_proc_macro = cached_is_proc_macro;
                        } else {
                            if let Ok(content) = std::fs::read_to_string(manifest_str) {
                                if content.contains("proc-macro") {
                                    is_proc_macro = true;
                                }
                            }
                            manifest_cache.insert(manifest_str.to_string(), is_proc_macro);
                        }
                    }
                }
                if is_proc_macro {
                    continue;
                }
                if already_compiled.contains(crate_name) {
                    continue;
                }
                already_compiled.insert(crate_name.to_string());
                let src_dir = manifest.parent().unwrap_or(Path::new("."));
                let lib_rs = src_dir.join("src").join("lib.rs");
                if lib_rs.exists() {
                    let in_registry = lib_rs
                        .components()
                        .any(|c| c.as_os_str() == "registry" || c.as_os_str() == "git");
                    let src_tree = lib_rs.parent();
                    let tree_bytes = src_tree
                        .and_then(|dir| std::fs::read_dir(dir).ok())
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
                                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                                .sum::<u64>()
                        })
                        .unwrap_or(0);
                    if in_registry && tree_bytes > 40_000 {
                        continue;
                    }
                    // Cargo resolves features per package. Do not lower both sides
                    // of a dependency's mutually exclusive cfg(feature) declarations.
                    let features = resolved_features(nodes, dep_id);
                    let namespace = crate_name.replace('-', "_");
                    if let Ok(Ok(module)) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let mut artifact = rust_front::parse_rust_artifact_with_cargo_cfg(
                                &lib_rs, &features, target_cfg,
                            )?;
                            crate::core_bindings::qualify_module_artifact(
                                &mut artifact,
                                &namespace,
                            )?;
                            Ok::<_, String>(artifact)
                        }))
                    {
                        modules.push((namespace, module));
                    }
                }
            }
        }
    }
}

/// Merge dependency modules into the main module.
/// All function and struct declarations from dependencies are added.
/// Also creates aliases for common re-export patterns.
/// Walk parents until a `Cargo.toml` directory is found.
pub fn find_crate_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if cur.join("Cargo.toml").is_file() {
            return Some(cur);
        }
        cur = cur.parent()?.to_path_buf();
    }
}

pub fn crate_package_name(crate_dir: &Path) -> Option<String> {
    get_manifest(crate_dir).and_then(|m| m.package_name.clone())
}

/// Find the crate root file (lib.rs or main.rs) from a project directory.
pub fn find_crate_root(project_dir: &Path) -> Result<PathBuf, String> {
    let project_dir = find_crate_dir(project_dir).unwrap_or_else(|| project_dir.to_path_buf());

    if let Some(manifest) = get_manifest(&project_dir) {
        if let Some(lib_path_str) = &manifest.lib_path {
            let lib_rs = project_dir.join(lib_path_str);
            if lib_rs.exists() {
                return Ok(lib_rs);
            }
        }
    }

    // Default: src/lib.rs
    let default_lib = project_dir.join("src").join("lib.rs");
    if default_lib.exists() {
        return Ok(default_lib);
    }

    Err("no crate root found".to_string())
}

// Keep original callable names and create the same crate-qualified forwarding aliases.
// Alias production is shared between semantic merge and proof-origin preflight.
fn dependency_function_aliases(
    crate_name: &str,
    dep_module: &UnifiedModule,
) -> Vec<(String, Decl)> {
    if crate_name.is_empty() || crate_name.starts_with("in-") {
        return vec![];
    }
    let mut aliases = Vec::new();
    for decl in &dep_module.decls {
        let Decl::Function {
            name, params, ret, ..
        } = decl
        else {
            continue;
        };
        let qualified = if name.starts_with(&format!("{crate_name}::")) {
            continue;
        } else {
            format!("{crate_name}::{name}")
        };
        if dep_module
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name: n, .. } if n == &qualified))
        {
            continue;
        }
        let args: Vec<Expr> = params.iter().map(|(n, _)| Expr::Ident(n.clone())).collect();
        aliases.push((
            name.clone(),
            Decl::Function {
                name: qualified,
                params: params.clone(),
                ret: ret.clone(),
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident(name.clone())),
                    args,
                }))],
                type_params: vec![],
            },
        ));
    }
    aliases
}

pub fn merge_dependency_modules(main: &mut UnifiedModule, deps: Vec<(String, UnifiedModule)>) {
    let is_structural_schema = |decl: &&Decl| {
        matches!(decl, Decl::Struct { name, fields, type_params }
            if type_params.is_empty() && crate::core_ir::tuples::fields(name)
                .or_else(|| crate::core_ir::rust_options::fields(name)).as_ref() == Some(fields))
    };
    let mut structural_schemas: std::collections::HashSet<_> = main
        .decls
        .iter()
        .filter(is_structural_schema)
        .cloned()
        .collect();
    for (crate_name, mut dep_module) in deps {
        main.regions.append(&mut dep_module.regions);
        // Tuple and Option identities are structural, not crate-local. Coalesce schemas
        // shared by different artifacts, never duplicates within one source or
        // malformed declarations that must still fail verification.
        let mut schemas = HashMap::new();
        for decl in dep_module.decls.iter().filter(is_structural_schema) {
            *schemas.entry(decl.clone()).or_insert(0usize) += 1;
        }
        dep_module
            .decls
            .retain(|decl| !structural_schemas.contains(decl) || schemas[decl] != 1);
        structural_schemas.extend(schemas.into_keys());
        if crate_name.is_empty() || crate_name.starts_with("in-") {
            main.decls.append(&mut dep_module.decls);
            continue;
        }
        let aliases = dependency_function_aliases(&crate_name, &dep_module);
        dep_module
            .decls
            .extend(aliases.into_iter().map(|(_, alias)| alias));
        main.decls.append(&mut dep_module.decls);
    }
}

/// Validate only callables selected by retained proofs (and their generated aliases).
/// Unrelated legacy duplicate symbols keep the existing semantic merge policy.
fn validate_unary_callable_origins(
    main: &CompileArtifact,
    deps: &[(String, CompileArtifact)],
) -> Result<(), String> {
    if !main.unary_methods.values().any(Result::is_ok)
        && !deps
            .iter()
            .any(|(_, artifact)| artifact.unary_methods.values().any(Result::is_ok))
    {
        return Ok(());
    }
    let artifacts: Vec<(&str, &CompileArtifact)> = std::iter::once(("", main))
        .chain(
            deps.iter()
                .map(|(name, artifact)| (name.as_str(), artifact)),
        )
        .collect();
    let mut callables: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let mut selected = Vec::new();
    for (owner, (crate_name, artifact)) in artifacts.iter().enumerate() {
        for decl in &artifact.semantic.decls {
            if let Decl::Function { name, .. } = decl {
                callables
                    .entry(name.clone())
                    .or_default()
                    .push((owner, name.clone()));
            }
        }
        for proof in artifact.unary_methods.values() {
            let Ok(target) = proof else {
                continue;
            };
            let mut definitions = artifact
                .semantic
                .decls
                .iter()
                .filter(|decl| matches!(decl, Decl::Function { name, .. } if name == target));
            let first = definitions.next().ok_or_else(|| {
                format!("missing dependency unary callable `{target}` in its proof origin")
            })?;
            if definitions.any(|definition| definition != first) {
                return Err(format!("ambiguous dependency unary callable `{target}`"));
            }
            selected.push((owner, target.clone(), target.clone()));
        }
        for (target, alias) in dependency_function_aliases(crate_name, &artifact.semantic) {
            let Decl::Function { name, .. } = alias else {
                return Err("invalid generated dependency function alias".into());
            };
            callables
                .entry(name.clone())
                .or_default()
                .push((owner, target.clone()));
            if artifact
                .unary_methods
                .values()
                .any(|proof| proof.as_ref().ok() == Some(&target))
            {
                selected.push((owner, name, target));
            }
        }
    }
    for (owner, name, target) in selected {
        if callables.get(&name).is_none_or(|origins| {
            origins
                .iter()
                .any(|(origin, source)| *origin != owner || source != &target)
        }) {
            return Err(format!("ambiguous dependency unary callable `{name}`"));
        }
    }
    Ok(())
}

/// Cargo preserves nominal names; do not attach a proof to a different crate's
/// same-spelled schema. Perform all checks before changing either semantic IR or metadata.
pub fn merge_dependency_artifacts(
    main: &mut CompileArtifact,
    deps: Vec<(String, CompileArtifact)>,
) -> Result<(), String> {
    validate_unary_callable_origins(main, &deps)?;
    let mut bindings = main.unary_methods.clone();
    let mut nominal_owners: HashMap<String, usize> = HashMap::new();
    let mut binding_owners: HashMap<String, usize> = HashMap::new();
    for (owner, artifact) in std::iter::once(main as &CompileArtifact)
        .chain(deps.iter().map(|(_, artifact)| artifact))
        .enumerate()
    {
        for decl in &artifact.semantic.decls {
            if let Decl::Struct { name, .. }
            | Decl::Class { name, .. }
            | Decl::Interface { name, .. } = decl
            {
                if binding_owners
                    .get(name)
                    .is_some_and(|previous| *previous != owner)
                {
                    return Err(format!("ambiguous dependency unary receiver `{name}`"));
                }
                if let Some(previous) = nominal_owners.insert(name.clone(), owner)
                    && previous != owner
                    && (binding_owners.contains_key(name)
                        || artifact.unary_methods.keys().any(|(receiver, _)| {
                            crate::core_ir::rust_references::receiver_nominal_identity(receiver)
                                == Some(name.as_str())
                        }))
                {
                    return Err(format!("ambiguous dependency unary receiver `{name}`"));
                }
            }
        }
        for (receiver, _) in artifact.unary_methods.keys() {
            let nominal = crate::core_ir::rust_references::receiver_nominal_identity(receiver)
                .ok_or_else(|| format!("invalid dependency unary receiver `{receiver}`"))?;
            if nominal_owners
                .get(nominal)
                .is_some_and(|previous| *previous != owner)
            {
                return Err(format!("ambiguous dependency unary receiver `{receiver}`"));
            }
            binding_owners.insert(nominal.to_string(), owner);
        }
        merge_unary_method_bindings(&mut bindings, artifact.unary_methods.clone())?;
    }
    merge_dependency_modules(
        &mut main.semantic,
        deps.into_iter()
            .map(|(name, artifact)| (name, artifact.semantic))
            .collect(),
    );
    main.unary_methods = bindings;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
                "inauguration-cargo-linker-{}-{}-{}",
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

    #[test]
    fn cargo_dependencies_select_features_and_target_cfg_before_lowering() {
        for enabled in [false, true] {
            let temp = TempDirGuard::new();
            fs::create_dir_all(temp.path.join("src")).unwrap();
            fs::create_dir_all(temp.path.join("dep/src")).unwrap();
            fs::write(temp.path.join("src/lib.rs"), "").unwrap();
            fs::write(temp.path.join("dep/Cargo.toml"), "[package]\nname=\"cfg-dep\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[features]\nselected=[]\n").unwrap();
            fs::write(temp.path.join("dep/src/lib.rs"), "#[cfg(feature=\"selected\")]pub const SELECTED:bool=true;#[cfg(not(feature=\"selected\"))]pub const SELECTED:bool=false;pub mod child;").unwrap();
            fs::write(temp.path.join("dep/src/child.rs"), "#[cfg(feature=\"selected\")]pub type Byte=u8;#[cfg(not(feature=\"selected\"))]pub type Byte=u16;pub fn value()->Byte{42}#[cfg(all(unix,target_os=\"linux\"))]pub const PLATFORM:bool=true;#[cfg(all(windows,target_os=\"windows\"))]pub const PLATFORM:bool=false;").unwrap();
            let features = if enabled { "[\"selected\"]" } else { "[]" };
            fs::write(temp.path.join("Cargo.toml"), format!("[package]\nname=\"cfg-caller\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\ncfg-dep={{path=\"dep\",features={features}}}\n")).unwrap();
            for (target, unix) in [
                ("x86_64-unknown-linux-gnu", true),
                ("x86_64-pc-windows-msvc", false),
            ] {
                let dependencies =
                    compile_cargo_dependency_artifacts_for_target(&temp.path, Some(target));
                let (_, artifact) = dependencies
                    .iter()
                    .find(|(name, _)| name == "cfg_dep")
                    .unwrap();
                for (expected_name, expected) in [
                    ("cfg_dep::SELECTED", enabled),
                    ("cfg_dep::child::PLATFORM", unix),
                ] {
                    let values: Vec<_> = artifact
                        .semantic
                        .decls
                        .iter()
                        .filter_map(|decl| match decl {
                            Decl::Global {
                                name,
                                init: Some(value),
                                ..
                            } if name == expected_name => Some(value.as_ref()),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(
                        values,
                        vec![&Expr::BoolLit(expected)],
                        "{target}: {expected_name}"
                    );
                }
                assert!(
                    crate::typecheck::TypeChecker::new()
                        .typed_proof_nodes(&artifact.semantic.decls)
                        .is_empty()
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn cargo_crates_keep_names_and_operator_proofs_separate_from_the_binary() {
        let temp = TempDirGuard::new();
        fs::create_dir_all(temp.path.join("src")).unwrap();
        fs::write(temp.path.join("Cargo.toml"), "[package]\nname=\"caller\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[features]\ndefault=[\"selected\"]\nselected=[]\n[dependencies]\nleft-value={path=\"left\"}\nright={path=\"right\"}\n").unwrap();
        fs::write(
            temp.path.join("src/lib.rs"),
            "pub type Result<T>=std::result::Result<T,()>;trait Mark{fn read(self)->i64;}struct Row{value:i64}impl Mark for Row{fn read(self)->i64{self.value}}fn keep<T:Mark>(row:T)->i64{row.read()}pub fn answer()->i64{let pair:(i64,i64)=(5,0);keep(Row{value:pair.0})}",
        )
        .unwrap();
        let path = temp.path.join("src/main.rs");
        fs::write(&path, "type Result<T>=std::result::Result<T,()>;struct Row{value:i64}#[cfg(all(feature=\"selected\",unix))]fn answer()->i64{left_value::answer()+right::answer()+caller::answer()}#[cfg(not(all(feature=\"selected\",unix)))]fn answer()->i64{7}fn main()->i64{answer()}").unwrap();
        for (directory, package, value, increment) in
            [("left", "left-value", 7, 10), ("right", "right", 4, 16)]
        {
            fs::create_dir_all(temp.path.join(directory).join("src")).unwrap();
            fs::write(
                temp.path.join(directory).join("Cargo.toml"),
                format!("[package]\nname=\"{package}\"\nversion=\"0.1.0\"\nedition=\"2021\"\n"),
            )
            .unwrap();
            fs::write(temp.path.join(directory).join("src/lib.rs"), format!("pub type Result<T>=std::result::Result<T,()>;pub struct Row{{value:i64}}impl std::ops::Neg for Row{{type Output=i64;fn neg(self)->i64{{self.value+helper()}}}}fn helper()->i64{{let pair:(i64,i64)=({increment},0);pair.0}}pub fn answer()->i64{{-Row{{value:{value}}}}}")).unwrap();
        }
        let report =
            crate::owned_compile::compile_owned(&crate::owned_compile::OwnedCompileRequest {
                path,
                module_id: "caller".into(),
                parser: crate::parser_registry::ParserCli::Auto,
                target: crate::owned_compile::CompileTarget::Jit,
                entry: Some("main".into()),
                out: None,
                linkage: crate::native_emit::NativeLinkage::Executable,
                target_triple: None,
                jobs: 1,
                debug: false,
                profile: crate::emit_profile::EmitProfile::Default,
                emit: None,
                base: None,
            });
        assert!(report.success && report.execution_started, "{report:?}");
        assert_eq!(
            report.eval_result,
            Some(crate::owned_compile::EvalValue::Int(42))
        );
    }

    #[test]
    fn test_compile_cargo_dependencies_finds_and_compiles() -> std::io::Result<()> {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        let src_dir = temp.path.join("src");
        fs::create_dir_all(&src_dir)?;
        let lib_rs = src_dir.join("lib.rs");

        let dep_dir = temp.path.join("dummy-dep");
        let dep_src_dir = dep_dir.join("src");
        fs::create_dir_all(&dep_src_dir)?;
        let dep_cargo_toml = dep_dir.join("Cargo.toml");
        let dep_lib_rs = dep_src_dir.join("lib.rs");

        // The dependency name must be in the direct_dep_names set.
        // We use "libc" as the dummy local dependency name so that the compiler logic processes it.
        fs::write(
            &dep_cargo_toml,
            r#"[package]
name = "libc"
version = "0.2.0"
edition = "2021"
"#,
        )?;
        fs::write(&dep_lib_rs, "pub fn libc_func() {}")?;

        fs::write(
            &cargo_toml,
            r#"[package]
name = "dummy-pkg"
version = "0.1.0"
edition = "2021"

[dependencies]
libc = { path = "dummy-dep" }
"#,
        )?;
        fs::write(&lib_rs, "pub fn foo() {}")?;

        let modules = compile_cargo_dependencies(&temp.path);

        assert!(
            !modules.is_empty(),
            "Expected at least one dependency compiled"
        );
        let libc_dep = modules.iter().find(|(name, _)| name == "libc");
        assert!(
            libc_dep.is_some(),
            "Expected 'libc' to be in the compiled dependencies"
        );

        Ok(())
    }

    #[test]
    fn find_crate_dir_walks_parents() {
        let dir = std::env::temp_dir().join(format!(
            "in-crate-walk-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"walkme\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let nested = dir.join("src").join("main.rs");
        fs::write(&nested, "fn main() {}").unwrap();
        assert_eq!(find_crate_dir(&nested).as_deref(), Some(dir.as_path()));
        assert_eq!(crate_package_name(&dir).as_deref(), Some("walkme"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_compile_cargo_dependencies_proc_macro() -> std::io::Result<()> {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        let src_dir = temp.path.join("src");
        fs::create_dir_all(&src_dir)?;
        let lib_rs = src_dir.join("lib.rs");

        let dep_dir = temp.path.join("proc_macro_dep");
        let dep_src_dir = dep_dir.join("src");
        fs::create_dir_all(&dep_src_dir)?;
        let dep_cargo_toml = dep_dir.join("Cargo.toml");
        let dep_lib_rs = dep_src_dir.join("lib.rs");

        fs::write(
            &dep_cargo_toml,
            r#"[package]
name = "proc_macro_dep"
version = "0.1.0"
edition = "2021"

[lib]
proc-macro = true
"#,
        )?;
        fs::write(&dep_lib_rs, "pub fn macro_func() {}")?;

        fs::write(
            &cargo_toml,
            r#"[package]
name = "dummy-pkg"
version = "0.1.0"
edition = "2021"

[dependencies]
proc_macro_dep = { path = "proc_macro_dep" }
"#,
        )?;
        fs::write(&lib_rs, "pub fn foo() {}")?;

        let modules = compile_cargo_dependencies(&temp.path);

        assert!(
            modules.is_empty(),
            "Expected proc-macro dependencies to be skipped"
        );

        Ok(())
    }

    #[test]
    fn test_compile_cargo_dependencies_transitive() -> std::io::Result<()> {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        let src_dir = temp.path.join("src");
        fs::create_dir_all(&src_dir)?;
        let lib_rs = src_dir.join("lib.rs");

        let direct_dir = temp.path.join("direct_dep");
        let direct_src_dir = direct_dir.join("src");
        fs::create_dir_all(&direct_src_dir)?;
        let direct_cargo_toml = direct_dir.join("Cargo.toml");
        let direct_lib_rs = direct_src_dir.join("lib.rs");

        let trans_dir = temp.path.join("transitive_dep");
        let trans_src_dir = trans_dir.join("src");
        fs::create_dir_all(&trans_src_dir)?;
        let trans_cargo_toml = trans_dir.join("Cargo.toml");
        let trans_lib_rs = trans_src_dir.join("lib.rs");

        fs::write(
            &trans_cargo_toml,
            r#"[package]
name = "transitive_dep"
version = "0.1.0"
edition = "2021"
"#,
        )?;
        fs::write(&trans_lib_rs, "pub fn transitive_func() {}")?;

        fs::write(
            &direct_cargo_toml,
            r#"[package]
name = "direct_dep"
version = "0.1.0"
edition = "2021"

[dependencies]
transitive_dep = { path = "../transitive_dep" }
"#,
        )?;
        fs::write(&direct_lib_rs, "pub fn direct_func() {}")?;

        fs::write(
            &cargo_toml,
            r#"[package]
name = "dummy-pkg"
version = "0.1.0"
edition = "2021"

[dependencies]
direct_dep = { path = "direct_dep" }
"#,
        )?;
        fs::write(&lib_rs, "pub fn foo() {}")?;

        let modules = compile_cargo_dependencies(&temp.path);

        assert_eq!(
            modules.len(),
            2,
            "Expected both direct and transitive dependencies to be compiled"
        );
        let has_direct = modules.iter().any(|(name, _)| name == "direct_dep");
        let has_transitive = modules.iter().any(|(name, _)| name == "transitive_dep");
        assert!(
            has_direct,
            "Expected 'direct_dep' to be in the compiled dependencies"
        );
        assert!(
            has_transitive,
            "Expected 'transitive_dep' to be in the compiled dependencies"
        );

        Ok(())
    }

    #[test]
    fn test_compile_cargo_dependencies_no_cargo_toml() {
        let temp = TempDirGuard::new();
        let modules = compile_cargo_dependencies(&temp.path);
        assert!(modules.is_empty());
    }

    #[test]
    fn test_compile_cargo_dependencies_invalid_cargo_toml() -> std::io::Result<()> {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        fs::write(&cargo_toml, "invalid toml [] [] []")?;
        let modules = compile_cargo_dependencies(&temp.path);
        assert!(modules.is_empty());

        Ok(())
    }

    #[test]
    fn test_get_manifest_valid() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"src/my_lib.rs\"\n",
        )
        .unwrap();

        let manifest = get_manifest(&temp.path).expect("manifest should be parsed");
        assert_eq!(manifest.package_name.as_deref(), Some("my-crate"));
        assert_eq!(manifest.lib_path.as_deref(), Some("src/my_lib.rs"));
    }

    #[test]
    fn test_get_manifest_partial() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"partial-crate\"\n",
        )
        .unwrap();

        let manifest = get_manifest(&temp.path).expect("manifest should be parsed");
        assert_eq!(manifest.package_name.as_deref(), Some("partial-crate"));
        assert_eq!(manifest.lib_path, None);
    }

    #[test]
    fn test_find_crate_root_default() {
        let temp = TempDirGuard::new();
        let src_dir = temp.path.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let lib_rs = src_dir.join("lib.rs");
        fs::write(&lib_rs, "pub fn default() {}").unwrap();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"test-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        assert_eq!(find_crate_root(&temp.path).unwrap(), lib_rs);
    }

    #[test]
    fn test_find_crate_root_custom_path() {
        let temp = TempDirGuard::new();
        let custom_lib = temp.path.join("custom_lib.rs");
        fs::write(&custom_lib, "pub fn custom() {}").unwrap();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"custom-crate\"\nversion = \"0.1.0\"\n\n[lib]\npath = \"custom_lib.rs\"\n",
        )
        .unwrap();

        assert_eq!(find_crate_root(&temp.path).unwrap(), custom_lib);
    }

    #[test]
    fn test_find_crate_root_nested_dir() {
        let temp = TempDirGuard::new();
        let src_dir = temp.path.join("src");
        let nested_dir = src_dir.join("nested").join("module");
        fs::create_dir_all(&nested_dir).unwrap();
        let lib_rs = src_dir.join("lib.rs");
        fs::write(&lib_rs, "pub fn default() {}").unwrap();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"test-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        assert_eq!(find_crate_root(&nested_dir).unwrap(), lib_rs);
    }

    #[test]
    fn test_get_manifest_success() {
        let temp = TempDirGuard::new();
        let manifest = temp.path.join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"test-package\"\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        let result = get_manifest(&temp.path).unwrap();
        assert_eq!(result.package_name.as_deref(), Some("test-package"));
        assert_eq!(result.lib_path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn test_get_manifest_no_file() {
        let temp = TempDirGuard::new();
        assert!(get_manifest(&temp.path).is_none());
    }

    #[test]
    fn test_find_crate_root_no_cargo_toml() {
        let temp = TempDirGuard::new();
        assert_eq!(
            find_crate_root(&temp.path),
            Err("no crate root found".to_string())
        );
    }

    #[test]
    fn test_find_crate_root_no_lib_rs() {
        let temp = TempDirGuard::new();
        fs::write(
            temp.path.join("Cargo.toml"),
            "[package]\nname = \"test-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        assert_eq!(
            find_crate_root(&temp.path),
            Err("no crate root found".to_string())
        );
    }

    #[test]
    fn manifest_rewrites_refresh_package_name_and_library_path() -> std::io::Result<()> {
        let temp = TempDirGuard::new();
        let first = temp.path.join("first.rs");
        let second = temp.path.join("second.rs");
        fs::write(&first, "pub fn first() {}")?;
        fs::write(&second, "pub fn second() {}")?;
        let manifest = temp.path.join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname = \"first-package\"\n[lib]\npath = \"first.rs\"\n",
        )?;
        assert_eq!(
            crate_package_name(&temp.path).as_deref(),
            Some("first-package")
        );
        assert_eq!(find_crate_root(&temp.path), Ok(first.clone()));

        // Keep both source files so a stale cached library path still exists.
        fs::write(
            &manifest,
            "[package]\nname = \"other-package\"\n[lib]\npath = \"second.rs\"\n",
        )?;
        assert!(first.exists() && second.exists());
        assert_eq!(
            crate_package_name(&temp.path).as_deref(),
            Some("other-package")
        );
        assert_eq!(find_crate_root(&temp.path), Ok(second));
        Ok(())
    }

    #[test]
    fn dependency_structural_merging_keeps_source_conflicts_visible() {
        for typ in [
            crate::core_ir::tuples::typ(&[crate::core_ir::Typ::Int]),
            crate::core_ir::rust_options::typ(crate::core_ir::Typ::Int),
        ] {
            let crate::core_ir::Typ::Named(name) = typ else {
                unreachable!()
            };
            let schema = Decl::Struct {
                fields: crate::core_ir::tuples::fields(&name)
                    .or_else(|| crate::core_ir::rust_options::fields(&name))
                    .unwrap(),
                name,
                type_params: vec![],
            };
            for copies in [1, 2] {
                let mut main = UnifiedModule::new(vec![schema.clone()]);
                merge_dependency_modules(
                    &mut main,
                    vec![(
                        "dep".into(),
                        UnifiedModule::new(vec![schema.clone(); copies]),
                    )],
                );
                let report = crate::core_ir_verifier::verify_module(
                    &main,
                    &crate::core_ir_verifier::VerifyOptions {
                        entry: None,
                        require_entry: false,
                    },
                );
                assert_eq!(report.ok, copies == 1, "{report:?}");
                if copies > 1 {
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("duplicate-top-level-name")
                    );
                }
            }
        }
    }

    #[test]
    fn test_merge_dependency_modules() {
        let mut main = UnifiedModule::new(vec![]);

        let mut in_dep = UnifiedModule::new(vec![Decl::Struct {
            name: "InStruct".into(),
            fields: vec![],
            type_params: vec![],
        }]);
        in_dep.mark_noalias(&["left", "right"]);

        let ext_dep = UnifiedModule::new(vec![Decl::Function {
            name: "ext_func".into(),
            params: vec![],
            ret: crate::core_ir::Typ::Void,
            body: vec![],
            type_params: vec![],
        }]);

        let empty_dep = UnifiedModule::new(vec![Decl::Function {
            name: "empty_func".into(),
            params: vec![],
            ret: crate::core_ir::Typ::Void,
            body: vec![],
            type_params: vec![],
        }]);

        merge_dependency_modules(
            &mut main,
            vec![
                ("in-core".into(), in_dep),
                ("my_crate".into(), ext_dep),
                ("".into(), empty_dep),
            ],
        );

        assert_eq!(main.decls.len(), 4);
        assert_eq!(main.noalias_params(), vec!["left", "right"]);

        assert!(
            main.decls
                .iter()
                .any(|d| matches!(d, Decl::Struct { name, .. } if name == "InStruct"))
        );
        assert!(
            main.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "ext_func"))
        );
        assert!(
            main.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "my_crate::ext_func"))
        );
        assert!(
            main.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "empty_func"))
        );
    }
}

#[cfg(test)]
mod retained_artifact_tests {
    use super::*;
    fn record() -> Decl {
        Decl::Struct {
            name: "Row".into(),
            fields: vec![("value".into(), crate::core_ir::Typ::Int)],
            type_params: vec![],
        }
    }
    #[test]
    fn metadata_only_dependency_is_retained_without_new_declarations() {
        let mut main = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
        let mut dep = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
        dep.unary_methods
            .insert(("Row".into(), "-".into()), Err("unresolved trait".into()));
        merge_dependency_artifacts(&mut main, vec![("dep".into(), dep.clone())]).unwrap();
        assert!(main.semantic.decls.is_empty());
        assert_eq!(main.unary_methods, dep.unary_methods);
    }
    #[test]
    fn metadata_only_binding_cannot_attach_to_another_origin_schema_in_either_order() {
        for receiver in ["Row", "&Row", "&mut Row"] {
            for proof in [Ok("Row::neg".into()), Err("unresolved".into())] {
                let mut binding = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
                binding
                    .unary_methods
                    .insert((receiver.into(), "-".into()), proof);
                let schema = CompileArtifact::from_semantic(UnifiedModule::new(vec![record()]));
                for (first, second) in [
                    (binding.clone(), schema.clone()),
                    (schema.clone(), binding.clone()),
                ] {
                    let mut main = first.clone();
                    assert!(
                        merge_dependency_artifacts(&mut main, vec![("dep".into(), second)])
                            .is_err()
                    );
                    assert_eq!(main, first);
                }
            }
        }
    }

    fn callable(name: &str, value: i64) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret: crate::core_ir::Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(value)))],
            type_params: vec![],
        }
    }
    fn proof_artifact() -> CompileArtifact {
        let mut artifact = CompileArtifact::from_semantic(UnifiedModule::new(vec![
            record(),
            callable("Row::neg", 77),
        ]));
        artifact
            .unary_methods
            .insert(("Row".into(), "-".into()), Ok("Row::neg".into()));
        artifact
    }
    #[test]
    fn proof_callable_cannot_collide_with_another_origins_exact_symbol() {
        for conflicting in ["Row::neg", "dep::Row::neg"] {
            let mut main =
                CompileArtifact::from_semantic(UnifiedModule::new(vec![callable(conflicting, 42)]));
            let before = main.clone();
            assert!(
                merge_dependency_artifacts(&mut main, vec![("dep".into(), proof_artifact())])
                    .is_err(),
                "{conflicting}"
            );
            assert_eq!(main, before);
        }
    }
    #[test]
    fn metadata_only_ok_proof_cannot_borrow_an_unrelated_callable() {
        let mut main =
            CompileArtifact::from_semantic(UnifiedModule::new(vec![callable("Row::neg", 42)]));
        let before = main.clone();
        let mut dep = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
        dep.unary_methods
            .insert(("Row".into(), "-".into()), Ok("Row::neg".into()));
        assert!(merge_dependency_artifacts(&mut main, vec![("dep".into(), dep)]).is_err());
        assert_eq!(main, before);
    }
    #[test]
    fn generated_alias_cannot_replace_a_proof_target_from_another_origin() {
        let mut main = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
        let mut proof = proof_artifact();
        proof.semantic.decls.push(callable("dep::Row::neg", 77));
        proof
            .unary_methods
            .insert(("Row".into(), "-".into()), Ok("dep::Row::neg".into()));
        let foreign =
            CompileArtifact::from_semantic(UnifiedModule::new(vec![callable("Row::neg", 42)]));
        assert!(
            merge_dependency_artifacts(
                &mut main,
                vec![("owner".into(), proof), ("dep".into(), foreign)]
            )
            .is_err()
        );
        assert!(main.semantic.decls.is_empty());
    }
    #[test]
    fn repeated_same_origin_metadata_and_unrelated_duplicates_keep_existing_policy() {
        let mut main = proof_artifact();
        let before = main.clone();
        merge_dependency_artifacts(&mut main, vec![]).unwrap();
        assert_eq!(main, before);
        let same_metadata = main.unary_methods.clone();
        merge_unary_method_bindings(&mut main.unary_methods, same_metadata).unwrap();
        let unrelated = CompileArtifact::from_semantic(UnifiedModule::new(vec![
            callable("other", 1),
            callable("other", 2),
        ]));
        merge_dependency_artifacts(&mut main, vec![("dep".into(), unrelated)]).unwrap();
        assert_eq!(main.unary_methods, before.unary_methods);
    }

    #[test]
    fn cargo_function_alias_does_not_change_nominal_proof_target() {
        let mut main = CompileArtifact::from_semantic(UnifiedModule::new(vec![]));
        let mut dep = CompileArtifact::from_semantic(UnifiedModule::new(vec![
            record(),
            Decl::Function {
                name: "Row::neg".into(),
                params: vec![],
                ret: crate::core_ir::Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(77)))],
                type_params: vec![],
            },
        ]));
        dep.unary_methods
            .insert(("Row".into(), "-".into()), Ok("Row::neg".into()));
        merge_dependency_artifacts(&mut main, vec![("dep".into(), dep)]).unwrap();
        assert_eq!(
            main.unary_methods.get(&("Row".into(), "-".into())),
            Some(&Ok("Row::neg".into()))
        );
        assert!(
            main.semantic
                .decls
                .iter()
                .any(|decl| matches!(decl, Decl::Function {name, ..} if name == "dep::Row::neg"))
        );
        assert!(
            !main
                .unary_methods
                .contains_key(&("dep::Row".into(), "-".into()))
        );
    }

    #[test]
    fn test_merge_dependency_modules_merges_decls_and_generates_aliases() {
        let mut main = UnifiedModule::new(vec![callable("main_func", 1)]);
        let dep1 = UnifiedModule::new(vec![callable("dep1_func", 2)]);
        let dep2 = UnifiedModule::new(vec![callable("dep2_func", 3)]);

        let deps = vec![("in-dep".into(), dep1), ("dep2".into(), dep2)];

        merge_dependency_modules(&mut main, deps);

        assert_eq!(main.decls.len(), 4);

        let has_main_func = main
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "main_func"));
        assert!(has_main_func);

        let has_dep1_func = main
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "dep1_func"));
        assert!(has_dep1_func);

        let has_dep2_func = main
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "dep2_func"));
        assert!(has_dep2_func);

        let has_dep2_alias = main
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "dep2::dep2_func"));
        assert!(has_dep2_alias);
    }

    #[test]
    fn same_named_nominals_from_different_dependencies_refuse_atomically() {
        for receiver in ["Row", "&Row", "&mut Row"] {
            let mut main = CompileArtifact::from_semantic(UnifiedModule::new(vec![record()]));
            let previous = main.clone();
            let mut dep = CompileArtifact::from_semantic(UnifiedModule::new(vec![record()]));
            dep.unary_methods
                .insert((receiver.into(), "-".into()), Ok("Row::neg".into()));
            assert!(
                merge_dependency_artifacts(&mut main, vec![("dep".into(), dep)]).is_err(),
                "{receiver}"
            );
            assert_eq!(main, previous);
        }
    }
}
