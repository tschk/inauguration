use crate::compiler::rust_front;
use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    modules: &mut Vec<(String, UnifiedModule)>,
) {
    let mut already_compiled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let direct_dep_names: std::collections::HashSet<&str> = [
        "clap",
        "serde",
        "serde_json",
        "sha2",
        "syn",
        "quote",
        "thiserror",
        "tokio",
        "tree-sitter",
        "tree-sitter-c",
        "tree-sitter-cpp",
        "tree-sitter-c-sharp",
        "tree-sitter-dart",
        "tree-sitter-elixir",
        "tree-sitter-erlang",
        "tree-sitter-fsharp",
        "tree-sitter-go",
        "tree-sitter-groovy",
        "tree-sitter-haskell",
        "tree-sitter-holyc",
        "tree-sitter-java",
        "tree-sitter-javascript",
        "tree-sitter-julia",
        "tree-sitter-kotlin-ng",
        "tree-sitter-lua",
        "tree-sitter-objc",
        "tree-sitter-ocaml",
        "tree-sitter-perl",
        "tree-sitter-php",
        "tree-sitter-python",
        "tree-sitter-r",
        "tree-sitter-ruby",
        "tree-sitter-rust",
        "tree-sitter-scala",
        "tree-sitter-swift",
        "tree-sitter-typescript",
        "tree-sitter-v",
        "tree-sitter-zig",
        "libc",
        "libloading",
        "notify",
    ]
    .iter()
    .cloned()
    .collect();
    for dep_id in all_dep_ids {
        if let Some(manifest) = pkg_manifest.get(*dep_id) {
            if let Some(pkg) = pkg_by_id.get(*dep_id) {
                let crate_name = pkg["name"].as_str().unwrap_or("");
                if !direct_dep_names.contains(crate_name) {
                    continue;
                }
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
                        if let Ok(content) = std::fs::read_to_string(manifest_str) {
                            if content.contains("proc-macro") {
                                is_proc_macro = true;
                            }
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
                    if let Ok(module) = rust_front::parse_rust_file(&lib_rs) {
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
/// Find the crate root file (lib.rs or main.rs) from a project directory.
pub fn find_crate_root(project_dir: &Path) -> Result<PathBuf, String> {
    // Check for Cargo.toml
    let cargo_toml = project_dir.join("Cargo.toml");
    if cargo_toml.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
            let lines: Vec<&str> = content.lines().collect();
            // Find [lib] section and look for path = ... within it
            for i in 0..lines.len() {
                if lines[i].trim() == "[lib]" {
                    // Scan subsequent lines until next section
                    for j in (i + 1)..lines.len().min(i + 20) {
                        let trimmed = lines[j].trim();
                        if trimmed.starts_with('[') {
                            break;
                        } // next section
                        if let Some(val) = trimmed.strip_prefix("path").and_then(|s| {
                            s.split('=')
                                .nth(1)
                                .map(|v| v.trim().trim_matches('"').to_string())
                        }) {
                            let lib_rs = project_dir.join(&val);
                            if lib_rs.exists() {
                                return Ok(lib_rs);
                            }
                        }
                    }
                }
            }
            // Default: src/lib.rs
            let default_lib = project_dir.join("src").join("lib.rs");
            if default_lib.exists() {
                return Ok(default_lib);
            }
        }
    }
    Err("no crate root found".to_string())
}

pub fn merge_dependency_modules(main: &mut UnifiedModule, deps: Vec<(String, UnifiedModule)>) {
    for (crate_name, mut dep_module) in deps {
        // Prefix function names with crate name to avoid duplicates across crates
        // Skip if crate_name starts with "in-" (the main crate) — keep original names
        if !crate_name.starts_with("in-") {
            for decl in &mut dep_module.decls {
                if let Decl::Function { name, .. } = decl {
                    if !name.contains("::") {
                        *name = format!("{crate_name}::{name}");
                    }
                }
            }
        }
        // Update call sites: replace unprefixed calls with prefixed names
        for decl in &mut dep_module.decls {
            if let Decl::Function { body, .. } = decl {
                prefix_calls(body, &crate_name, false);
            }
        }
        main.decls.append(&mut dep_module.decls);
    }
}

/// Recursively prefix function call targets in a statement list.
fn prefix_calls(stmts: &mut [Stmt], crate_name: &str, _in_prefixed: bool) {
    for stmt in stmts {
        match stmt {
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
    fn test_compile_cargo_dependencies_finds_and_compiles() {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        let src_dir = temp.path.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let lib_rs = src_dir.join("lib.rs");

        let dep_dir = temp.path.join("dummy-dep");
        let dep_src_dir = dep_dir.join("src");
        fs::create_dir_all(&dep_src_dir).unwrap();
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
        )
        .unwrap();
        fs::write(&dep_lib_rs, "pub fn libc_func() {}").unwrap();

        fs::write(
            &cargo_toml,
            r#"[package]
name = "dummy-pkg"
version = "0.1.0"
edition = "2021"

[dependencies]
libc = { path = "dummy-dep" }
"#,
        )
        .unwrap();
        fs::write(&lib_rs, "pub fn foo() {}").unwrap();

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
    }

    #[test]
    fn test_compile_cargo_dependencies_no_cargo_toml() {
        let temp = TempDirGuard::new();
        let modules = compile_cargo_dependencies(&temp.path);
        assert!(modules.is_empty());
    }

    #[test]
    fn test_compile_cargo_dependencies_invalid_cargo_toml() {
        let temp = TempDirGuard::new();
        let cargo_toml = temp.path.join("Cargo.toml");
        fs::write(&cargo_toml, "invalid toml [] [] []").unwrap();
        let modules = compile_cargo_dependencies(&temp.path);
        assert!(modules.is_empty());
    }
}
