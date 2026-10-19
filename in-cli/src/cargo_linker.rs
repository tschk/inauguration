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

    compile_resolved_dependencies(&all_dep_ids, &pkg_manifest, &pkg_by_id, &mut modules);

    modules
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
    pkg_manifest: &HashMap<&str, PathBuf>,
    pkg_by_id: &HashMap<&str, &serde_json::Value>,
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
                    if let Ok(Ok(module)) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            rust_front::parse_rust_artifact(&lib_rs)
                        }))
                    {
                        modules.push((crate_name.to_string(), module));
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

    let prefix = format!("{crate_name}::");
    let mut existing_qualified = std::collections::HashSet::new();
    for decl in &dep_module.decls {
        if let Decl::Function { name, .. } = decl {
            if name.starts_with(&prefix) {
                existing_qualified.insert(name.as_str());
            }
        }
    }

    let mut aliases = Vec::new();
    for decl in &dep_module.decls {
        let Decl::Function {
            name, params, ret, ..
        } = decl
        else {
            continue;
        };
        if name.starts_with(&prefix) {
            continue;
        }

        let qualified = format!("{crate_name}::{name}");
        if existing_qualified.contains(qualified.as_str()) {
            continue;
        }

        let mut args = Vec::with_capacity(params.len());
        for (n, _) in params {
            args.push(Expr::Ident(n.clone()));
        }

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
    for (crate_name, mut dep_module) in deps {
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

#[allow(dead_code)]
fn prefix_calls(stmts: &mut [Stmt], crate_name: &str, _in_prefixed: bool) {
    for stmt in stmts {
        match stmt {
            // Type qualification is performed by the shared namespace walker;
            // a declaration has no initializer call to prefix.
            Stmt::Declare(..) => {}
            Stmt::Expr(expr) | Stmt::Return(Some(expr)) => prefix_call_expr(expr, crate_name),
            Stmt::Let(_, _, expr) | Stmt::Assign(_, expr) => prefix_call_expr(expr, crate_name),
            Stmt::If {
                then_body,
                else_body,
                cond,
                ..
            } => {
                prefix_call_expr(cond, crate_name);
                prefix_calls(then_body, crate_name, false);
                prefix_calls(else_body, crate_name, false);
            }
            Stmt::Loop { body, cond, .. } => {
                if let Some(cond_expr) = cond {
                    prefix_call_expr(cond_expr, crate_name);
                }
                prefix_calls(body, crate_name, false);
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    prefix_calls(&mut arm.body, crate_name, false);
                }
            }
            _ => {}
        }
    }
}

#[allow(dead_code)]
fn prefix_call_expr(expr: &mut Expr, crate_name: &str) {
    match expr {
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_mut() {
                if !name.contains("::") {
                    *name = format!("{crate_name}::{name}");
                }
            }
            prefix_call_expr(callee.as_mut(), crate_name);
            for arg in args.iter_mut() {
                prefix_call_expr(arg, crate_name);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            prefix_call_expr(lhs.as_mut(), crate_name);
            prefix_call_expr(rhs.as_mut(), crate_name);
        }
        Expr::Unary { expr: inner, .. } => prefix_call_expr(inner.as_mut(), crate_name),
        Expr::Field { base, .. } => prefix_call_expr(base.as_mut(), crate_name),
        Expr::Index { base, index } => {
            prefix_call_expr(base.as_mut(), crate_name);
            prefix_call_expr(index.as_mut(), crate_name);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields.iter_mut() {
                prefix_call_expr(field_expr, crate_name);
            }
        }
        _ => {}
    }
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
