use crate::owned_compile::OwnedCompileReport;
use std::fs;
use std::path::{Path, PathBuf};

const CACHE_SCHEMA_VERSION: u32 = 1;

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
    eval_result: Option<i64>,
    #[serde(default)]
    eval_result_string: Option<String>,
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
            eval_result: report.eval_result,
            eval_result_string: report.eval_result_string.clone(),
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
            eval_result_string: cached.eval_result_string,
            error: cached.error,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompileCacheMetadata {
    schema_version: u32,
    frontend_hash: String,
    report: CachedOwnedCompileReport,
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
    Some(metadata.report.into())
}

pub fn write_cached_report(
    cwd: &Path,
    frontend_hash: &str,
    report: &OwnedCompileReport,
) -> Result<(), String> {
    let dir = cache_dir_for_hash(cwd, frontend_hash);
    fs::create_dir_all(&dir).map_err(|err| format!("create compile cache dir: {err}"))?;
    let metadata = CompileCacheMetadata {
        schema_version: CACHE_SCHEMA_VERSION,
        frontend_hash: frontend_hash.to_string(),
        report: report.into(),
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
    fn roundtrip_cache_metadata() {
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
            eval_result_string: None,
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
        let _ = fs::remove_dir_all(cwd);
    }
}
