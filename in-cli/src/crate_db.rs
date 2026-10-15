//! Lazy crate database: caches parsed modules, symbol indices.
//!
//! Crates loaded on demand — no eager parsing. When a symbol is
//! requested, the owning crate and module are located, parsed, and
//! registered. Transitive deps resolved recursively.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::core_ir::{Decl, UnifiedModule};
use crate::parser_registry::{ParserId, ResolvedBuildParser};

/// Location of a symbol definition within a crate.
#[derive(Clone, Debug)]
pub struct SymbolLocation {
    pub crate_name: String,
    pub module_path: String,
    pub source_file: PathBuf,
}

/// A parsed module's cached data.
struct ParsedModule {
    module: UnifiedModule,
}

/// Per-crate info.
pub struct CrateInfo {
    pub name: String,
    pub root: PathBuf,
    modules: RwLock<HashMap<String, ParsedModule>>,
}

/// Top-level crate database.
pub struct CrateDb {
    crates: RwLock<HashMap<String, CrateInfo>>,
    symbol_index: RwLock<HashMap<String, SymbolLocation>>,
    pub search_roots: Vec<PathBuf>,
}

impl CrateDb {
    pub fn new() -> Self {
        let mut search_roots = Vec::new();
        if let Ok(out) = std::process::Command::new("rustc")
            .arg("--print")
            .arg("sysroot")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        {
            let lib = PathBuf::from(&out).join("lib/rustlib/src/rust/library");
            if lib.exists() {
                search_roots.push(lib);
            }
        }
        let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor");
        if vendor.exists() {
            search_roots.push(vendor);
        }
        Self {
            crates: RwLock::new(HashMap::new()),
            symbol_index: RwLock::new(HashMap::new()),
            search_roots,
        }
    }

    pub fn register_crate(&self, name: &str, root: PathBuf) {
        self.crates.write().unwrap().insert(
            name.to_string(),
            CrateInfo {
                name: name.to_string(),
                root,
                modules: RwLock::new(HashMap::new()),
            },
        );
    }

    /// Resolve a fully-qualified name like "std::fs::read_to_string".
    /// Returns (crate, module_path, parsed_module).
    pub fn resolve(&self, fq_name: &str) -> Result<(String, String, Arc<UnifiedModule>), String> {
        if !fq_name
            .split("::")
            .all(|part| syn::parse_str::<syn::Ident>(part).is_ok())
        {
            return Err(format!("invalid qualified dependency name `{fq_name}`"));
        }
        // Check cache
        {
            let idx = self.symbol_index.read().unwrap();
            if let Some(loc) = idx.get(fq_name) {
                return self.get_module(&loc.crate_name, &loc.module_path);
            }
        }

        // Parse name to find owning crate and module
        let parts: Vec<&str> = fq_name.split("::").collect();
        if parts.len() < 2 {
            return Err(format!("not qualified: `{fq_name}`"));
        }
        let crate_name = parts[0].to_string();
        // Module path = all but last segment
        let module_path = if parts.len() == 2 {
            parts[0].to_string()
        } else {
            parts[..parts.len() - 1].join("::")
        };

        // Find source file
        let rel = module_path.replace("::", "/");
        let source_file = self
            .find_source(&crate_name, &rel)
            .ok_or_else(|| format!("no source for `{fq_name}` (crate={crate_name}, path={rel})"))?;

        // Cache the lookup
        self.symbol_index.write().unwrap().insert(
            fq_name.to_string(),
            SymbolLocation {
                crate_name: crate_name.clone(),
                module_path: module_path.clone(),
                source_file: source_file.clone(),
            },
        );

        self.get_module(&crate_name, &module_path)
    }

    fn get_module(
        &self,
        crate_name: &str,
        module_path: &str,
    ) -> Result<(String, String, Arc<UnifiedModule>), String> {
        // Check already-loaded
        {
            let crates = self.crates.read().unwrap();
            if let Some(ci) = crates.get(crate_name) {
                if let Some(pm) = ci.modules.read().unwrap().get(module_path) {
                    return Ok((
                        crate_name.to_string(),
                        module_path.to_string(),
                        Arc::new(pm.module.clone()),
                    ));
                }
            }
        }

        // Find and parse source file
        let rel = module_path.replace("::", "/");
        let source_file = self
            .find_source(crate_name, &rel)
            .ok_or_else(|| format!("source not found for `{crate_name}::{module_path}`"))?;

        // Parse via standard pipeline
        let resolved = ResolvedBuildParser::CoreIr(ParserId::Rust);
        let mut parsed = crate::parser_registry::parse_with_resolved(resolved, &source_file)
            .map_err(|e| format!("parse error {source_file:?}: {e}"))?
            .ok_or_else(|| format!("no module from {source_file:?}"))?;
        crate::core_bindings::qualify_module_decls(&mut parsed.decls, module_path);

        let exports: Vec<String> = parsed
            .decls
            .iter()
            .map(|d| match d {
                Decl::Function { name, .. }
                | Decl::Struct { name, .. }
                | Decl::Global { name, .. }
                | Decl::Class { name, .. }
                | Decl::Interface { name, .. }
                | Decl::Component { name, .. } => name.clone(),
            })
            .collect();

        // Index all exports
        {
            let mut idx = self.symbol_index.write().unwrap();
            for name in &exports {
                idx.entry(name.clone()).or_insert(SymbolLocation {
                    crate_name: crate_name.to_string(),
                    module_path: module_path.to_string(),
                    source_file: source_file.clone(),
                });
            }
        }

        // Cache
        {
            let crates = self.crates.read().unwrap();
            if let Some(ci) = crates.get(crate_name) {
                ci.modules.write().unwrap().insert(
                    module_path.to_string(),
                    ParsedModule {
                        module: parsed.clone(),
                    },
                );
            }
        }

        Ok((
            crate_name.to_string(),
            module_path.to_string(),
            Arc::new(parsed),
        ))
    }

    fn find_source(&self, crate_name: &str, rel: &str) -> Option<PathBuf> {
        let relative = if rel == crate_name {
            ""
        } else {
            rel.strip_prefix(&format!("{crate_name}/"))?
        };
        let relative = relative
            .split('/')
            .map(|part| part.strip_prefix("r#").unwrap_or(part))
            .collect::<Vec<_>>()
            .join("/");
        let mut roots = Vec::new();
        if let Some(info) = self.crates.read().unwrap().get(crate_name) {
            roots.push(info.root.clone());
        }
        roots.extend(self.search_roots.iter().map(|root| root.join(crate_name)));
        for root in roots {
            let candidates = if relative.is_empty() {
                vec![root.join("src/lib.rs"), root.join("src/main.rs")]
            } else {
                vec![
                    root.join("src").join(&relative).with_extension("rs"),
                    root.join("src").join(&relative).join("mod.rs"),
                ]
            };
            if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
                return Some(path);
            }
        }
        None
    }
}

impl Default for CrateDb {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_roots_follow_cargo_paths_and_cache_canonical_modules() {
        let root = std::env::temp_dir().join(format!("crate_db_namespace_{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn root()->i64{42}").unwrap();
        std::fs::write(src.join("foo.rs"), "pub fn answer()->i64{42}").unwrap();
        std::fs::write(src.join("a\u{301}.rs"), "pub fn answer()->i64{42}").unwrap();
        std::fs::write(src.join("type.rs"), "pub fn answer()->i64{42}").unwrap();
        std::fs::write(src.join("nested/mod.rs"), "pub fn answer()->i64{42}").unwrap();
        let mut db = CrateDb::new();
        db.search_roots.clear();
        db.register_crate("sample", root.clone());
        for name in [
            "sample::root",
            "sample::foo::answer",
            "sample::nested::answer",
            "sample::a\u{301}::answer",
            "sample::r#type::answer",
        ] {
            let (_, _, module) = db.resolve(name).unwrap();
            assert!(
                module.decls.iter().any(
                    |decl| matches!(decl, Decl::Function { name: found, .. } if found == name)
                )
            );
        }
        let (_, _, before) = db.resolve("sample::foo::answer").unwrap();
        std::fs::write(src.join("foo.rs"), "invalid Rust source").unwrap();
        assert_eq!(
            db.resolve("sample::foo::answer").unwrap().2.as_ref(),
            before.as_ref()
        );
        for name in [
            "sample::../escape::answer",
            "sample::::answer",
            "sample::foo/bar::answer",
            "sample::foo\\bar::answer",
        ] {
            assert!(db.resolve(name).is_err(), "{name}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_cratedb_new_initialization() {
        let db = CrateDb::new();
        assert!(
            db.crates.read().unwrap().is_empty(),
            "Crates should be empty"
        );
        assert!(
            db.symbol_index.read().unwrap().is_empty(),
            "Symbol index should be empty"
        );
    }

    #[test]
    fn test_cratedb_new_search_roots_initialization() {
        let db = CrateDb::new();
        // search_roots may contain `rustlib` and/or `vendor`.
        // Even if `rustc` is not available, we should at least not panic
        // and `search_roots` must not contain garbage paths.
        for path in &db.search_roots {
            assert!(
                path.exists(),
                "Every search root must be an existing directory: {:?}",
                path
            );
            assert!(path.is_dir(), "Search root must be a directory: {:?}", path);
        }
    }
}
