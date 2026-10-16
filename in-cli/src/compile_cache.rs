use crate::owned_compile::OwnedCompileReport;
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_SCHEMA_VERSION: u32 = 2;

fn default_cached_linkage() -> String {
    "executable".to_string()
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedOwnedCompileReport {
    schema_version: u32,
    owned: bool,
    path: String,
    module_id: String,
    #[serde(default)]
    package_name: Option<String>,
    #[serde(default)]
    module_identity: Option<crate::core_ir::ModuleIdentityReport>,
    target: String,
    #[serde(default)]
    target_triple: Option<String>,
    entry: Option<String>,
    #[serde(default = "default_cached_linkage")]
    linkage: String,
    frontend_level: String,
    semantic_level: String,
    backend_level: String,
    runtime_level: String,
    external_invocations: Vec<String>,
    reason_code: Option<String>,
    reason: Option<String>,
    success: bool,
    artifact_path: Option<String>,
    executable_path: Option<String>,
    abi_path: Option<String>,
    #[serde(default)]
    degradations: Vec<crate::native_emit::lower::LoweringDegradation>,
    parsed_function_count: usize,
    typed_function_count: usize,
    call_edge_count: usize,
    jobs: usize,
    timing_micros: u128,
    timing_waves_us: Option<Vec<u128>>,
    cache_hit: bool,
    frontend_hash: Option<String>,
    eval_exit_code: Option<u8>,
    #[serde(default)]
    eval_result: Option<crate::owned_compile::EvalValue>,
    #[serde(default)]
    execution_started: bool,
    error: Option<String>,
}

impl From<&OwnedCompileReport> for CachedOwnedCompileReport {
    fn from(report: &OwnedCompileReport) -> Self {
        Self {
            schema_version: report.schema_version,
            owned: report.owned,
            path: report.path.clone(),
            module_id: report.module_id.clone(),
            module_identity: report.module_identity.clone(),
            package_name: report.package_name.clone(),
            target: report.target.clone(),
            target_triple: report.target_triple.clone(),
            entry: report.entry.clone(),
            linkage: report.linkage.clone(),
            frontend_level: report.frontend_level.clone(),
            semantic_level: report.semantic_level.clone(),
            backend_level: report.backend_level.clone(),
            runtime_level: report.runtime_level.clone(),
            external_invocations: report.external_invocations.clone(),
            reason_code: report.reason_code.clone(),
            reason: report.reason.clone(),
            success: report.success,
            artifact_path: report.artifact_path.clone(),
            executable_path: report.executable_path.clone(),
            abi_path: report.abi_path.clone(),
            degradations: report.degradations.clone(),
            parsed_function_count: report.parsed_function_count,
            typed_function_count: report.typed_function_count,
            call_edge_count: report.call_edge_count,
            jobs: report.jobs,
            timing_micros: report.timing_micros,
            timing_waves_us: report.timing_waves_us.clone(),
            cache_hit: report.cache_hit,
            frontend_hash: report.frontend_hash.clone(),
            eval_exit_code: report.eval_exit_code,
            eval_result: report.eval_result.clone(),
            execution_started: report.execution_started,
            error: report.error.clone(),
        }
    }
}

impl From<CachedOwnedCompileReport> for OwnedCompileReport {
    fn from(cached: CachedOwnedCompileReport) -> Self {
        Self {
            schema_version: cached.schema_version,
            owned: cached.owned,
            path: cached.path,
            module_id: cached.module_id,
            module_identity: cached.module_identity,
            package_name: cached.package_name,
            target: cached.target,
            target_triple: cached.target_triple,
            entry: cached.entry,
            linkage: cached.linkage,
            frontend_level: cached.frontend_level,
            semantic_level: cached.semantic_level,
            backend_level: cached.backend_level,
            runtime_level: cached.runtime_level,
            external_invocations: cached.external_invocations,
            reason_code: cached.reason_code,
            reason: cached.reason,
            success: cached.success,
            artifact_path: cached.artifact_path,
            executable_path: cached.executable_path,
            abi_path: cached.abi_path,
            degradations: cached.degradations,
            parsed_function_count: cached.parsed_function_count,
            typed_function_count: cached.typed_function_count,
            call_edge_count: cached.call_edge_count,
            jobs: cached.jobs,
            timing_micros: cached.timing_micros,
            timing_waves_us: cached.timing_waves_us,
            cache_hit: cached.cache_hit,
            frontend_hash: cached.frontend_hash,
            eval_exit_code: cached.eval_exit_code,
            eval_result: cached.eval_result,
            execution_started: cached.execution_started,
            error: cached.error,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompileCacheMetadata {
    schema_version: u32,
    frontend_hash: String,
    report: CachedOwnedCompileReport,
    output_fingerprints: Vec<(String, u64)>,
}

pub fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn compiler_build_fingerprint() -> u64 {
    let salt = std::env::current_exe()
        .ok()
        .and_then(|path| {
            let meta = fs::metadata(&path).ok()?;
            let modified = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos();
            Some(format!("{}:{}:{modified}", path.display(), meta.len()))
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    fnv1a_hash(salt.as_bytes())
}

pub fn source_frontend_hash(path: &Path, content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= 0xff;
    let content_hash = fnv1a_hash(content.as_bytes());
    hash = hash.wrapping_mul(0x100000001b3) ^ content_hash;
    hash = hash.wrapping_mul(0x100000001b3) ^ compiler_build_fingerprint();
    format!("{hash:016x}")
}

pub fn resolved_frontend_hash(
    source_fingerprint: &str,
    module: &crate::core_ir::UnifiedModule,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    "resolved-core-ir-v2".hash(&mut hash);
    source_fingerprint.hash(&mut hash);
    module.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

fn output_fingerprints(report: &OwnedCompileReport) -> Result<Vec<(String, u64)>, String> {
    [
        &report.artifact_path,
        &report.executable_path,
        &report.abi_path,
    ]
    .into_iter()
    .filter_map(|path| path.as_ref())
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .map(|path| {
        let bytes =
            fs::read(path).map_err(|error| format!("read cached output `{path}`: {error}"))?;
        Ok((path.clone(), fnv1a_hash(&bytes)))
    })
    .collect()
}

pub fn cache_root(cwd: &Path) -> PathBuf {
    cwd.join("target").join("in").join("cache")
}

pub fn cache_dir_for_hash(cwd: &Path, frontend_hash: &str) -> PathBuf {
    cache_root(cwd).join(frontend_hash)
}

pub fn read_cached_report(cwd: &Path, frontend_hash: &str) -> Option<OwnedCompileReport> {
    let path = cache_dir_for_hash(cwd, frontend_hash).join("metadata.json");
    let raw = fs::read_to_string(&path).ok()?;
    let metadata: CompileCacheMetadata = serde_json::from_str(&raw).ok()?;
    if metadata.schema_version != CACHE_SCHEMA_VERSION {
        return None;
    }
    if metadata.frontend_hash != frontend_hash {
        return None;
    }
    let report = metadata.report.into();
    if output_fingerprints(&report).ok()? != metadata.output_fingerprints {
        return None;
    }
    Some(report)
}

pub fn write_cached_report(
    cwd: &Path,
    frontend_hash: &str,
    report: &OwnedCompileReport,
) -> Result<(), String> {
    // Directory bundles are rebuilt; this cache validates individual file outputs.
    if [
        &report.artifact_path,
        &report.executable_path,
        &report.abi_path,
    ]
    .into_iter()
    .filter_map(|path| path.as_ref())
    .any(|path| Path::new(path).is_dir())
    {
        return Ok(());
    }
    let dir = cache_dir_for_hash(cwd, frontend_hash);
    fs::create_dir_all(&dir).map_err(|err| format!("create compile cache dir: {err}"))?;
    let metadata = CompileCacheMetadata {
        schema_version: CACHE_SCHEMA_VERSION,
        frontend_hash: frontend_hash.to_string(),
        report: report.into(),
        output_fingerprints: output_fingerprints(report)?,
    };
    let raw =
        serde_json::to_string_pretty(&metadata).map_err(|err| format!("serialize cache: {err}"))?;
    fs::write(dir.join("metadata.json"), raw)
        .map_err(|err| format!("write compile cache metadata: {err}"))
}

pub fn workspace_cwd_for_path(source_path: &Path) -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .canonicalize()
        .ok()
        .filter(|cwd| source_path.starts_with(cwd))
        .unwrap_or_else(|| {
            source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owned_compile::OwnedCompileReport;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "inauguration-compile-cache-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn resolved_module_changes_invalidate_the_frontend_key() {
        use crate::core_ir::{Decl, Expr, Typ, UnifiedModule};
        let first = UnifiedModule::new(vec![Decl::Global {
            name: "dependency::VALUE".into(),
            typ: Typ::Int,
            mutable: false,
            constant_value: true,
            init: Some(Box::new(Expr::IntLit(22))),
        }]);
        let mut changed = first.clone();
        let Decl::Global { init, .. } = &mut changed.decls[0] else {
            unreachable!()
        };
        *init = Some(Box::new(Expr::IntLit(24)));
        let source_hash = source_frontend_hash(
            Path::new("unchanged.in"),
            "fn main()->Int{return dependency::VALUE;}",
        );
        let initial = resolved_frontend_hash(&source_hash, &first);
        assert_eq!(
            initial,
            resolved_frontend_hash(&source_hash, &first.clone())
        );
        assert_ne!(initial, resolved_frontend_hash(&source_hash, &changed));
        assert_ne!(
            initial,
            resolved_frontend_hash(&format!("{source_hash}:Lean"), &first)
        );
    }

    #[test]
    fn resolved_keys_include_bodies_schemas_and_float_bits() {
        use crate::core_ir::{Decl, Expr, FloatVal, Stmt, Typ, UnifiedModule};
        let module = |value| {
            UnifiedModule::new(vec![
                Decl::Function {
                    name: "dependency::answer".into(),
                    params: vec![],
                    ret: Typ::Float,
                    body: vec![Stmt::Return(Some(Expr::FloatLit(FloatVal(value))))],
                    type_params: vec![],
                },
                Decl::Struct {
                    name: "dependency::Entry".into(),
                    fields: vec![("value".into(), Typ::Int)],
                    type_params: vec![],
                },
            ])
        };
        let first = module(22.0);
        let key = resolved_frontend_hash("same-source", &first);
        assert_ne!(key, resolved_frontend_hash("same-source", &module(24.0)));
        let mut schema = first.clone();
        let Decl::Struct { fields, .. } = &mut schema.decls[1] else {
            unreachable!()
        };
        fields[0].1 = Typ::Bool;
        assert_ne!(key, resolved_frontend_hash("same-source", &schema));
        let nan_a = module(f64::from_bits(0x7ff8_0000_0000_0001));
        let nan_b = module(f64::from_bits(0x7ff8_0000_0000_0002));
        assert_eq!(format!("{nan_a:?}"), format!("{nan_b:?}"));
        assert_ne!(
            resolved_frontend_hash("same-source", &nan_a),
            resolved_frontend_hash("same-source", &nan_b)
        );
    }

    #[test]
    fn hash_is_stable_for_same_path_and_content() {
        let path = Path::new("apps/sample.in");
        let a = source_frontend_hash(path, "fn main() -> void { return; }\n");
        let b = source_frontend_hash(path, "fn main() -> void { return; }\n");
        assert_eq!(a, b);
        assert_ne!(
            a,
            source_frontend_hash(path, "fn main() -> void { return; }")
        );
    }

    #[test]
    fn roundtrip_cache_metadata_rejects_changed_or_missing_outputs() {
        let cwd = temp_dir("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let hash = "abc123deadbeef00";
        let report = OwnedCompileReport {
            schema_version: 1,
            owned: true,
            path: "sample.in".to_string(),
            module_id: "App".to_string(),
            package_name: None,
            module_identity: Some(crate::core_ir::ModuleIdentityReport {
                package: Some("agents.video".to_string()),
                module: Some("agents.video.main".to_string()),
                requested_module_id: "App".to_string(),
                effective_module_id: "agents.video.main".to_string(),
            }),
            target: "jit".to_string(),
            target_triple: None,
            entry: None,
            linkage: "executable".to_string(),
            frontend_level: "core-ir-direct".to_string(),
            semantic_level: "typed-subset".to_string(),
            backend_level: "owned-native-subset".to_string(),
            runtime_level: "inrt-jit".to_string(),
            external_invocations: Vec::new(),
            reason_code: None,
            reason: None,
            success: true,
            artifact_path: None,
            executable_path: None,
            abi_path: None,
            degradations: Vec::new(),
            parsed_function_count: 1,
            typed_function_count: 1,
            call_edge_count: 0,
            jobs: 1,
            timing_micros: 10,
            timing_waves_us: None,
            cache_hit: false,
            frontend_hash: Some(hash.to_string()),
            eval_exit_code: None,
            eval_result: None,
            execution_started: false,
            error: None,
        };
        write_cached_report(&cwd, hash, &report).unwrap();
        let loaded = read_cached_report(&cwd, hash).expect("cached report");
        assert!(loaded.success);
        assert_eq!(loaded.frontend_hash.as_deref(), Some(hash));
        assert_eq!(
            loaded
                .module_identity
                .as_ref()
                .and_then(|identity| identity.package.as_deref()),
            Some("agents.video")
        );
        assert_eq!(
            loaded
                .module_identity
                .as_ref()
                .map(|identity| identity.effective_module_id.as_str()),
            Some("agents.video.main")
        );
        let output = cwd.join("app.native");
        let abi = cwd.join("app.abi.json");
        fs::write(&output, b"initial executable").unwrap();
        fs::write(&abi, b"initial ABI").unwrap();
        let mut report = report;
        report.artifact_path = Some(output.display().to_string());
        report.executable_path = report.artifact_path.clone();
        report.abi_path = Some(abi.display().to_string());
        assert_eq!(output_fingerprints(&report).unwrap().len(), 2);
        write_cached_report(&cwd, hash, &report).unwrap();
        assert!(read_cached_report(&cwd, hash).is_some());
        fs::write(&output, b"another profile overwrote the executable").unwrap();
        assert!(read_cached_report(&cwd, hash).is_none());
        fs::write(&output, b"initial executable").unwrap();
        assert!(read_cached_report(&cwd, hash).is_some());
        fs::write(&abi, b"modified ABI").unwrap();
        assert!(read_cached_report(&cwd, hash).is_none());
        fs::write(&abi, b"initial ABI").unwrap();
        fs::remove_file(&output).unwrap();
        assert!(read_cached_report(&cwd, hash).is_none());
        fs::create_dir(&output).unwrap();
        write_cached_report(&cwd, "directory-output", &report).unwrap();
        assert!(read_cached_report(&cwd, "directory-output").is_none());
        let _ = fs::remove_dir_all(cwd);
    }
}
