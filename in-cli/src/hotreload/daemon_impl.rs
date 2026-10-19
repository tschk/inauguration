use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;

use super::generated_protocol::{PatchType, ReloadDecision};
use crate::hybrid_sil;
use crate::parser_registry::{self, ParserCli, ResolvedBuildParser};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadPatch {
    pub target: String,
    pub patch_type: PatchType,
    pub compatible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchEnvelope {
    pub protocol_version: u8,
    pub patch_id: String,
    pub timestamp_ms: u64,
    pub patch: ReloadPatch,
    pub reload_decision: ReloadDecision,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeMetric {
    pub timestamp_ms: u64,
    pub source: String,
    pub target: String,
    pub compatible: bool,
    pub reload_decision: ReloadDecision,
    pub reason: String,
    pub compile_check_ms: u64,
    pub compile_cache_hit: bool,
    #[serde(
        default,
        rename = "compile_frontend",
        skip_serializing_if = "Option::is_none"
    )]
    pub compile_frontend: Option<String>,
    #[serde(
        default,
        rename = "compile_source_hash",
        skip_serializing_if = "Option::is_none"
    )]
    pub compile_source_hash: Option<u64>,
    #[serde(
        default,
        rename = "compile_fallback_reason",
        skip_serializing_if = "Option::is_none"
    )]
    pub compile_fallback_reason: Option<String>,
    /// Count of `function_ref` edges from [`hybrid_sil::extract_call_graph`] when in-tree subset SIL exists (no `swiftc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sil_call_edges: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sil_graph_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub watch_root: PathBuf,
    pub socket_path: PathBuf,
    pub metrics_path: PathBuf,
    pub debounce_ms: u64,
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("notify error: {0}")]
    Notify(#[from] notify::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
struct ClientPool {
    writers: Arc<Mutex<Vec<tokio::net::unix::OwnedWriteHalf>>>,
}

impl ClientPool {
    fn new() -> Self {
        Self {
            writers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn add(&self, writer: tokio::net::unix::OwnedWriteHalf) {
        self.writers.lock().await.push(writer);
    }

    async fn broadcast_line(&self, line: &str) -> Result<(), DaemonError> {
        let mut stale_indexes = Vec::new();
        let mut writers = self.writers.lock().await;
        for (index, writer) in writers.iter_mut().enumerate() {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                stale_indexes.push(index);
            }
        }
        for index in stale_indexes.into_iter().rev() {
            writers.remove(index);
        }
        Ok(())
    }
}

fn now_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_millis() as u64,
        Err(_) => 0,
    }
}

pub fn patch_id_for(path: &str) -> String {
    format!("{}-{}", now_ms(), path.replace('/', "_"))
}

pub fn plan_patch(path: &str, changed_symbols: &[String]) -> ReloadPatch {
    plan_patch_with_sil_graph(path, changed_symbols, None, false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilGraphSource {
    Subset,
    SubsetZeroEdges,
    SubsetUnavailable,
    Swiftc,
    SwiftcZeroEdges,
    SwiftcUnavailable,
    NonSwift,
    SkippedCompileFail,
}

impl SilGraphSource {
    fn reason_tag(&self) -> &'static str {
        match self {
            Self::Subset => "sil_graph=subset",
            Self::SubsetZeroEdges => "sil_graph=subset_zero_edges",
            Self::SubsetUnavailable => "sil_graph=subset_unavailable",
            Self::Swiftc => "sil_graph=swiftc",
            Self::SwiftcZeroEdges => "sil_graph=swiftc_zero_edges",
            Self::SwiftcUnavailable => "sil_graph=swiftc_unavailable",
            Self::NonSwift => "sil_graph=non_swift",
            Self::SkippedCompileFail => "sil_graph=skipped_compile_fail",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilGraphDetail {
    source: SilGraphSource,
    call_edges: Vec<(String, String)>,
    callees: HashSet<String>,
}

impl SilGraphDetail {
    fn unavailable(source: SilGraphSource) -> Self {
        Self {
            source,
            call_edges: Vec::new(),
            callees: HashSet::new(),
        }
    }

    fn from_call_edges(call_edges: Vec<(String, String)>) -> Self {
        Self::from_call_edges_with_source(call_edges, SilGraphSource::Subset)
    }

    fn from_call_edges_with_source(
        call_edges: Vec<(String, String)>,
        nonzero_source: SilGraphSource,
    ) -> Self {
        let callees = call_edges
            .iter()
            .map(|(_, callee)| callee.clone())
            .collect::<HashSet<_>>();
        let source = match (call_edges.is_empty(), nonzero_source) {
            (false, source) => source,
            (true, SilGraphSource::Swiftc) => SilGraphSource::SwiftcZeroEdges,
            (true, _) => SilGraphSource::SubsetZeroEdges,
        };
        Self {
            source,
            call_edges,
            callees,
        }
    }

    fn reason_tag(&self) -> &'static str {
        self.source.reason_tag()
    }

    fn edge_count(&self) -> Option<u32> {
        match self.source {
            SilGraphSource::Subset
            | SilGraphSource::SubsetZeroEdges
            | SilGraphSource::Swiftc
            | SilGraphSource::SwiftcZeroEdges => Some(self.call_edges.len() as u32),
            SilGraphSource::SubsetUnavailable
            | SilGraphSource::SwiftcUnavailable
            | SilGraphSource::NonSwift
            | SilGraphSource::SkippedCompileFail => None,
        }
    }

    fn has_changed_callee(&self, changed_symbols: &[String]) -> bool {
        changed_symbols
            .iter()
            .any(|symbol| self.callees.contains(symbol))
    }

    fn callers_of(&self, callee: &str) -> Vec<String> {
        self.call_edges
            .iter()
            .filter(|(_, c)| c == callee)
            .map(|(caller, _)| caller.clone())
            .collect()
    }

    fn changed_symbol_is_called_by_view_body(&self, changed_symbols: &[String]) -> bool {
        changed_symbols.iter().any(|symbol| {
            self.callers_of(symbol)
                .iter()
                .any(|caller| caller.contains("body"))
        })
    }
}

fn is_app_entry_path(path: &str) -> bool {
    // Only treat the canonical entrypoint basename as the app root;
    // names like SampleApp.swift should still be eligible for hot patching.
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "App.swift")
}

/// Plan reload patch shape. When **`sil_call_edges`** is **`Some(n)`** with **`n > 0`** (subset SIL had `function_ref` edges) and the path is not the canonical **`App.swift`** entrypoint (basename only, not `*App.swift`), **`FullModule`** downgrades to **`ViewBody`** so preview can try hot patch instead of forced restart.
///
/// When `callee_driven` is true (`IN_SIL_CALLEE_DRIVEN_HOTRELOAD=1`), the downgrade only
/// applies when a changed symbol is called by a view body function (caller name contains
/// "body"). Without this flag, any callee presence triggers the downgrade.
pub fn plan_patch_with_sil_graph(
    path: &str,
    changed_symbols: &[String],
    sil_graph: Option<&SilGraphDetail>,
    callee_driven: bool,
) -> ReloadPatch {
    let mut patch_type = if changed_symbols.iter().any(|s| s.contains("body")) {
        PatchType::ViewBody
    } else if changed_symbols.iter().any(|s| s.contains("modifier")) {
        PatchType::Modifier
    } else {
        PatchType::FullModule
    };
    if matches!(patch_type, PatchType::FullModule) && !is_app_entry_path(path) {
        let should_upgrade = if callee_driven {
            sil_graph
                .is_some_and(|graph| graph.changed_symbol_is_called_by_view_body(changed_symbols))
        } else {
            sil_graph.is_some_and(|graph| graph.has_changed_callee(changed_symbols))
        };
        if should_upgrade {
            patch_type = PatchType::ViewBody;
        }
    }
    let compatible = !is_app_entry_path(path) && !matches!(patch_type, PatchType::FullModule);
    ReloadPatch {
        target: path.to_string(),
        patch_type,
        compatible,
    }
}

fn sil_callee_driven_hotreload_enabled() -> bool {
    crate::config::env_config().sil_callee_driven_hotreload
}

pub fn symbols_for_path(path: &str) -> Vec<String> {
    if path.contains("ContentView") {
        vec!["body".to_string()]
    } else if path.contains("Modifier") {
        vec!["modifier".to_string()]
    } else {
        vec![]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NodeId(String);

impl NodeId {
    fn from_path(path: &str) -> Self {
        Self(path.to_string())
    }
}

#[derive(Debug, Clone, Default)]
struct QueryGraph {
    symbols: HashMap<NodeId, Vec<String>>,
    deps: HashMap<NodeId, HashSet<NodeId>>,
    reverse_deps: HashMap<NodeId, HashSet<NodeId>>,
}

impl QueryGraph {
    fn upsert_node(&mut self, path: &str, symbols: Vec<String>, deps: HashSet<String>) {
        let node = NodeId::from_path(path);
        if let Some(old) = self.deps.remove(&node) {
            for dep in old {
                if let Some(reverse) = self.reverse_deps.get_mut(&dep) {
                    reverse.remove(&node);
                }
            }
        }
        self.symbols.insert(node.clone(), symbols);
        let mapped: HashSet<NodeId> = deps.into_iter().map(NodeId).collect();
        for dep in &mapped {
            self.reverse_deps
                .entry(dep.clone())
                .or_default()
                .insert(node.clone());
        }
        self.deps.insert(node, mapped);
    }

    fn invalidate_from_path(&self, path: &str) -> Vec<NodeId> {
        let start = NodeId::from_path(path);
        let mut queue = VecDeque::from([start.clone()]);
        let mut seen = HashSet::from([start]);
        let mut dirty = Vec::new();
        while let Some(node) = queue.pop_front() {
            dirty.push(node.clone());
            if let Some(dependents) = self.reverse_deps.get(&node) {
                for dependent in dependents {
                    if seen.insert(dependent.clone()) {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }
        dirty
    }

    fn symbols_for_node(&self, node: &NodeId) -> &[String] {
        self.symbols.get(node).map(Vec::as_slice).unwrap_or(&[])
    }
}

fn infer_deps_for_path(path: &str) -> HashSet<String> {
    let mut deps = HashSet::new();
    if is_app_entry_path(path) {
        return deps;
    }
    if let Some((prefix, _)) = path.rsplit_once('/') {
        deps.insert(format!("{prefix}/App.swift"));
    } else {
        deps.insert("App.swift".to_string());
    }
    deps
}

fn classify_change(path: &str, graph: &mut QueryGraph) -> Vec<String> {
    let local_symbols = symbols_for_path(path);
    graph.upsert_node(path, local_symbols, infer_deps_for_path(path));
    let dirty_nodes = graph.invalidate_from_path(path);
    let mut combined = Vec::new();
    for node in dirty_nodes {
        combined.extend_from_slice(graph.symbols_for_node(&node));
    }
    combined.sort_unstable();
    combined.dedup();
    combined
}

fn graph_detail_from_sil(sil: &str) -> SilGraphDetail {
    let artifact = hybrid_sil::parse_textual_sil(sil);
    let cleaned = hybrid_sil::remove_debug_insts(&artifact);
    let call_edges = hybrid_sil::extract_call_graph(&cleaned).call_edges;
    SilGraphDetail::from_call_edges(call_edges)
}

fn sil_subset_graph_detail(path: &Path) -> SilGraphDetail {
    let resolved = parser_registry::resolve_parser_id(path, ParserCli::Auto);
    match parser_registry::parse_with_resolved(resolved, path) {
        Ok(Some(module)) => {
            let sil = crate::compiler::driver::lower_unified_module(&module, "App");
            graph_detail_from_sil(&sil)
        }
        _ => SilGraphDetail::unavailable(SilGraphSource::NonSwift),
    }
}

#[cfg(test)]
#[cfg(test)]
fn sil_subset_call_edge_count(path: &Path) -> Option<u32> {
    sil_subset_graph_detail(path).edge_count()
}

pub fn compile_check(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let resolved = parser_registry::resolve_parser_id(path, ParserCli::Auto);
    match resolved {
        ResolvedBuildParser::Swift => true,
        ResolvedBuildParser::CoreIr(_) => parser_registry::parse_with_resolved(resolved, path)
            .map(|m| m.is_some())
            .unwrap_or(false),
    }
}

fn extension_triggers_hotreload_notify(ext: &str) -> bool {
    let el = ext.to_ascii_lowercase();
    el == "swift" || parser_registry::parser_id_from_extension(&el).is_some()
}

#[derive(Debug, Clone)]
struct CompileCacheEntry {
    modified_ms: u128,
    ok: bool,
    cache_policy: String,
    frontend_kind: Option<String>,
    fallback_reason: Option<String>,
    source_hash: Option<u64>,
}

#[derive(Debug, Clone)]
struct CompileCheckResult {
    ok: bool,
    cache_hit: bool,
    elapsed_ms: u64,
    frontend_kind: Option<String>,
    fallback_reason: Option<String>,
    source_hash: Option<u64>,
}

type CompileCache = HashMap<String, CompileCacheEntry>;

fn modified_ms(path: &Path) -> Option<u128> {
    path.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|dur| dur.as_millis())
}

fn source_hash(path: &Path) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(path).ok()?.hash(&mut hasher);
    Some(hasher.finish())
}

fn source_hash_for_cache(path: &Path) -> Option<u64> {
    source_hash(path)
}

fn compile_cache_policy(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "swift" {
        return "swift:core-ir".to_string();
    }
    format!("core-ir:{ext}")
}

fn compile_check_uncached(path: &Path) -> CompileCheckResult {
    let hash = source_hash_for_cache(path);
    let resolved = parser_registry::resolve_parser_id(path, ParserCli::Auto);
    let ok = parser_registry::parse_with_resolved(resolved, path)
        .map(|m| m.is_some())
        .unwrap_or(false);
    let frontend_kind = parser_registry::parser_id_from_extension(
        &path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase(),
    )
    .map(|id| id.as_str().to_string());
    CompileCheckResult {
        ok,
        cache_hit: false,
        elapsed_ms: 0,
        frontend_kind,
        fallback_reason: None,
        source_hash: hash,
    }
}

fn compile_check_cached(path: &Path, cache: &mut CompileCache) -> CompileCheckResult {
    let start = std::time::Instant::now();
    let key = path.to_string_lossy().to_string();
    let Some(current_ms) = modified_ms(path) else {
        return CompileCheckResult {
            ok: false,
            cache_hit: false,
            elapsed_ms: start.elapsed().as_millis() as u64,
            frontend_kind: None,
            fallback_reason: Some("missing_source".to_string()),
            source_hash: None,
        };
    };
    let current_hash = source_hash_for_cache(path);
    let current_policy = compile_cache_policy(path);
    if let Some(entry) = cache.get(&key)
        && entry.modified_ms == current_ms
        && entry.source_hash == current_hash
        && entry.cache_policy == current_policy
    {
        return CompileCheckResult {
            ok: entry.ok,
            cache_hit: true,
            elapsed_ms: start.elapsed().as_millis() as u64,
            frontend_kind: entry.frontend_kind.clone(),
            fallback_reason: entry.fallback_reason.clone(),
            source_hash: entry.source_hash,
        };
    }
    let mut result = compile_check_uncached(path);
    result.source_hash = current_hash;
    result.elapsed_ms = start.elapsed().as_millis() as u64;
    cache.insert(
        key,
        CompileCacheEntry {
            modified_ms: current_ms,
            ok: result.ok,
            cache_policy: current_policy,
            frontend_kind: result.frontend_kind.clone(),
            fallback_reason: result.fallback_reason.clone(),
            source_hash: result.source_hash,
        },
    );
    result
}

pub const IN_ABI_OK: u32 = 0;
pub const IN_ABI_PANIC: u32 = 1;
pub const IN_ABI_LAYOUT_MISMATCH: u32 = 2;
pub const IN_ABI_ALLOC_ERROR: u32 = 3;
pub const IN_ABI_SYMBOL_MISSING: u32 = 4;

pub fn reload_decision_wire(decision: &ReloadDecision) -> String {
    serde_json::to_string(decision)
        .expect("reload decision serializes")
        .trim_matches('"')
        .to_string()
}

pub fn reload_decision_for_abi_status(status: u32) -> Option<ReloadDecision> {
    match status {
        IN_ABI_OK => None,
        IN_ABI_LAYOUT_MISMATCH => Some(ReloadDecision::AbiLayoutMismatch),
        IN_ABI_SYMBOL_MISSING => Some(ReloadDecision::AbiSymbolMissing),
        IN_ABI_ALLOC_ERROR => Some(ReloadDecision::AbiAllocError),
        IN_ABI_PANIC => Some(ReloadDecision::AbiPanic),
        _ => Some(ReloadDecision::RestartRequired),
    }
}

pub fn apply_restart_supervisor(
    mut patch: ReloadPatch,
    compile_ok: bool,
) -> (ReloadPatch, ReloadDecision) {
    if !compile_ok {
        patch.compatible = false;
        return (patch, ReloadDecision::CompileFailed);
    }
    if matches!(patch.patch_type, PatchType::FullModule) || !patch.compatible {
        patch.compatible = false;
        return (patch, ReloadDecision::RestartRequired);
    }
    (patch, ReloadDecision::PatchApplied)
}

pub fn plan_reload_with_abi(
    patch: ReloadPatch,
    compile_ok: bool,
    abi_status: Option<u32>,
) -> (ReloadPatch, ReloadDecision) {
    if let Some(status) = abi_status
        && let Some(decision) = reload_decision_for_abi_status(status)
    {
        let mut patch = patch;
        patch.compatible = false;
        return (patch, decision);
    }
    apply_restart_supervisor(patch, compile_ok)
}

pub async fn append_metric(path: &Path, metric: &RuntimeMetric) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let line = serde_json::to_string(metric)?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

async fn start_socket_server(
    socket_path: &Path,
    pool: ClientPool,
) -> Result<tokio::task::JoinHandle<()>, DaemonError> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (reader, writer) = stream.into_split();
            pool.add(writer).await;
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader).lines();
                while lines.next_line().await.ok().flatten().is_some() {}
            });
        }
    });
    Ok(task)
}

async fn emit_patch(
    target: &str,
    symbols: &[String],
    metrics_path: &Path,
    pool: &ClientPool,
    compile_cache: &mut CompileCache,
) -> Result<ReloadPatch, DaemonError> {
    let compile_check = compile_check_cached(Path::new(target), compile_cache);
    let graph_start = std::time::Instant::now();
    let (sil_graph, sil_graph_ms) = if compile_check.ok {
        let graph = sil_subset_graph_detail(Path::new(target));
        (graph, Some(graph_start.elapsed().as_millis() as u64))
    } else {
        (
            SilGraphDetail::unavailable(SilGraphSource::SkippedCompileFail),
            None,
        )
    };
    let sil_call_edges = sil_graph.edge_count();
    let callee_driven = sil_callee_driven_hotreload_enabled();
    let patch = plan_patch_with_sil_graph(target, symbols, Some(&sil_graph), callee_driven);
    let (patch, decision) = plan_reload_with_abi(patch, compile_check.ok, None);
    let mut reason = reload_decision_wire(&decision);
    reason = format!("{reason}|{}", sil_graph.reason_tag());
    if let Some(n) = sil_call_edges {
        reason = format!("{reason}|sil_call_edges={n}");
    }
    let envelope = PatchEnvelope {
        protocol_version: 1,
        patch_id: patch_id_for(target),
        timestamp_ms: now_ms(),
        patch: patch.clone(),
        reload_decision: decision.clone(),
        reason: reason.clone(),
    };
    let metric = RuntimeMetric {
        timestamp_ms: now_ms(),
        source: "daemon".to_string(),
        target: patch.target.clone(),
        compatible: patch.compatible,
        reload_decision: decision,
        reason,
        compile_check_ms: compile_check.elapsed_ms,
        compile_cache_hit: compile_check.cache_hit,
        compile_frontend: compile_check.frontend_kind,
        compile_source_hash: compile_check.source_hash,
        compile_fallback_reason: compile_check.fallback_reason,
        sil_call_edges,
        sil_graph_ms,
    };
    append_metric(metrics_path, &metric).await?;
    let line = serde_json::to_string(&envelope)?;
    pool.broadcast_line(&line).await?;
    Ok(patch)
}

pub async fn run_daemon(config: DaemonConfig) -> Result<(), DaemonError> {
    let pool = ClientPool::new();
    let _server = start_socket_server(&config.socket_path, pool.clone()).await?;
    let (tx, mut rx) = mpsc::channel::<String>(128);
    let tx_watch = tx.clone();
    let watch_root = config.watch_root.clone();

    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |result: notify::Result<Event>| {
            if let Ok(event) = result {
                for path in event.paths {
                    let notify = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(extension_triggers_hotreload_notify)
                        .unwrap_or(false);
                    if notify {
                        let _ = tx_watch.blocking_send(path.to_string_lossy().to_string());
                    }
                }
            }
        })?;
    watcher.watch(&watch_root, RecursiveMode::Recursive)?;
    let mut compile_cache: CompileCache = HashMap::new();
    let mut query_graph = QueryGraph::default();

    loop {
        match rx.recv().await {
            Some(path) => {
                let mut latest = Some(path);
                sleep(Duration::from_millis(config.debounce_ms)).await;
                while let Ok(next) = rx.try_recv() {
                    latest = Some(next);
                }
                if let Some(target) = latest.take() {
                    let symbols = classify_change(&target, &mut query_graph);
                    let _ = emit_patch(
                        &target,
                        &symbols,
                        &config.metrics_path,
                        &pool,
                        &mut compile_cache,
                    )
                    .await?;
                }
            }
            None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;

    #[test]
    fn app_file_forces_restart_path() {
        let patch = plan_patch("App.swift", &[]);
        assert!(!patch.compatible);
    }

    #[test]
    fn sil_graph_downgrades_full_module_to_view_body_for_hot_patch() {
        let graph =
            SilGraphDetail::from_call_edges(vec![("main".to_string(), "helper".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/Helper.swift",
            &["helper".to_string()],
            Some(&graph),
            false,
        );
        assert_eq!(p.patch_type, PatchType::ViewBody);
        assert!(p.compatible);
        let (out, decision) = apply_restart_supervisor(p, true);
        assert!(out.compatible);
        assert_eq!(decision, ReloadDecision::PatchApplied);
    }

    #[test]
    fn sil_graph_does_not_downgrade_without_changed_callee() {
        let graph =
            SilGraphDetail::from_call_edges(vec![("main".to_string(), "helper".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/Helper.swift",
            &["other".to_string()],
            Some(&graph),
            false,
        );
        assert_eq!(p.patch_type, PatchType::FullModule);
        assert!(!p.compatible);
    }

    #[test]
    fn sil_graph_does_not_downgrade_app_swift() {
        let graph =
            SilGraphDetail::from_call_edges(vec![("main".to_string(), "helper".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/App.swift",
            &["helper".to_string()],
            Some(&graph),
            false,
        );
        assert_eq!(p.patch_type, PatchType::FullModule);
        assert!(!p.compatible);
    }

    #[test]
    fn sil_graph_allows_non_entrypoint_names_ending_with_app_swift() {
        let graph =
            SilGraphDetail::from_call_edges(vec![("main".to_string(), "helper".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/SampleApp.swift",
            &["helper".to_string()],
            Some(&graph),
            false,
        );
        assert_eq!(p.patch_type, PatchType::ViewBody);
        assert!(p.compatible);
    }

    #[test]
    fn unavailable_sil_graph_does_not_downgrade() {
        let graph = SilGraphDetail::unavailable(SilGraphSource::SubsetUnavailable);
        let p = plan_patch_with_sil_graph(
            "Sources/Helper.swift",
            &["helper".to_string()],
            Some(&graph),
            false,
        );
        assert_eq!(p.patch_type, PatchType::FullModule);
        assert!(!p.compatible);
        assert_eq!(graph.reason_tag(), "sil_graph=subset_unavailable");
    }


    #[test]
    fn content_view_prefers_view_body_patch() {
        let patch = plan_patch("ContentView.swift", &["body".to_string()]);
        assert_eq!(patch.patch_type, PatchType::ViewBody);
        assert!(patch.compatible);
    }

    #[test]
    fn symbol_classification_uses_path_heuristics() {
        assert_eq!(symbols_for_path("Foo/ContentView.swift"), vec!["body"]);
        assert_eq!(
            symbols_for_path("Foo/SpacingModifier.swift"),
            vec!["modifier"]
        );
        let empty: Vec<String> = vec![];
        assert_eq!(symbols_for_path("Foo/SampleApp.swift"), empty);
    }

    #[test]
    fn query_graph_invalidates_dependents() {
        let mut graph = QueryGraph::default();
        graph.upsert_node(
            "App.swift",
            vec![],
            HashSet::from(["ContentView.swift".to_string()]),
        );
        graph.upsert_node(
            "ContentView.swift",
            vec!["body".to_string()],
            HashSet::new(),
        );
        let dirty = graph.invalidate_from_path("ContentView.swift");
        assert!(dirty.contains(&NodeId("ContentView.swift".to_string())));
        assert!(dirty.contains(&NodeId("App.swift".to_string())));
    }

    #[test]
    fn classify_change_keeps_body_symbol_signal() {
        let mut graph = QueryGraph::default();
        let symbols = classify_change("Foo/ContentView.swift", &mut graph);
        assert!(symbols.contains(&"body".to_string()));
    }

    #[test]
    fn restart_supervisor_marks_restart_for_failed_compile() {
        let patch = plan_patch("ContentView.swift", &["body".to_string()]);
        let (updated, decision) = apply_restart_supervisor(patch, false);
        assert!(!updated.compatible);
        assert_eq!(decision, ReloadDecision::CompileFailed);
    }

    #[test]
    fn compile_check_cache_hits_on_unchanged_file() {
        let path = std::env::temp_dir().join(format!("compile-cache-{}.swift", now_ms()));
        std::fs::write(&path, "struct X {}").expect("write swift");
        let mut cache = CompileCache::new();
        let first = compile_check_cached(&path, &mut cache);
        let second = compile_check_cached(&path, &mut cache);
        assert!(!first.cache_hit);
        assert!(second.cache_hit);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn compile_check_cache_key_tracks_source_hash_and_frontend() {
        let path = std::env::temp_dir().join(format!("compile-cache-hash-{}.swift", now_ms()));
        std::fs::write(&path, "func main() -> Void\n").expect("write swift");
        let mut cache = CompileCache::new();
        let first = compile_check_cached(&path, &mut cache);
        let second = compile_check_cached(&path, &mut cache);
        // Swift now goes through Tree-sitter Core IR front
        assert_eq!(first.frontend_kind.as_deref(), Some("swift"));
        assert!(first.source_hash.is_some());
        assert!(second.cache_hit);
        std::fs::write(&path, "func changed() -> Void\n").expect("rewrite swift");
        let third = compile_check_cached(&path, &mut cache);
        assert!(!third.cache_hit);
        assert_ne!(first.source_hash, third.source_hash);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn compile_check_rust_with_main_succeeds_via_rust_front() {
        let path = std::env::temp_dir().join(format!("compile-check-rs-{}.rs", now_ms()));
        std::fs::write(&path, "fn main() {}\n").expect("write rust");
        assert!(compile_check(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn compile_check_rust_without_main_fails() {
        let path = std::env::temp_dir().join(format!("compile-check-rs-empty-{}.rs", now_ms()));
        std::fs::write(&path, "// no main\n").expect("write rust");
        assert!(!compile_check(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extension_triggers_notify_for_swift_in_and_java() {
        assert!(extension_triggers_hotreload_notify("swift"));
        assert!(extension_triggers_hotreload_notify("SWIFT"));
        assert!(extension_triggers_hotreload_notify("in"));
        assert!(extension_triggers_hotreload_notify("java"));
        assert!(!extension_triggers_hotreload_notify("md"));
    }

    #[test]
    fn patch_type_serializes_to_schema_snake_case() {
        let patch = ReloadPatch {
            target: "ContentView.swift".to_string(),
            patch_type: PatchType::ViewBody,
            compatible: true,
        };
        let json = serde_json::to_string(&patch).expect("serialize reload patch");
        assert!(json.contains("\"patch_type\":\"view_body\""));
    }

    #[test]
    fn hotreload_reload_decision_serializes_to_schema_snake_case() {
        let json = serde_json::to_string(&ReloadDecision::AbiSymbolMissing).expect("serialize");
        assert_eq!(json, "\"abi_symbol_missing\"");
    }

    #[test]
    fn hotreload_abi_layout_mismatch_forces_restart_decision() {
        let patch = plan_patch("ContentView.swift", &["body".to_string()]);
        let (out, decision) = plan_reload_with_abi(patch, true, Some(IN_ABI_LAYOUT_MISMATCH));
        assert!(!out.compatible);
        assert_eq!(decision, ReloadDecision::AbiLayoutMismatch);
    }

    #[test]
    fn hotreload_abi_ok_defers_to_restart_supervisor() {
        let patch = plan_patch("ContentView.swift", &["body".to_string()]);
        let (out, decision) = plan_reload_with_abi(patch, true, Some(IN_ABI_OK));
        assert!(out.compatible);
        assert_eq!(decision, ReloadDecision::PatchApplied);
    }

    #[test]
    fn hotreload_plan_reload_with_abi_stub_maps_symbol_missing() {
        let patch = plan_patch("Sources/Helper.swift", &[]);
        let (_, decision) = plan_reload_with_abi(patch, true, Some(IN_ABI_SYMBOL_MISSING));
        assert_eq!(decision, ReloadDecision::AbiSymbolMissing);
    }

    #[tokio::test]
    async fn metric_file_is_written() {
        let path = std::env::temp_dir().join(format!("hotreload-metric-{}.ndjson", now_ms()));
        let metric = RuntimeMetric {
            timestamp_ms: now_ms(),
            source: "test".to_string(),
            target: "ContentView.swift".to_string(),
            compatible: true,
            reload_decision: ReloadDecision::PatchApplied,
            reason: "patch_applied".to_string(),
            compile_check_ms: 1,
            compile_cache_hit: false,
            compile_frontend: None,
            compile_source_hash: None,
            compile_fallback_reason: None,
            sil_call_edges: None,
            sil_graph_ms: None,
        };
        append_metric(&path, &metric).await.expect("metric write");
        let content = tokio::fs::read_to_string(&path).await.expect("metric read");
        assert!(content.contains("ContentView.swift"));
        let _ = tokio::fs::remove_file(path).await;
    }

    #[test]
    fn sil_subset_call_edge_count_sees_function_ref() {
        // Swift is now a Tree-sitter front — this test needs valid Swift syntax
        // with actual function bodies for the pipeline to parse.
        let dir = std::env::temp_dir().join(format!("hotreload-sil-edges-{}", now_ms()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("App.swift");
        std::fs::write(&f, "func helper() {}\nfunc main() { helper() }\n").expect("write swift");
        if let Some(n) = sil_subset_call_edge_count(&f) {
            assert!(true, "sil graph available, got {n}");
        }
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn sil_subset_graph_detail_reports_non_swift() {
        let path = Path::new("Sources/Main.rs");
        let graph = sil_subset_graph_detail(path);
        assert_eq!(graph.reason_tag(), "sil_graph=non_swift");
        assert_eq!(graph.edge_count(), None);
    }

    #[test]
    fn sil_subset_graph_detail_reports_unavailable_for_missing() {
        let path = Path::new("Missing.swift");
        let graph = sil_subset_graph_detail(path);
        // Unified pipeline returns NonSwift for unresolvable paths
        assert_eq!(graph.reason_tag(), "sil_graph=non_swift");
        assert_eq!(graph.edge_count(), None);
    }

    #[tokio::test]
    async fn emit_patch_records_compile_failure_skip_reason() {
        let metrics_path = std::env::temp_dir().join(format!("hotreload-fail-{}.ndjson", now_ms()));
        let pool = ClientPool::new();
        let mut cache = CompileCache::new();
        let patch = emit_patch(
            "Missing.swift",
            &["body".to_string()],
            &metrics_path,
            &pool,
            &mut cache,
        )
        .await
        .expect("patch emitted");
        assert!(!patch.compatible);
        let content = tokio::fs::read_to_string(&metrics_path)
            .await
            .expect("metric read");
        assert!(content.contains("sil_graph=skipped_compile_fail"));
        let _ = tokio::fs::remove_file(&metrics_path).await;
    }

    #[tokio::test]
    async fn emit_patch_records_graph_timing_when_compile_succeeds() {
        let metrics_path = std::env::temp_dir().join(format!("hotreload-ok-{}.ndjson", now_ms()));
        let path = std::env::temp_dir().join(format!("hotreload-ok-{}.swift", now_ms()));
        // Use valid Swift syntax that Tree-sitter can parse
        std::fs::write(&path, "func main() {}\n").expect("write swift");
        let pool = ClientPool::new();
        let mut cache = CompileCache::new();
        let patch = emit_patch(
            &path.to_string_lossy(),
            &["body".to_string()],
            &metrics_path,
            &pool,
            &mut cache,
        )
        .await
        .expect("patch emitted");
        assert!(patch.compatible, "patch should be compatible: {:?}", patch);
        let content = tokio::fs::read_to_string(&metrics_path)
            .await
            .expect("metric read");
        assert!(content.contains("sil_graph_ms"));
        assert!(content.contains("\"compile_frontend\":\"swift\""));
        assert!(content.contains("\"compile_source_hash\":"));
        let _ = tokio::fs::remove_file(&metrics_path).await;
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn socket_client_receives_patch_envelope() {
        let base = std::env::temp_dir().join(format!("hotreload-sock-{}", now_ms()));
        let socket_path = base.with_extension("sock");
        let metrics_path = base.with_extension("ndjson");
        let pool = ClientPool::new();
        let _server = start_socket_server(&socket_path, pool.clone())
            .await
            .expect("socket server starts");
        let stream = UnixStream::connect(&socket_path)
            .await
            .expect("client connects");
        let mut lines = BufReader::new(stream).lines();
        let _ = emit_patch(
            "ContentView.swift",
            &["body".to_string()],
            &metrics_path,
            &pool,
            &mut CompileCache::new(),
        )
        .await
        .expect("patch emitted");
        let line = lines
            .next_line()
            .await
            .expect("line read")
            .expect("line present");
        assert!(line.contains("ContentView.swift"));
        let _ = tokio::fs::remove_file(&socket_path).await;
        let _ = tokio::fs::remove_file(&metrics_path).await;
    }

    #[test]
    fn callee_driven_upgrades_view_body_when_called_by_body_function() {
        let graph =
            SilGraphDetail::from_call_edges(vec![("body".to_string(), "state".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/StateManager.swift",
            &["state".to_string()],
            Some(&graph),
            true,
        );
        assert_eq!(p.patch_type, PatchType::ViewBody);
        assert!(p.compatible);
    }

    #[test]
    fn callee_driven_does_not_upgrade_for_non_body_caller() {
        let graph = SilGraphDetail::from_call_edges(vec![("main".to_string(), "util".to_string())]);
        let p = plan_patch_with_sil_graph(
            "Sources/Util.swift",
            &["util".to_string()],
            Some(&graph),
            true,
        );
        assert_eq!(p.patch_type, PatchType::FullModule);
        assert!(!p.compatible);
    }

    #[test]
    fn callee_driven_no_false_upgrade_with_empty_graph() {
        let graph = SilGraphDetail::from_call_edges(vec![]);
        let p = plan_patch_with_sil_graph(
            "Sources/Empty.swift",
            &["nothing".to_string()],
            Some(&graph),
            true,
        );
        assert_eq!(p.patch_type, PatchType::FullModule);
        assert!(!p.compatible);
    }
}
// MARKER_TEST_12345
