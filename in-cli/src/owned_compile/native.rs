use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
use crate::native_backend;
use crate::native_emit::lower::LoweringDegradation;
use crate::native_emit::{self, NativeLinkage, antidecomp};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use super::OwnedCompileRequest;
use super::jit::const_eval_entry_exit_code;
use super::metadata::emit_component_metadata_sidecar;
use super::util::{artifact_stem, path_extension_is};

pub struct NativeCompileResult {
    pub artifact_path: String,
    pub eval_exit_code: Option<u8>,
    pub eval_result: Option<i64>,
    pub eval_result_string: Option<String>,
    pub abi_path: Option<String>,
    pub backend_level: String,
    pub runtime_level: String,
    pub reason_code: String,
    pub reason: String,
    /// Code the lowerer could not compile and replaced with a runtime trap.
    pub degradations: Vec<LoweringDegradation>,
}

pub fn compile_native(
    module: &UnifiedModule,
    module_id: &str,
    request: &OwnedCompileRequest,
) -> Result<NativeCompileResult, String> {
    antidecomp::set_profile(request.profile);
    struct _ProfileGuard;
    impl Drop for _ProfileGuard {
        fn drop(&mut self) {
            antidecomp::clear_profile();
        }
    }
    let _profile_guard = _ProfileGuard;
    let entry = request
        .entry
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or("main");
    // For a static library every function is a potential export — callers may
    // resolve them from the object (e.g. assembly capsules calling `.in`
    // handlers). Do not drop functions that are unreachable from `entry`.
    let native_module = match request.linkage {
        NativeLinkage::StaticLib => module.clone(),
        _ => native_entry_module(module, entry),
    };
    let eval_exit = match request.linkage {
        NativeLinkage::Executable => {
            match native_entry_exit_code(&native_module, module_id, entry) {
                Ok(code) => Some(code),
                // The entry could not be executed to learn its exit code because
                // it reaches code the lowerer trapped. The executable's entry stub
                // derives the code at runtime, so report it as unknown rather than
                // inventing one.
                Err(err) if err.starts_with("entry-exit-unknown:") => None,
                Err(err) => return Err(err),
            }
        }
        NativeLinkage::Dylib | NativeLinkage::StaticLib => None,
    };
    let out_path = request
        .out
        .as_ref()
        .ok_or_else(|| "native compile requires --out executable path".to_string())?;
    if let Some(target_triple) = request.target_triple.as_deref() {
        let exit = if request.linkage == NativeLinkage::StaticLib {
            0
        } else {
            native_entry_exit_code(&native_module, module_id, entry)?
        };
        if request.linkage == NativeLinkage::Executable
            && target_triple == "aarch64-apple-darwin"
            && path_extension_is(out_path, "app")
        {
            let degradations = emit_macos_app_bundle(&native_module, module_id, entry, out_path)?;
            return Ok(NativeCompileResult {
                artifact_path: out_path.display().to_string(),
                eval_exit_code: Some(exit),
                eval_result: None,
                eval_result_string: None,
                abi_path: None,
                backend_level: "owned-native-subset-aarch64-app".to_string(),
                runtime_level: "macos-app-bundle".to_string(),
                reason_code: "native-aarch64-darwin-app-subset".to_string(),
                reason: "inauguration owns macOS .app bundle emission around its AArch64 Mach-O executable subset".to_string(),
                degradations,
            });
        }
        if request.linkage == NativeLinkage::Executable
            && target_triple == "x86_64-unknown-linux-gnu"
            && path_extension_is(out_path, "AppImage")
        {
            return Err("native-package-not-implemented: AppImage requires an owned AppImage runtime and SquashFS writer before this backend can claim .AppImage artifacts".to_string());
        }
        if request.linkage == NativeLinkage::Executable
            && target_triple == "x86_64-unknown-linux-gnu"
            && path_extension_is(out_path, "AppDir")
        {
            emit_linux_appdir(exit, out_path)?;
            return Ok(NativeCompileResult {
                artifact_path: out_path.display().to_string(),
                eval_exit_code: Some(exit),
                eval_result: None,
                eval_result_string: None,
                abi_path: None,
                backend_level: "owned-native-subset-x86_64-appdir".to_string(),
                runtime_level: "linux-appdir".to_string(),
                reason_code: "native-x86_64-linux-appdir-subset".to_string(),
                reason: "inauguration owns Linux AppDir emission around its x86_64 ELF executable subset".to_string(),
                degradations: Vec::new(),
            });
        }
        let object_request = native_emit::NativeObjectRequest {
            target_triple,
            linkage: request.linkage,
            entry,
            exit_code: exit,
            module,
            module_id,
            base: request.base,
        };
        if let Some(artifact) = native_emit::emit_native_object(&object_request) {
            fs::write(out_path, artifact.bytes)
                .map_err(|err| format!("native object write `{}`: {err}", out_path.display()))?;
            set_native_artifact_permissions(out_path, request.linkage)?;
            let abi_path = if let Some(manifest) = artifact.abi_manifest {
                let abi_path = out_path.with_extension("abi.json");
                fs::write(&abi_path, manifest)
                    .map_err(|err| format!("write abi manifest `{}`: {err}", abi_path.display()))?;
                Some(abi_path.display().to_string())
            } else {
                None
            };
            // Emit component metadata sidecar if the module has component declarations
            let _meta_path = emit_component_metadata_sidecar(module, entry, out_path);
            return Ok(NativeCompileResult {
                artifact_path: out_path.display().to_string(),
                eval_exit_code: if request.linkage == NativeLinkage::Executable {
                    Some(exit)
                } else {
                    None
                },
                eval_result: None,
                eval_result_string: None,
                abi_path,
                backend_level: artifact.backend_level.to_string(),
                runtime_level: artifact.runtime_level.to_string(),
                reason_code: artifact.reason_code.to_string(),
                reason: artifact.reason.to_string(),
                degradations: Vec::new(),
            });
        }
        return Err(format!(
            "native-target-not-implemented: target `{target_triple}` with linkage `{}` is not implemented by the owned backend",
            super::util::linkage_label(request.linkage)
        ));
    }
    let outcome = native_emit::compile_native_artifact_for_host_with_report(
        &native_module,
        module_id,
        entry,
        request.linkage,
        out_path,
    )?;
    set_native_artifact_permissions(out_path, request.linkage)?;
    let status = native_backend::native_backend_status();
    Ok(NativeCompileResult {
        artifact_path: out_path.display().to_string(),
        eval_exit_code: eval_exit,
        eval_result: None,
        eval_result_string: None,
        abi_path: outcome
            .abi_path
            .map(|path| path.display().to_string()),
        backend_level: "owned-native-subset".to_string(),
        runtime_level: "inrt-native".to_string(),
        reason_code: status.reason_code.to_string(),
        reason: status.reason.to_string(),
        degradations: outcome.degradations,
    })
}

fn native_entry_exit_code(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
) -> Result<u8, String> {
    let returns_exit_code = module.decls.iter().any(|decl| {
        matches!(decl, Decl::Function { name, ret, .. } if name == entry && matches!(ret.canonical(), Typ::Int | Typ::Bool))
    });
    if returns_exit_code {
        const_eval_entry_exit_code(module, module_id, entry)
    } else {
        Ok(0)
    }
}

/// Normalize a name by removing spaces around :: separators
fn normalize_name(name: &str) -> String {
    name.chars().filter(|&c| c != ' ').collect()
}

fn collect_expr_calls(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_ref() {
                out.insert(normalize_name(name));
            } else {
                collect_expr_calls(callee, out);
            }
            for arg in args {
                collect_expr_calls(arg, out);
            }
        }
        Expr::Unary { expr, .. } => collect_expr_calls(expr, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_calls(lhs, out);
            collect_expr_calls(rhs, out);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_expr_calls(expr, out);
            }
        }
        Expr::Field { base, .. } => collect_expr_calls(base, out),
        Expr::ArrayLit(items) => {
            for item in items {
                collect_expr_calls(item, out);
            }
        }
        Expr::Index { base, index, .. } => {
            collect_expr_calls(base, out);
            collect_expr_calls(index, out);
        }
        Expr::Closure { body, .. } => collect_stmt_calls(body, out),
        Expr::Ident(name) => {
            // Also track function names used as values (address references)
            // The caller will filter these against module function names.
            out.insert(normalize_name(name));
        }
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => {}
    }
}

fn collect_stmt_calls(stmts: &[Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Return(Some(expr))
            | Stmt::Expr(expr)
            | Stmt::Throw(expr) => collect_expr_calls(expr, out),
            Stmt::FieldAssign { base, value, .. } => {
                collect_expr_calls(base, out);
                collect_expr_calls(value, out);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                collect_expr_calls(base, out);
                collect_expr_calls(index, out);
                collect_expr_calls(value, out);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_expr_calls(cond, out);
                collect_stmt_calls(then_body, out);
                collect_stmt_calls(else_body, out);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    collect_expr_calls(cond, out);
                }
                collect_stmt_calls(body, out);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                collect_expr_calls(scrutinee, out);
                for arm in arms {
                    collect_stmt_calls(&arm.body, out);
                }
            }
            Stmt::Try { body, catches, .. } => {
                collect_stmt_calls(body, out);
                for catch in catches {
                    collect_stmt_calls(&catch.body, out);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Propagate => {}
        }
    }
}

pub fn native_entry_module(module: &UnifiedModule, entry: &str) -> UnifiedModule {
    let mut reachable = HashSet::from([entry.to_string()]);
    loop {
        let mut next = reachable.clone();
        for decl in &module.decls {
            let Decl::Function { name, body, .. } = decl else {
                continue;
            };
            if reachable.contains(name)
                || reachable.iter().any(|r| r.ends_with(&format!("::{name}")))
            {
                collect_stmt_calls(body, &mut next);
            }
        }
        if next.len() == reachable.len() {
            break;
        }
        reachable = next;
    }
    UnifiedModule::new(
        module
            .decls
            .iter()
            .filter(|decl| match decl {
                Decl::Function { name, .. } => {
                    reachable.contains(name)
                        || reachable.iter().any(|r| r.ends_with(&format!("::{name}")))
                }
                _ => true,
            })
            .cloned()
            .collect(),
    )
}

pub fn emit_macos_app_bundle(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
    out_path: &Path,
) -> Result<Vec<LoweringDegradation>, String> {
    let name = artifact_stem(out_path, "App");
    let contents = out_path.join("Contents");
    let macos = contents.join("MacOS");
    fs::create_dir_all(&macos)
        .map_err(|err| format!("create app bundle `{}`: {err}", macos.display()))?;
    let executable = macos.join(&name);
    let outcome = native_emit::compile_native_artifact_with_report(
        module,
        module_id,
        entry,
        NativeLinkage::Executable,
        &executable,
    )?;
    set_native_artifact_permissions(&executable, NativeLinkage::Executable)?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n<key>CFBundleExecutable</key>\n<string>{name}</string>\n<key>CFBundleIdentifier</key>\n<string>inauguration.{name}</string>\n<key>CFBundleName</key>\n<string>{name}</string>\n<key>CFBundlePackageType</key>\n<string>APPL</string>\n</dict>\n</plist>\n"
    );
    fs::write(contents.join("Info.plist"), plist)
        .map_err(|err| format!("write app Info.plist `{}`: {err}", out_path.display()))?;
    fs::write(contents.join("PkgInfo"), "APPL????")
        .map_err(|err| format!("write app PkgInfo `{}`: {err}", out_path.display()))?;
    Ok(outcome.degradations)
}

pub fn emit_linux_appdir(exit: u8, out_path: &Path) -> Result<(), String> {
    fs::create_dir_all(out_path)
        .map_err(|err| format!("create AppDir `{}`: {err}", out_path.display()))?;
    let app_run = out_path.join("AppRun");
    let exe = native_emit::ElfExecutable {
        code: native_emit::x86_64_linux_exit_code(exit),
        entry_offset: 0,
    };
    let mut bytes = Vec::new();
    native_emit::write_elf_executable(&exe, &mut bytes);
    fs::write(&app_run, bytes)
        .map_err(|err| format!("write AppRun `{}`: {err}", app_run.display()))?;
    set_native_artifact_permissions(&app_run, NativeLinkage::Executable)?;
    fs::write(
        out_path.join("answer.desktop"),
        "[Desktop Entry]\nType=Application\nName=answer\nExec=AppRun\n",
    )
    .map_err(|err| format!("write AppDir desktop file `{}`: {err}", out_path.display()))
}

#[cfg(unix)]
pub fn set_native_artifact_permissions(path: &Path, linkage: NativeLinkage) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|err| format!("native artifact metadata: {err}"))?
        .permissions();
    perms.set_mode(match linkage {
        NativeLinkage::StaticLib => 0o644,
        NativeLinkage::Executable | NativeLinkage::Dylib => 0o755,
    });
    fs::set_permissions(path, perms).map_err(|err| format!("chmod native artifact: {err}"))
}

#[cfg(not(unix))]
pub fn set_native_artifact_permissions(
    _path: &Path,
    _linkage: NativeLinkage,
) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_entry_uses_zero_exit_code_without_jit_execution() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".to_string(),
                params: vec![],
                ret: Typ::String,
                body: vec![Stmt::Return(Some(Expr::StringLit("value".to_string())))],
                type_params: vec![],
            }],
        };

        assert_eq!(native_entry_exit_code(&module, "App", "main"), Ok(0));
    }
}
