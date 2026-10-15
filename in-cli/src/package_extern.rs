use std::fs;
use std::path::Path;

use crate::in_lang_parse::InExternBinding;
use crate::package_install::{INSTALLED_PACKAGE_METADATA, InstalledPackageMetadata};
use crate::package_lock::PackageLock;
use crate::package_manifest::{
    PackageDependency, PackageManifest, PackageSemanticImport, resolve_dependency_install_path,
};

pub fn extern_language_for_ecosystem(ecosystem: &str) -> &'static str {
    match ecosystem {
        "cargo" => "rust",
        "npm" => "js",
        "pypi" => "python",
        "go" => "go",
        "composer" => "php",
        "nuget" => "dotnet",
        "gem" => "ruby",
        "hex" => "elixir",
        "v" => "v",
        _ => "package",
    }
}

pub fn load_installed_exports(install_path: &Path) -> Vec<String> {
    let metadata_path = install_path.join(INSTALLED_PACKAGE_METADATA);
    let Ok(source) = fs::read_to_string(&metadata_path) else {
        return Vec::new();
    };
    let Ok(metadata) = serde_json::from_str::<InstalledPackageMetadata>(&source) else {
        return Vec::new();
    };
    metadata.exports
}

pub fn export_symbols_for_resolved_import(
    import: &PackageSemanticImport,
    package_root: &Path,
    manifest: &PackageManifest,
    lock: Option<&PackageLock>,
) -> Vec<String> {
    let Some(dependency_key) = import.dependency.as_ref() else {
        return Vec::new();
    };
    let Some(dependency) = manifest.dependencies.get(dependency_key) else {
        return Vec::new();
    };
    let Some(install_path) =
        resolve_dependency_install_path(package_root, dependency_key, dependency, lock)
    else {
        return Vec::new();
    };
    load_installed_exports(&install_path)
}

pub fn package_import_bindings_for_semantic_import(
    import: &str,
    package_root: &Path,
    manifest: &PackageManifest,
    lock: Option<&PackageLock>,
) -> Vec<InExternBinding> {
    let resolved =
        crate::package_manifest::resolve_semantic_imports(&[import.to_string()], Some(manifest))
            .into_iter()
            .next()
            .unwrap_or(crate::package_manifest::PackageSemanticImport {
                import: import.to_string(),
                dependency: None,
                status: "unresolved".to_string(),
                reason: "package-manifest-missing".to_string(),
            });
    if resolved.status != "resolved" {
        return Vec::new();
    }
    let dependency_key = match resolved.dependency.as_ref() {
        Some(key) => key,
        None => return Vec::new(),
    };
    let dependency = match manifest.dependencies.get(dependency_key) {
        Some(dep) => dep,
        None => return Vec::new(),
    };
    bindings_for_dependency(package_root, dependency_key, dependency, lock)
}

pub fn package_import_bindings_for_source(source_path: &Path) -> Vec<InExternBinding> {
    let Ok((root, manifest)) =
        crate::package_manifest::load_package_manifest_from_source(source_path)
    else {
        return Vec::new();
    };
    let lock = crate::package_lock::discover_package_lock(&root.root)
        .and_then(|lock_root| crate::package_lock::load_package_lock(&lock_root.lock_path).ok());
    let source_text = match fs::read_to_string(source_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[package-extern] warning: cannot read {}: {e}",
                source_path.display()
            );
            return Vec::new();
        }
    };
    let Ok(surface) = crate::in_lang_parse::parse_in_surface_info(&source_text) else {
        return Vec::new();
    };
    let mut bindings = Vec::new();
    for import in &surface.semantic_imports {
        bindings.extend(package_import_bindings_for_semantic_import(
            import,
            &root.root,
            &manifest,
            lock.as_ref(),
        ));
    }
    bindings
}

fn bindings_for_dependency(
    package_root: &Path,
    dependency_key: &str,
    dependency: &PackageDependency,
    lock: Option<&PackageLock>,
) -> Vec<InExternBinding> {
    let install_path =
        match resolve_dependency_install_path(package_root, dependency_key, dependency, lock) {
            Some(path) => path,
            None => return Vec::new(),
        };
    let metadata_path = install_path.join(INSTALLED_PACKAGE_METADATA);
    let metadata = match fs::read_to_string(&metadata_path)
        .ok()
        .and_then(|source| serde_json::from_str::<InstalledPackageMetadata>(&source).ok())
    {
        Some(metadata) => metadata,
        None => {
            let package_ref =
                crate::package_ref::package_ref_for_dependency(dependency_key, dependency);
            let language = package_ref
                .as_ref()
                .map(|package_ref| {
                    extern_language_for_ecosystem(&package_ref.ecosystem).to_string()
                })
                .unwrap_or_else(|| "package".to_string());
            let exports = package_ref
                .map(|package_ref| vec![crate::package_install::export_symbol_for(&package_ref)])
                .unwrap_or_default();
            return exports
                .into_iter()
                .map(|name| InExternBinding {
                    language: language.clone(),
                    params: vec![],
                    name,
                    required_capabilities: vec!["package.invoke".into()],
                    ret: None,
                })
                .collect();
        }
    };
    metadata
        .bindings
        .iter()
        .map(|binding| InExternBinding {
            language: extern_language_for_ecosystem(&metadata.ecosystem).to_string(),
            name: binding.symbol.clone(),
            params: vec![],
            required_capabilities: vec!["package.invoke".into()],
            ret: crate::package_runtime::binding_return_type(binding),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_install::INSTALLED_PACKAGE_METADATA;
    use crate::package_manifest::PACKAGE_MANIFEST_FILE;

    #[test]
    fn maps_ecosystem_to_extern_language() {
        assert_eq!(extern_language_for_ecosystem("cargo"), "rust");
        assert_eq!(extern_language_for_ecosystem("npm"), "js");
        assert_eq!(extern_language_for_ecosystem("pypi"), "python");
        assert_eq!(extern_language_for_ecosystem("go"), "go");
        assert_eq!(extern_language_for_ecosystem("composer"), "php");
        assert_eq!(extern_language_for_ecosystem("nuget"), "dotnet");
        assert_eq!(extern_language_for_ecosystem("gem"), "ruby");
        assert_eq!(extern_language_for_ecosystem("hex"), "elixir");
        assert_eq!(extern_language_for_ecosystem("v"), "v");
        assert_eq!(extern_language_for_ecosystem("unknown"), "package");
        assert_eq!(extern_language_for_ecosystem(""), "package");
    }

    #[test]
    fn synthesizes_bindings_from_installed_metadata() {
        let temp = std::env::temp_dir().join(format!(
            "package-extern-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let install_path = temp.join("vendor/pypi/flask");
        fs::create_dir_all(&install_path).expect("vendor dir");
        fs::write(
            install_path.join(INSTALLED_PACKAGE_METADATA),
            r#"{"ecosystem":"pypi","name":"flask","version":"vendor","registry":"path","install_path":"vendor/pypi/flask","exports":["flask_greet","env-temp-dir","read-file"],"bindings":[{"symbol":"flask_greet","returns":"string","invoke":{"program":"python3","args":["-c","from greet import greet; import sys; sys.stdout.write(greet())"]}},{"symbol":"env-temp-dir","returns":"int","invoke":{"program":"python3","args":["-c","print(42)"]}},{"symbol":"read-file","returns":"bool","invoke":{"program":"python3","args":["-c","print(True)"]}}]}"#,
        )
        .expect("metadata");
        fs::write(
            install_path.join("greet.py"),
            "def greet():\n    return 'flask'\n",
        )
        .expect("greet");
        fs::write(
            temp.join(PACKAGE_MANIFEST_FILE),
            "name: demo\nversion: 0.1.0\ndependencies:\n  pypi:flask:\n    version: path:vendor/pypi/flask\n    kind: pypi\n",
        )
        .expect("manifest");
        let manifest = crate::package_manifest::load_package_manifest(&temp).expect("manifest");
        let bindings =
            package_import_bindings_for_semantic_import("pip:flask", &temp, &manifest, None);
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].name, "flask_greet");
        assert_eq!(bindings[0].language, "python");
        for (binding, expected) in bindings.iter().zip([
            crate::core_ir::Typ::String,
            crate::core_ir::Typ::Int,
            crate::core_ir::Typ::Bool,
        ]) {
            let declaration = crate::in_lang_parse::surface::binding_decl(binding);
            assert!(
                matches!(declaration, crate::core_ir::Decl::Function { params, ret, .. }
                if params.is_empty() && ret == expected),
                "{binding:?}"
            );
        }
        let _ = fs::remove_dir_all(temp);
    }
}
