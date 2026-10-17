//! Core IR → AArch64 lowering for the owned native subset.

use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
use crate::inrt;
use crate::native_emit::aarch64::{self, CodeEmitter, REG_FP};
use crate::native_emit::macho::{ExportSymbol, MachOLinkage};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;

/// Stable code for a function the native lowerer could not compile.
pub const DEGRADATION_SKIPPED_FUNCTION: &str = "IN3001";
/// Stable code for a call target the native lowerer could not resolve.
pub const DEGRADATION_UNRESOLVED_CALL: &str = "IN3002";

/// Longest trap message embedded in a stubbed function body. Keeps the emitted
/// blob small and the `movz` length immediate exact.
const MAX_TRAP_MESSAGE: usize = 400;

thread_local! {
    /// Collector for external symbol references during lowering.
    /// `lower_call` pushes (instruction_offset, symbol_name) when a function
    /// is not in the table and native mode is active.
    pub(crate) static TL_EXTERNAL_REFS: RefCell<Vec<(u32, String)>> = const { RefCell::new(Vec::new()) };
    /// Set true before native binary compilation (false for JIT).
    /// Controls whether unknown functions emit external symbol references
    /// or use dlsym.
    pub(crate) static TL_NATIVE_MODE: RefCell<bool> = const { RefCell::new(false) };
    /// Offsets where `adr x27, <error slot>` must be emitted symbolically.
    /// Only populated when the assembly path provides a writable error slot.
    /// Sites are per-function and translated to code offsets once functions are
    /// placed, because each function is lowered into its own buffer.
    pub(crate) static TL_ERROR_SLOT_REFS: RefCell<Vec<(String, u32)>> =
        const { RefCell::new(Vec::new()) };
    /// How throw/try/catch gets its error slot for the module being lowered.
    pub(crate) static TL_ERROR_SLOT_MODE: RefCell<ErrorSlotMode> =
        const { RefCell::new(ErrorSlotMode::RuntimeProvided) };
}

/// Where the throw/try error slot comes from.
///
/// The lowering writes the flag and value through x27, so x27 has to hold a
/// writable address before any throw or try runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorSlotMode {
    /// The JIT runtime sets x27 to its error page before invoking the entry.
    RuntimeProvided,
    /// The generated assembly declares the slot and emits `adr x27, <slot>`.
    AssemblyProvided,
    /// Neither: no writable slot exists, so throw/try cannot be compiled.
    Unsupported,
}

/// Symbol the generated assembly uses for the throw/try error slot.
pub const INRT_ERROR_SLOT_SYMBOL: &str = "_inrt_error_slot";

/// Whether the module being lowered can compile throw/try/catch.
pub(crate) fn error_slot_mode() -> ErrorSlotMode {
    TL_ERROR_SLOT_MODE.with(|mode| *mode.borrow())
}

/// Record a site that needs the error slot address loaded into x27, as a
/// per-function offset. [`lower_module_with_jobs`] turns these into code offsets.
pub(crate) fn note_error_slot_ref(function: &str, site: u32) {
    TL_ERROR_SLOT_REFS.with(|refs| refs.borrow_mut().push((function.to_string(), site)));
}

mod lower_boundary;
mod lower_call;
mod lower_collect;
mod lower_compile;
mod lower_ctx;
mod lower_expr;
mod lower_iterator;
mod lower_stdlib;
mod lower_stmt;
mod lower_util;
pub(crate) use lower_util::{ensure_unit_sequence_layout, field_path};

pub use lower_compile::{
    NativeArtifactOutcome, compile_native_artifact, compile_native_artifact_for_host,
    compile_native_artifact_for_host_with_report, compile_native_artifact_with_report,
    compile_native_executable, compile_native_executable_for_host, host_supports_native_subset,
};

pub(crate) use lower_boundary::boundary_from_module;
pub(crate) use lower_collect::{
    collect_functions, collect_strings, collect_structs, entry_return_kind,
};
pub(crate) use lower_ctx::{
    IteratorMapSlots, LocalSlot, LowerCtx, alloc_declared_locals, append_string_table,
};
pub(crate) use lower_iterator::lower_vec_for;
pub(crate) use lower_stmt::{lower_stmt, lower_struct_expr_into_regs};
pub(crate) use lower_util::{
    base_struct_name, emit_epilogue, emit_failure_return, ensure_return_type, expr_type,
    find_field_offset, is_native_scalar_type, lower_comparison_result, native_param_abi_slots,
    native_struct_fields, pick_scratch, reject_unsupported_function,
};

pub const TARGET_TRIPLE: &str = "aarch64-apple-darwin";

const ENTRY_STUB_SIZE: u32 = 32; // 16 bytes entry stub + 16 bytes global error flag/value

/// Build ARM64 assembly source from lowered code bytes.
/// Each external call site (BL 0 placeholder) is replaced with a proper
/// `bl _symbol` instruction so the assembler+linker resolve it.
fn build_assembly(
    code: &[u8],
    exports: &[(String, u32)],
    external_refs: &[(u32, String)],
    error_slot_refs: &[u32],
    data: &[u8],
    data_relocations: &[(u64, u64)],
    global_relocs: &[(u32, u64)],
    entry: &str,
) -> String {
    // Sort external refs by site for sequential processing
    let mut ext_refs = external_refs.to_vec();
    ext_refs.sort_by_key(|(site, _)| *site);
    let error_slots: std::collections::HashSet<u32> = error_slot_refs.iter().copied().collect();

    let global_sites: HashMap<u32, u64> = global_relocs.iter().copied().collect();

    let entry_name = native_link_name(entry);
    let mut asm = String::new();
    asm.push_str(".text\n");

    // Build a set of function start positions for labels
    let mut fn_starts: std::collections::HashMap<u32, &str> =
        exports.iter().map(|(n, o)| (*o, n.as_str())).collect();
    // Remove entry and any matching the entry name from fn_starts (we emit them via the entry stub)
    fn_starts.remove(&0);
    // Also remove the entry point function (it's the same symbol)
    fn_starts.retain(|_, name| *name != entry_name);

    // Emit .globl for each exported function
    for (name, _) in exports {
        asm.push_str(&format!(".globl {name}\n"));
    }
    // Emit .globl for external symbols
    let mut seen_ext = std::collections::HashSet::new();
    for (_, name) in &ext_refs {
        if seen_ext.insert(name.clone()) {
            asm.push_str(&format!(".globl {name}\n"));
        }
    }

    // Emit entry point label
    asm.push_str(&format!("{entry_name}:\n"));

    let mut ext_idx = 0usize;
    let mut pos = 0usize;
    while pos < code.len() {
        // Check if this position is a function start → emit label
        if let Some(&fn_name) = fn_starts.get(&(pos as u32)) {
            asm.push_str(&format!("{fn_name}:\n"));
        }

        if let Some(offset) = global_sites.get(&(pos as u32 + 8)) {
            let reg = u32::from_le_bytes(code[pos..pos + 4].try_into().unwrap()) & 31;
            // Keep the JIT address sequence's width so code-relative branches stay valid.
            asm.push_str(&format!("\tadrp x{reg}, _inrt_globals+{offset}@PAGE\n\tadd x{reg}, x{reg}, _inrt_globals+{offset}@PAGEOFF\n\tnop\n\tnop\n"));
            pos += 16;
            continue;
        }

        // Check if this position has an external call
        if ext_idx < ext_refs.len() && (pos as u32) == ext_refs[ext_idx].0 {
            let (_, name) = &ext_refs[ext_idx];
            // Emit the external call as assembly instruction
            // First emit any args setup (the code before BL is setup)
            // The BL is 4 bytes, we replace it with assembly
            asm.push_str(&format!("\tbl\t{name}\n"));
            pos += 4;
            ext_idx += 1;
            continue;
        }

        // A throw/try/catch site needs x27 to hold the error slot address, which
        // only the assembler can name, so emit the pair symbolically. Two
        // instructions are reserved for it: `adr` cannot reach another section.
        if error_slots.contains(&(pos as u32)) {
            asm.push_str(&format!(
                "\tadrp\tx27, {INRT_ERROR_SLOT_SYMBOL}@PAGE\n\tadd\tx27, x27, {INRT_ERROR_SLOT_SYMBOL}@PAGEOFF\n"
            ));
            pos += 8;
            continue;
        }

        // Check if this is the entry-point stub area (first ENTRY_STUB_SIZE bytes)
        // The entry stub is generated by inrt — it has a BL to the entry function
        // For now, emit these as raw bytes
        let remaining = code.len() - pos;
        let chunk = if remaining >= 4 { 4 } else { remaining };
        if chunk == 4 {
            let val = u32::from_le_bytes(code[pos..pos + 4].try_into().unwrap());
            asm.push_str(&format!("\t.4byte\t0x{val:08x}\n"));
        } else {
            for i in 0..chunk {
                asm.push_str(&format!("\t.byte\t0x{:02x}\n", code[pos + i]));
            }
        }
        pos += chunk;
    }

    if !error_slot_refs.is_empty() {
        // Flag at +0, value at +8, type at +16, matching what the lowering writes through x27.
        asm.push_str(&format!(
            "\t.data\n\t.p2align 3\n{INRT_ERROR_SLOT_SYMBOL}:\n\t.space 24\n"
        ));
    }

    if !data.is_empty() {
        asm.push_str("\t.data\n\t.p2align 3\n_inrt_globals:\n");
        let fixups: HashMap<u64, u64> = data_relocations.iter().copied().collect();
        let mut offset = 0usize;
        while offset < data.len() {
            if let Some(target) = fixups.get(&(offset as u64)) {
                asm.push_str(&format!("\t.quad _inrt_globals+{target}\n"));
                offset += 8;
            } else {
                asm.push_str(&format!("\t.byte {}\n", data[offset]));
                offset += 1;
            }
        }
    }

    asm
}

/// Map a Rust function name to the linker-visible symbol name.
/// Handles `std::process::exit` → `_exit` (C ABI in libSystem) and
/// raw source names → C ABI symbols with a leading underscore on macOS.
pub fn native_link_name(name: &str) -> String {
    // Clean up spaces (rust_front may produce "std :: process :: exit" instead of "std::process::exit")
    let cleaned = name.replace(' ', "");
    match cleaned.as_str() {
        "std::process::exit" | "std::process::abort" => "_exit".to_string(),
        "std::io::_print" | "print" => "_printf".to_string(),
        _ => {
            // Replace :: with . for valid assembly symbol names (Mach-O accepts dots).
            // Inlang kebab-case (`make-point`) is not asm-safe; map `-` → `_`.
            // Strip angle-bracket generic params: serde_json.from_str.<BenchMetric> → serde_json.from_str
            let no_generics: String = cleaned.chars().filter(|&c| c != '<' && c != '>').collect();
            // Remove trailing dots
            let trimmed = no_generics.trim_end_matches('.');
            let asm_safe = trimmed.replace("::", ".").replace('-', "_");
            if asm_safe.is_empty() {
                return "_unknown".to_string();
            }
            format!("_{asm_safe}")
        }
    }
}

/// Find the macOS SDK root for the system linker.
fn find_sdk_root() -> Option<String> {
    // Try xcrun --show-sdk-path first
    if let Ok(output) = std::process::Command::new("xcrun")
        .arg("--show-sdk-path")
        .output()
    {
        if output.status.success() {
            if let Ok(path) = String::from_utf8(output.stdout) {
                let trimmed = path.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    // Fallback: common SDK paths
    for path in &[
        "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
        "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk",
    ] {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLinkage {
    Executable,
    Dylib,
    StaticLib,
}

impl From<NativeLinkage> for MachOLinkage {
    fn from(linkage: NativeLinkage) -> Self {
        match linkage {
            NativeLinkage::Executable => MachOLinkage::Executable,
            NativeLinkage::Dylib => MachOLinkage::Dylib,
            NativeLinkage::StaticLib => MachOLinkage::StaticLib,
        }
    }
}

#[derive(Debug)]
pub struct LoweredModule {
    pub code: Vec<u8>,
    pub entry_offset: Option<u32>,
    pub exports: Vec<ExportSymbol>,
    /// Function name → byte offset in code (for JIT dispatch)
    pub function_offsets: std::collections::HashMap<String, u32>,
    /// (offset, codegen_base) — absolute address relocations to patch at JIT load time.
    /// Empty for AArch64 (position-independent code).
    pub relocations: Vec<(u32, u64)>,
    /// Writable global data and literal-address sites rebased to it at JIT load.
    pub data: Vec<u8>,
    /// Data-relative pointer slots and their data-relative payload targets.
    pub data_relocations: Vec<(u64, u64)>,
    pub global_relocs: Vec<(u32, u64)>,
    /// External symbol names referenced by the code (for native linking).
    /// Each entry is (instruction_offset, symbol_name).
    pub external_refs: Vec<(u32, String)>,
    /// Code the lowerer could not compile. Each entry corresponds to a trap
    /// body in `code`, so an empty list means nothing was substituted.
    pub degradations: Vec<LoweringDegradation>,
    /// Offsets needing `adr x27, <error slot>` in the generated assembly. Only
    /// populated when [`ErrorSlotMode::AssemblyProvided`] applies.
    pub error_slot_refs: Vec<u32>,
}

/// A construct the native lowerer could not compile.
///
/// The lowerer emits a trap body for every degradation instead of a return-0
/// stub, so an unlowered function can never be mistaken for one that returned a
/// real value. Callers should surface these to the user and to machine-readable
/// reports rather than treating the build as fully compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoweringDegradation {
    /// [`DEGRADATION_SKIPPED_FUNCTION`] or [`DEGRADATION_UNRESOLVED_CALL`].
    pub code: String,
    /// Function the degradation is attributed to: the function that could not
    /// be compiled, or the caller when a call target could not be resolved.
    pub function: String,
    /// Unresolved callee name, for [`DEGRADATION_UNRESOLVED_CALL`].
    pub target: Option<String>,
    /// Underlying lowerer error.
    pub reason: String,
    /// Whether the entry can reach this trap by following calls out of compiled
    /// code. Unreachable traps are dead code in the artifact; reachable ones
    /// abort the program when that path runs.
    #[serde(default)]
    pub reachable: bool,
}

impl LoweringDegradation {
    fn skipped_function(function: &str, reason: &str) -> Self {
        Self {
            code: DEGRADATION_SKIPPED_FUNCTION.to_string(),
            function: function.to_string(),
            target: None,
            reason: reason.to_string(),
            reachable: false,
        }
    }

    fn unresolved_call(caller: &str, target: &str) -> Self {
        Self {
            code: DEGRADATION_UNRESOLVED_CALL.to_string(),
            function: caller.to_string(),
            target: Some(target.to_string()),
            reason: format!("call to `{target}` was not resolved to a function"),
            reachable: false,
        }
    }

    /// Name the trap body is registered under in the function table.
    fn trap_site(&self) -> &str {
        self.target.as_deref().unwrap_or(&self.function)
    }

    /// One-line rendering used for CLI output.
    pub fn describe(&self) -> String {
        match &self.target {
            Some(target) => format!(
                "{}: `{}` calls `{}`, which was not resolved",
                self.code, self.function, target
            ),
            None => format!(
                "{}: `{}` was not compiled to native code ({})",
                self.code, self.function, self.reason
            ),
        }
    }

    /// Message embedded in the trap body, so a trapped program explains itself.
    fn trap_message(&self) -> String {
        format!("{}\n", self.describe())
    }
}

/// Names the entry can reach by following calls out of compiled functions.
///
/// A trap ends the program, so only lowered functions can pass execution along;
/// calls out of skipped functions never matter. When there is no entry point
/// (libraries), every trap is treated as reachable because an external caller
/// may invoke any exported function.
fn reachable_trap_sites(
    buffers: &[FunctionBuffer],
    entry: &str,
    executable: bool,
    degradations: &[LoweringDegradation],
) -> std::collections::HashSet<String> {
    if !executable {
        return degradations
            .iter()
            .map(|degradation| degradation.trap_site().to_string())
            .collect();
    }
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for buf in buffers {
        edges
            .entry(buf.name.as_str())
            .or_default()
            .extend(buf.pending_calls.iter().map(|call| call.target.as_str()));
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut queue: Vec<&str> = vec![entry];
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        if let Some(next) = edges.get(name) {
            for target in next {
                if !seen.contains(target) {
                    queue.push(target);
                }
            }
        }
    }
    seen.into_iter().map(str::to_string).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryReturn {
    IntLike,
    VoidOrReference,
}

#[derive(Clone, Copy)]
pub(crate) enum FunctionReferenceKind {
    Call,
    Address { rd: u8 },
}

pub(crate) struct PendingCall {
    pub(crate) site: u32,
    pub(crate) target: String,
    pub(crate) kind: FunctionReferenceKind,
}

fn function_reference_insn(
    site: u32,
    target: u32,
    kind: FunctionReferenceKind,
) -> Result<u32, String> {
    let delta = i64::from(target) - i64::from(site);
    match kind {
        FunctionReferenceKind::Address { rd } => {
            if !(-(1 << 20)..(1 << 20)).contains(&delta) {
                return Err("native-lower: function address exceeds AArch64 ADR range".into());
            }
            Ok(aarch64::adr(rd, delta as i32))
        }
        FunctionReferenceKind::Call => {
            if delta % 4 != 0 || !(-(1 << 27)..(1 << 27)).contains(&delta) {
                return Err("native-lower: function call exceeds AArch64 BL range".into());
            }
            Ok(aarch64::bl(delta as i32))
        }
    }
}

pub(crate) struct PendingInrtCall {
    pub(crate) site: u32,
    pub(crate) target: String,
}

pub(crate) struct PendingString {
    pub(crate) adr_site: u32,
    pub(crate) string_index: i64,
    pub(crate) rd: u8,
}

pub(crate) struct FunctionBuffer {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) pending_calls: Vec<PendingCall>,
    pub(crate) pending_inrt_calls: Vec<PendingInrtCall>,
    pub(crate) pending_strings: Vec<PendingString>,
    pub(crate) pending_globals: Vec<(u32, u64)>,
    pub(crate) external_refs: Vec<(u32, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub(crate) name: String,
    pub(crate) params: Vec<(String, Typ)>,
    pub(crate) ret: Typ,
    pub(crate) body: Vec<Stmt>,
    pub(crate) may_throw: bool,
}

fn lower_function_to_buffer(
    func: &FunctionInfo,
    functions: &HashMap<String, FunctionInfo>,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    strings: &HashMap<String, i64>,
    globals: &HashMap<String, u64>,
    global_types: &HashMap<String, Typ>,
) -> Result<FunctionBuffer, String> {
    let mut emitter = CodeEmitter::new();
    let mut pending_calls = Vec::new();
    let mut pending_inrt_calls = Vec::new();
    let mut pending_strings = Vec::new();
    let mut pending_globals = Vec::new();
    let lowered = lower_function(
        &mut emitter,
        func,
        functions,
        structs,
        strings,
        &mut pending_calls,
        &mut pending_inrt_calls,
        &mut pending_strings,
        globals,
        global_types,
        &mut pending_globals,
        0,
    );
    let external_refs = TL_EXTERNAL_REFS.with(|refs| std::mem::take(&mut *refs.borrow_mut()));
    lowered?;
    Ok(FunctionBuffer {
        name: func.name.clone(),
        bytes: emitter.bytes,
        pending_calls,
        pending_inrt_calls,
        pending_strings,
        pending_globals,
        external_refs,
    })
}

/// Clamp a trap message to [`MAX_TRAP_MESSAGE`] bytes, keeping it line-terminated.
fn truncate_trap_message(message: &str) -> String {
    if message.len() <= MAX_TRAP_MESSAGE {
        return message.to_string();
    }
    let mut end = MAX_TRAP_MESSAGE - 1;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n", &message[..end])
}

/// Emit a trap body standing in for code the lowerer could not compile.
///
/// The body writes the degradation message to stderr and exits with
/// [`inrt::INRT_TRAP_EXIT_CODE`]. It never returns, so it needs no prologue,
/// epilogue, or return value. Returns the offset of the `bl` site so the
/// runtime-blob patch pass can point it at the trap routine.
fn emit_degradation_trap(emitter: &mut CodeEmitter, degradation: &LoweringDegradation) -> u32 {
    let message = truncate_trap_message(&degradation.trap_message());

    let mut body = Vec::with_capacity(24 + message.len());
    body.extend_from_slice(&aarch64::adr(0, 0).to_le_bytes());
    body.extend_from_slice(&aarch64::movz64(1, message.len() as u16, 0).to_le_bytes());
    body.extend_from_slice(&aarch64::bl(0).to_le_bytes());
    // The message is data inside the code stream: keep it 8-byte aligned so the
    // next function body still starts on an instruction boundary.
    while !body.len().is_multiple_of(8) {
        body.push(0);
    }
    let data_offset = body.len() as i32;
    body.extend_from_slice(message.as_bytes());
    while !body.len().is_multiple_of(8) {
        body.push(0);
    }
    body[..4].copy_from_slice(&aarch64::adr(0, data_offset).to_le_bytes());

    let base = emitter.len();
    emitter.bytes.extend_from_slice(&body);
    base + 8
}

pub(crate) fn lower_native_module(
    module: &UnifiedModule,
    entry: &str,
    linkage: NativeLinkage,
) -> Result<LoweredModule, String> {
    let previous_mode = TL_NATIVE_MODE.with(|mode| mode.replace(true));
    let lowered = lower_module(module, entry, linkage);
    TL_NATIVE_MODE.with(|mode| *mode.borrow_mut() = previous_mode);
    lowered
}

pub fn lower_module(
    module: &UnifiedModule,
    entry: &str,
    linkage: NativeLinkage,
) -> Result<LoweredModule, String> {
    lower_module_with_jobs(module, entry, linkage, 1)
}

pub fn lower_module_with_jobs(
    module: &UnifiedModule,
    entry: &str,
    linkage: NativeLinkage,
    jobs: usize,
) -> Result<LoweredModule, String> {
    crate::core_ir_verifier::check_execution_contracts(module)?;
    let prepared = crate::lower_core::prepared_native_module(module);
    let module = prepared.as_ref();
    crate::core_ir_verifier::check_execution_contracts(module)?;
    // Clear any stale external refs from previous invocations
    TL_EXTERNAL_REFS.with(|refs| refs.borrow_mut().clear());
    TL_ERROR_SLOT_REFS.with(|refs| refs.borrow_mut().clear());
    // throw/try/catch writes through x27, so x27 must hold a writable error slot
    // before any of them runs. The JIT runtime provides one; a native executable
    // gets one from the generated assembly, which only the as+ld path emits.
    let slot_mode = if !TL_NATIVE_MODE.with(|m| *m.borrow()) {
        ErrorSlotMode::RuntimeProvided
    } else if linkage == NativeLinkage::Executable && cfg!(target_os = "macos") {
        ErrorSlotMode::AssemblyProvided
    } else {
        ErrorSlotMode::Unsupported
    };
    TL_ERROR_SLOT_MODE.with(|mode| *mode.borrow_mut() = slot_mode);
    let functions = collect_functions(module)?;
    let structs = collect_structs(module);
    let strings = collect_strings(module);
    let globals = super::global_data::collect_globals(module, &structs)?;
    let super::global_data::DataSection {
        bytes: data,
        pointer_fixups: data_relocations,
    } = super::global_data::build_data_section(
        module,
        &globals,
        super::global_data::StringLayout::Instring,
    )?;
    let global_types: HashMap<String, Typ> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    // Try exact match first, then namespaced variants
    let resolved_entry = if functions.contains_key(entry) {
        entry.to_string()
    } else {
        // Try any function ending with .<entry> (namespaced)
        functions
            .keys()
            .find(|k| k.ends_with(&format!(".{entry}")))
            .cloned()
            .or_else(|| functions.keys().find(|k| k.as_str() == entry).cloned())
            .unwrap_or_else(|| {
                // Entry not found — pick first non-closure function as fallback.
                // This lets library crates without main lower all functions.
                functions
                    .keys()
                    .find(|k| !k.starts_with("__closure_"))
                    .cloned()
                    .unwrap_or_else(|| entry.to_string())
            })
    };
    let entry_return = functions
        .get(&resolved_entry)
        .map(|func| entry_return_kind(&func.ret))
        .unwrap_or(EntryReturn::VoidOrReference);

    let mut emitter = CodeEmitter::new();
    emitter.bytes.resize(ENTRY_STUB_SIZE as usize, 0);
    let mut function_offsets = HashMap::new();
    let mut external_refs = Vec::new();
    let mut pending_inrt_calls = Vec::new();
    let mut pending_strings = Vec::new();
    let mut names: Vec<String> = functions.keys().cloned().collect();
    let mut called = std::collections::HashSet::new();
    for function in functions.values() {
        for stmt in &function.body {
            crate::core_opt::collect_calls_in_stmt(stmt, &mut called);
        }
    }
    names.retain(|name| {
        !name.starts_with("__closure_") || called.contains(name) || name == &resolved_entry
    });
    names.sort();

    // Lower each function. Failures are skipped; callers get return-0 stubs.
    // ponytail: graceful degradation for self-hosting — skip stdlib functions
    // that use unsupported types rather than failing the entire module.
    let jobs = jobs.max(1);
    let (buffers, skipped): (Vec<FunctionBuffer>, Vec<(String, String)>) =
        if jobs == 1 || names.len() <= 1 {
            let mut bufs = Vec::with_capacity(names.len());
            let mut skip = Vec::new();
            for name in &names {
                let func = &functions[name];
                match lower_function_to_buffer(
                    func,
                    &functions,
                    &structs,
                    &strings,
                    &globals,
                    &global_types,
                ) {
                    Ok(buf) => bufs.push(buf),
                    Err(e) => {
                        skip.push((name.clone(), e));
                    }
                }
            }
            (bufs, skip)
        } else {
            std::thread::scope(|s| {
                let functions = &functions;
                let structs = &structs;
                let strings = &strings;
                let globals = &globals;
                let global_types = &global_types;
                let chunks: Vec<Vec<String>> = names
                    .chunks(names.len().div_ceil(jobs))
                    .map(|c| c.to_vec())
                    .collect();
                let mut handles = Vec::with_capacity(chunks.len());
                for chunk in chunks {
                    handles.push(s.spawn(move || {
                        let mut bufs = Vec::new();
                        let mut skips = Vec::new();
                        for name in chunk {
                            let func = &functions[&name];
                            match lower_function_to_buffer(
                                func,
                                functions,
                                structs,
                                strings,
                                globals,
                                global_types,
                            ) {
                                Ok(buf) => bufs.push((name, buf)),
                                Err(e) => {
                                    skips.push((name, e));
                                }
                            }
                        }
                        (bufs, skips)
                    }));
                }
                let mut results: Vec<(String, FunctionBuffer)> = Vec::with_capacity(names.len());
                let mut skip = Vec::new();
                for h in handles {
                    let (bufs, skips) = h
                        .join()
                        .map_err(|e| format!("native-lower: worker thread panicked: {e:?}"))?;
                    results.extend(bufs);
                    skip.extend(skips);
                }
                results.sort_by(|(a, _), (b, _)| a.cmp(b));
                let buffers: Vec<FunctionBuffer> = results.into_iter().map(|(_, b)| b).collect();
                Ok::<_, String>((buffers, skip))
            })?
        };

    // A body already refused by the lowerer is replaced by the existing explicit
    // degradation trap. Check completion only for actual retained definitions,
    // using collector identities (including any disambiguated names).
    crate::core_ir_verifier::check_nonvoid_completion_for(
        module,
        buffers.iter().map(|buffer| {
            let function = &functions[&buffer.name];
            (
                function.name.as_str(),
                function.params.as_slice(),
                &function.ret,
                function.body.as_slice(),
            )
        }),
    )?;

    // Every skipped function and unresolved call target becomes a trap body
    // instead of a return-0 stub, and is recorded so callers can report it.
    // ponytail: graceful degradation for self-hosting — skip stdlib functions
    // that use unsupported types rather than failing the entire module.
    let mut degradations: Vec<LoweringDegradation> = Vec::new();
    let mut trap_calls: Vec<PendingInrtCall> = Vec::new();

    // If the entry function itself was skipped, fail the entire module.
    if let Some((_, err)) = skipped.iter().find(|(name, _)| name == &resolved_entry) {
        return Err(format!(
            "native-lower: entry function `{resolved_entry}` unsupported: {err}"
        ));
    }
    for (name, reason) in &skipped {
        let degradation = LoweringDegradation::skipped_function(name, reason);
        let offset = emitter.len();
        function_offsets.insert(name.clone(), offset);
        let bl_site = emit_degradation_trap(&mut emitter, &degradation);
        trap_calls.push(PendingInrtCall {
            site: bl_site,
            target: inrt::INRT_TRAP_BUILTIN.to_string(),
        });
        degradations.push(degradation);
    }
    // Append all per-function buffers and record global offsets.
    for buf in &buffers {
        let offset = emitter.len();
        function_offsets.insert(buf.name.clone(), offset);
        emitter.bytes.extend_from_slice(&buf.bytes);
    }

    // Translate per-function error-slot sites into code offsets. Refs from
    // functions that failed to lower are dropped: their code was never placed,
    // so there is nothing for the assembler to patch.
    let lowered_names: std::collections::HashSet<&str> =
        buffers.iter().map(|buf| buf.name.as_str()).collect();
    let error_slot_refs: Vec<u32> = TL_ERROR_SLOT_REFS
        .with(|refs| std::mem::take(&mut *refs.borrow_mut()))
        .into_iter()
        .filter(|(name, _)| lowered_names.contains(name.as_str()))
        .filter_map(|(name, local_site)| function_offsets.get(&name).map(|base| base + local_site))
        .collect();

    // Patch internal function calls (BL) using global offsets.
    // ponytail: calls to skipped/unresolved targets trap at runtime rather than
    // failing the entire module; the trap names the caller and the target.
    for buf in &buffers {
        let base = *function_offsets
            .get(&buf.name)
            .ok_or_else(|| format!("native-lower: missing function offset for `{}`", buf.name))?;
        for call in &buf.pending_calls {
            let target_offset = function_offsets.get(&call.target).copied();
            let target_offset = match target_offset {
                Some(o) => o,
                None => {
                    // Emit a trap body for this unresolved target.
                    let stub_offset = emitter.len();
                    function_offsets.insert(call.target.clone(), stub_offset);
                    let degradation = LoweringDegradation::unresolved_call(&buf.name, &call.target);
                    let bl_site = emit_degradation_trap(&mut emitter, &degradation);
                    trap_calls.push(PendingInrtCall {
                        site: bl_site,
                        target: inrt::INRT_TRAP_BUILTIN.to_string(),
                    });
                    degradations.push(degradation);
                    stub_offset
                }
            };
            let global_site = base + call.site;
            emitter.patch_u32(
                global_site,
                function_reference_insn(global_site, target_offset, call.kind)?,
            );
        }
    }

    // Mark which traps the entry can actually reach. Done after the call-patch
    // pass because unresolved call targets only become degradations there.
    let reachable = reachable_trap_sites(
        &buffers,
        &resolved_entry,
        linkage == NativeLinkage::Executable,
        &degradations,
    );
    for degradation in &mut degradations {
        degradation.reachable = reachable.contains(degradation.trap_site());
    }

    // Collect pending static arrays, strings, and inrt calls from all buffers,
    // translating local sites to global offsets.
    let mut global_relocs = Vec::new();
    for buf in buffers {
        let base = *function_offsets
            .get(&buf.name)
            .ok_or_else(|| format!("native-lower: missing function offset for `{}`", buf.name))?;
        external_refs.extend(
            buf.external_refs
                .into_iter()
                .map(|(site, name)| (base + site, name)),
        );
        global_relocs.extend(
            buf.pending_globals
                .into_iter()
                .map(|(site, offset)| (base + site, offset)),
        );
        pending_strings.extend(buf.pending_strings.into_iter().map(|p| PendingString {
            adr_site: base + p.adr_site,
            string_index: p.string_index,
            rd: p.rd,
        }));
        pending_inrt_calls.extend(buf.pending_inrt_calls.into_iter().map(|c| PendingInrtCall {
            site: base + c.site,
            target: c.target,
        }));
    }

    append_string_table(&mut emitter, &strings, pending_strings)?;

    // Trap bodies emitted for degraded code call into the runtime blob, so they
    // join the same patch pass as ordinary inrt calls.
    pending_inrt_calls.extend(trap_calls);

    if !pending_inrt_calls.is_empty() {
        let (runtime_blob, runtime_offsets) = inrt::build_runtime_blob();
        let runtime_base = emitter.len();
        emitter.bytes.extend_from_slice(&runtime_blob);
        for call in &pending_inrt_calls {
            let fn_offset = *runtime_offsets.get(call.target.as_str()).ok_or_else(|| {
                format!(
                    "native-lower: unresolved inrt call target `{}`",
                    call.target
                )
            })?;
            let target_abs = runtime_base + fn_offset;
            let offset = target_abs as i32 - call.site as i32;
            emitter.patch_u32(call.site, aarch64::bl(offset));
        }
    }

    let entry_offset = if linkage == NativeLinkage::Executable {
        let entry_fn_offset = *function_offsets
            .get(&resolved_entry)
            .ok_or_else(|| format!("native-lower: missing entry function `{resolved_entry}`"))?;
        let stub = match entry_return {
            EntryReturn::IntLike => inrt::build_entry_stub(entry_fn_offset),
            EntryReturn::VoidOrReference => {
                inrt::build_entry_stub_with_forced_exit(entry_fn_offset, 0)
            }
        };
        emitter.bytes[..stub.len()].copy_from_slice(&stub);
        Some(0)
    } else {
        None
    };

    let mut export_names: Vec<String> = function_offsets.keys().cloned().collect();
    export_names.sort();
    let exports: Vec<ExportSymbol> = export_names
        .into_iter()
        .map(|name| {
            let offset = *function_offsets
                .get(&name)
                .ok_or_else(|| format!("native-lower: missing export offset for `{}`", name))?;
            Ok(ExportSymbol {
                name: name.clone(),
                offset,
            })
        })
        .collect::<Result<_, String>>()?;

    Ok(LoweredModule {
        code: emitter.bytes,
        entry_offset,
        exports,
        function_offsets,
        relocations: Vec::new(), // Code-relative references use PC-relative instructions.
        data,
        data_relocations,
        global_relocs,
        external_refs,
        degradations,
        error_slot_refs,
    })
}

fn lower_function(
    emitter: &mut CodeEmitter,
    func: &FunctionInfo,
    functions: &HashMap<String, FunctionInfo>,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    strings: &HashMap<String, i64>,
    pending_calls: &mut Vec<PendingCall>,
    pending_inrt_calls: &mut Vec<PendingInrtCall>,
    pending_strings: &mut Vec<PendingString>,
    globals: &HashMap<String, u64>,
    global_types: &HashMap<String, Typ>,
    pending_globals: &mut Vec<(u32, u64)>,
    _fn_start_offset: u32,
) -> Result<(), String> {
    ensure_return_type(&func.ret, &func.name, structs)?;
    reject_unsupported_function(func, structs)?;

    emitter.emit_u32(0xA9BF_7BFD);
    emitter.emit_u32(aarch64::mov_reg64(REG_FP, aarch64::REG_SP));
    // X27 set by JIT caller to error page address (throw/try/catch).

    let mut ctx = LowerCtx::new(
        &func.params,
        structs,
        strings,
        globals,
        global_types,
        pending_inrt_calls,
        pending_strings,
        &func.name,
    )?;
    ctx.ret_typ = func.ret.clone();
    alloc_declared_locals(&mut ctx, &func.body, &func.name, functions)?;
    for name in crate::core_effects::error_bindings(&func.body) {
        if ctx.locals.contains_key(&name) {
            let offset = ctx.alloc_slot();
            ctx.error_types.insert(name, offset);
        }
    }
    ctx.binop_temp = ctx.alloc_slot();
    for index in 0..ctx.binop_temps.len() {
        ctx.binop_temps[index] = ctx.alloc_slot();
    }
    for _ in 0..lower_ctx::max_try_depth(&func.body) {
        let slot = ctx.alloc_slot();
        ctx.try_flag_slots.push(slot);
    }
    for index in 0..ctx.call_arg_temps.len() {
        ctx.call_arg_temps[index] = ctx.alloc_slot();
    }
    let staging = native_staging_plan(&func.body, &ctx, functions, &func.name)?;
    let scalar_array_words = staging
        .scalar_array_depth
        .checked_mul(19)
        .ok_or("native-lower: scalar array spill size overflow")?;
    let scalar_array_bytes = scalar_array_words
        .checked_mul(8)
        .ok_or("native-lower: scalar array spill size overflow")?;
    if scalar_array_bytes > 4095 {
        return Err(format!(
            "native-lower: stack frame exceeds the AArch64 immediate-offset subset in `{}`",
            func.name
        ));
    }
    for _ in 0..scalar_array_words {
        let offset = ctx.alloc_slot();
        ctx.scalar_array_spills.push(offset);
    }
    let aggregate_vector_words = (staging.vector != 0).then_some(staging.vector);
    if has_struct_return_vec_literal(&func.body)
        || aggregate_vector_words.is_some()
        || has_iter_once(&func.body)
        || has_iter_map(&func.body)
    {
        ctx.vec_literal_header_offset = Some(ctx.alloc_slot());
        ctx.alloc_slot();
        ctx.alloc_slot();
    }
    if has_iter_chain(&func.body) {
        ctx.iterator_chain_header_offset = Some(ctx.alloc_slot());
        ctx.alloc_slot();
        ctx.alloc_slot();
    }
    if has_iter_map(&func.body) {
        ctx.iterator_map_slots = Some(IteratorMapSlots {
            ptr: ctx.alloc_slot(),
            len: ctx.alloc_slot(),
            index: ctx.alloc_slot(),
            binding: ctx.alloc_slot(),
        });
    }
    if let Some(words) = aggregate_vector_words {
        let offset = ctx.alloc_slot();
        for _ in 1..words {
            ctx.alloc_slot();
        }
        ctx.aggregate_vector_scratch = Some((offset, words));
    }
    let record_words = staging.record;
    if record_words != 0 {
        let bytes = record_words
            .checked_mul(8)
            .ok_or("native-lower: record array staging size overflow")?;
        if bytes > 4095 {
            return Err("native-lower: record array staging exceeds AArch64 frame subset".into());
        }
        let offset = ctx.alloc_slot();
        for _ in 1..record_words {
            ctx.alloc_slot();
        }
        ctx.record_array_scratch = Some((offset, record_words));
    }
    if ctx.stack_reserve() > 4095 {
        return Err(format!(
            "native-lower: stack frame exceeds the AArch64 immediate-offset subset in `{}`",
            func.name
        ));
    }
    if ctx.stack_size > 0 {
        emitter.emit_u32(aarch64::sub_imm64(
            aarch64::REG_SP,
            aarch64::REG_SP,
            ctx.stack_reserve() as u16,
        ));
    }
    ctx.prologue_stack_reserve = ctx.stack_reserve();
    for (reg, offset) in &ctx.param_stores {
        emitter.emit_u32(aarch64::str64(*reg, aarch64::REG_SP, *offset));
    }
    // Load incoming stack words before dereferencing any receiver pointer.
    let stack_base = ctx.stack_reserve() + 16;
    for (arg_idx, local_off) in &ctx.stack_params {
        let incoming_off = stack_base + 8 * arg_idx;
        emitter.emit_u32(aarch64::ldr64(15, aarch64::REG_SP, incoming_off));
        emitter.emit_u32(aarch64::str64(15, aarch64::REG_SP, *local_off));
    }
    for (name, typ) in &func.params {
        if name == "self"
            && let Typ::Named(struct_name) = typ
            && let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(name).cloned()
            && let Some(&pointer_slot) = ctx.receiver_pointers.get(name)
        {
            emitter.emit_u32(aarch64::ldr64(15, aarch64::REG_SP, pointer_slot));
            emitter.emit_insns(&aarch64::load_i64(14, 1));
            emitter.emit_u32(aarch64::cmp_reg64(15, 14));
            let valid = emitter.emit_insn(aarch64::b_cond(8, 0));
            emit_failure_return(emitter, &mut ctx, &func.name)?;
            emitter.patch_u32(
                valid,
                aarch64::b_cond(8, emitter.len() as i32 - valid as i32),
            );
            if structs
                .get(base_struct_name(struct_name))
                .is_some_and(|fields| fields.is_empty())
            {
                continue;
            }
            let mut offsets: Vec<_> = fields.values().copied().collect();
            offsets.sort_unstable();
            // Receiver layout uses contiguous local storage, including empty address slots.
            for (index, offset) in offsets.into_iter().enumerate() {
                emitter.emit_u32(aarch64::ldr64(14, 15, (index * 8) as u32));
                emitter.emit_u32(aarch64::str64(14, aarch64::REG_SP, offset));
            }
        }
    }
    // Handle implicit return: last stmt is an Expr with multi-field struct init
    let last_is_struct_init = func
        .body
        .last()
        .map_or(false, |s| matches!(s, Stmt::Expr(Expr::StructInit { .. })));
    if last_is_struct_init {
        let last = func.body.last().unwrap();
        let (init_name, init_fields) = match last {
            Stmt::Expr(Expr::StructInit { name, fields }) => (name.clone(), fields.clone()),
            _ => return Err("native-lower: expected StructInit as last statement".to_string()),
        };
        for stmt in &func.body[..func.body.len() - 1] {
            lower_stmt(
                emitter,
                &mut ctx,
                stmt,
                functions,
                pending_calls,
                &func.name,
                &func.ret,
            )?;
        }
        let ret_name = match &func.ret {
            Typ::Named(n) => n.clone(),
            _ => init_name.clone(),
        };
        lower_struct_expr_into_regs(
            emitter,
            &mut ctx,
            &Expr::StructInit {
                name: init_name,
                fields: init_fields,
            },
            &ret_name,
            functions,
            pending_calls,
            &func.name,
        )?;
    } else {
        for stmt in &func.body {
            lower_stmt(
                emitter,
                &mut ctx,
                stmt,
                functions,
                pending_calls,
                &func.name,
                &func.ret,
            )?;
        }
    }
    ctx.assert_vec_for_slots_consumed(&func.name)?;

    // A return in one branch does not terminate every fallthrough path.
    if func.ret.canonical() == Typ::Void || !ctx.emitted_return {
        if func.ret.canonical() == Typ::Void {
            emitter.emit_insns(&aarch64::load_i64(0, 0));
        }
        emit_epilogue(emitter, ctx.prologue_stack_reserve);
    }

    pending_globals.append(&mut ctx.pending_globals);
    Ok(())
}

fn has_iter_once(body: &[Stmt]) -> bool {
    fn expr_has_once(expr: &Expr) -> bool {
        match expr {
            Expr::MatchValue { scrutinee, arms } => {
                expr_has_once(scrutinee)
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(expr_has_once) || expr_has_once(&arm.value)
                    })
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => [cond, then_expr, else_expr]
                .into_iter()
                .any(|value| expr_has_once(value)),
            Expr::Block { body, result } => {
                has_iter_once(body) || result.as_deref().is_some_and(expr_has_once)
            }

            Expr::Call { callee, args, .. } => {
                matches!(callee.as_ref(), Expr::Ident(name) if name == "std::iter::once")
                    || expr_has_once(callee)
                    || args.iter().any(expr_has_once)
            }
            Expr::Binary { lhs, rhs, .. } => expr_has_once(lhs) || expr_has_once(rhs),
            Expr::Unary { expr, .. } | Expr::Field { base: expr, .. } => expr_has_once(expr),
            Expr::Index { base, index } => expr_has_once(base) || expr_has_once(index),
            Expr::StructInit { fields, .. } => fields.iter().any(|(_, value)| expr_has_once(value)),
            Expr::ArrayLit(items) => items.iter().any(expr_has_once),
            Expr::UnitLit
            | Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::Ident(_)
            | Expr::Closure { .. } => false,
        }
    }
    body.iter().any(|stmt| match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => expr_has_once(expr),
        Stmt::IndexAssign {
            base, index, value, ..
        } => expr_has_once(base) || expr_has_once(index) || expr_has_once(value),
        Stmt::FieldAssign { base, value, .. } => expr_has_once(base) || expr_has_once(value),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => expr_has_once(cond) || has_iter_once(then_body) || has_iter_once(else_body),
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref().is_some_and(expr_has_once) || has_iter_once(body)
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => expr_has_once(scrutinee) || arms.iter().any(|arm| has_iter_once(&arm.body)),
        Stmt::Try { body, catches, .. } => {
            has_iter_once(body) || catches.iter().any(|catch| has_iter_once(&catch.body))
        }
        Stmt::Declare(_, _)
        | Stmt::Return(None)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Propagate => false,
    })
}

fn has_iter_map(body: &[Stmt]) -> bool {
    fn expr_has_map(expr: &Expr) -> bool {
        match expr {
            Expr::MatchValue { scrutinee, arms } => {
                expr_has_map(scrutinee)
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(expr_has_map) || expr_has_map(&arm.value)
                    })
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => [cond, then_expr, else_expr]
                .into_iter()
                .any(|value| expr_has_map(value)),
            Expr::Block { body, result } => {
                has_iter_map(body) || result.as_deref().is_some_and(expr_has_map)
            }

            Expr::Call { callee, args, .. } => {
                matches!(callee.as_ref(), Expr::Ident(name) if name == "map")
                    || expr_has_map(callee)
                    || args.iter().any(expr_has_map)
            }
            Expr::Binary { lhs, rhs, .. } => expr_has_map(lhs) || expr_has_map(rhs),
            Expr::Unary { expr, .. } | Expr::Field { base: expr, .. } => expr_has_map(expr),
            Expr::Index { base, index } => expr_has_map(base) || expr_has_map(index),
            Expr::StructInit { fields, .. } => fields.iter().any(|(_, value)| expr_has_map(value)),
            Expr::ArrayLit(items) => items.iter().any(expr_has_map),
            Expr::Closure { body, .. } => has_iter_map(body),
            Expr::UnitLit
            | Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::Ident(_) => false,
        }
    }
    body.iter().any(|stmt| match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => expr_has_map(expr),
        Stmt::IndexAssign {
            base, index, value, ..
        } => expr_has_map(base) || expr_has_map(index) || expr_has_map(value),
        Stmt::FieldAssign { base, value, .. } => expr_has_map(base) || expr_has_map(value),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => expr_has_map(cond) || has_iter_map(then_body) || has_iter_map(else_body),
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref().is_some_and(expr_has_map) || has_iter_map(body)
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => expr_has_map(scrutinee) || arms.iter().any(|arm| has_iter_map(&arm.body)),
        Stmt::Try { body, catches, .. } => {
            has_iter_map(body) || catches.iter().any(|catch| has_iter_map(&catch.body))
        }
        Stmt::Declare(_, _)
        | Stmt::Return(None)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Propagate => false,
    })
}

fn has_iter_chain(body: &[Stmt]) -> bool {
    fn expr_has_chain(expr: &Expr) -> bool {
        match expr {
            Expr::MatchValue { scrutinee, arms } => {
                expr_has_chain(scrutinee)
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(expr_has_chain) || expr_has_chain(&arm.value)
                    })
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => [cond, then_expr, else_expr]
                .into_iter()
                .any(|value| expr_has_chain(value)),
            Expr::Block { body, result } => {
                has_iter_chain(body) || result.as_deref().is_some_and(expr_has_chain)
            }

            Expr::Call { callee, args, .. } => {
                matches!(callee.as_ref(), Expr::Ident(name) if name == "chain")
                    || expr_has_chain(callee)
                    || args.iter().any(expr_has_chain)
            }
            Expr::Binary { lhs, rhs, .. } => expr_has_chain(lhs) || expr_has_chain(rhs),
            Expr::Unary { expr, .. } | Expr::Field { base: expr, .. } => expr_has_chain(expr),
            Expr::Index { base, index } => expr_has_chain(base) || expr_has_chain(index),
            Expr::StructInit { fields, .. } => {
                fields.iter().any(|(_, value)| expr_has_chain(value))
            }
            Expr::ArrayLit(items) => items.iter().any(expr_has_chain),
            Expr::UnitLit
            | Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::Ident(_)
            | Expr::Closure { .. } => false,
        }
    }
    body.iter().any(|stmt| match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => expr_has_chain(expr),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => expr_has_chain(cond) || has_iter_chain(then_body) || has_iter_chain(else_body),
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref().is_some_and(expr_has_chain) || has_iter_chain(body)
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => expr_has_chain(scrutinee) || arms.iter().any(|arm| has_iter_chain(&arm.body)),
        Stmt::Try { body, catches, .. } => {
            has_iter_chain(body) || catches.iter().any(|catch| has_iter_chain(&catch.body))
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => expr_has_chain(base) || expr_has_chain(index) || expr_has_chain(value),
        Stmt::FieldAssign { base, value, .. } => expr_has_chain(base) || expr_has_chain(value),
        Stmt::Declare(_, _)
        | Stmt::Return(None)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Propagate => false,
    })
}

fn has_struct_return_vec_literal(body: &[Stmt]) -> bool {
    fn contains_literal(expr: &Expr) -> bool {
        match expr {
            Expr::StructInit { fields, .. } => fields
                .iter()
                .any(|(_, value)| matches!(value, Expr::ArrayLit(_))),
            Expr::Block { body, result } => {
                has_struct_return_vec_literal(body)
                    || result.as_deref().is_some_and(contains_literal)
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => contains_literal(then_expr) || contains_literal(else_expr),
            _ => false,
        }
    }
    body.iter().any(|stmt| match stmt {
        Stmt::Return(Some(value)) => contains_literal(value),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => has_struct_return_vec_literal(then_body) || has_struct_return_vec_literal(else_body),
        Stmt::Loop { body, .. } | Stmt::Try { body, .. } => has_struct_return_vec_literal(body),
        Stmt::Match { arms, .. } => arms
            .iter()
            .any(|arm| has_struct_return_vec_literal(&arm.body)),
        _ => false,
    })
}

fn max_nested_vector_element_words(
    typ: &Typ,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    fn_name: &str,
    visited: &mut Vec<String>,
    depth: u32,
) -> Result<usize, String> {
    if depth > 40 {
        return Ok(0);
    }
    match typ {
        Typ::Vector(element) => {
            let mut max = lower_util::native_param_abi_slots(
                &[("value".to_string(), (**element).clone())],
                structs,
                fn_name,
            )?;
            max = max.max(max_nested_vector_element_words(
                element,
                structs,
                fn_name,
                visited,
                depth + 1,
            )?);
            Ok(max)
        }
        Typ::Named(name) => {
            let base = base_struct_name(name);
            if visited.iter().any(|v| v == base) {
                return Ok(0);
            }
            visited.push(base.to_string());
            let fields = lower_util::native_struct_fields(structs, name, fn_name)?;
            let mut max = 0;
            for (_, field_typ) in fields {
                max = max.max(max_nested_vector_element_words(
                    &field_typ,
                    structs,
                    fn_name,
                    visited,
                    depth + 1,
                )?);
            }
            visited.pop();
            Ok(max)
        }
        Typ::Array(element) => {
            max_nested_vector_element_words(element, structs, fn_name, visited, depth + 1)
        }
        _ => Ok(0),
    }
}

/// Peak simultaneously live typed record staging words. Sequential items and
/// branches reuse storage; a record being built retains its fields while a
/// nested record operation runs. Each called function plans its own frame.
#[derive(Debug, Default, PartialEq, Eq)]
struct NativeStagingPlan {
    record: usize,
    vector: usize,
    scalar_array_depth: usize,
}

fn native_staging_plan(
    body: &[Stmt],
    ctx: &LowerCtx<'_>,
    functions: &HashMap<String, FunctionInfo>,
    fn_name: &str,
) -> Result<NativeStagingPlan, String> {
    struct Planner<'a, 'b> {
        ctx: &'a LowerCtx<'b>,
        functions: &'a HashMap<String, FunctionInfo>,
        vector: std::cell::Cell<usize>,
        array_depth: std::cell::Cell<usize>,
        array_peak: std::cell::Cell<usize>,
        fn_name: &'a str,
    }
    impl Planner<'_, '_> {
        fn record_words(&self, typ: &Typ) -> Result<usize, String> {
            Ok(lower_util::record_array_fields(typ, self.ctx.structs)?
                .map_or(0, |fields| fields.len()))
        }
        fn value(&self, expr: &Expr, expected: Option<&Typ>) -> Result<usize, String> {
            // Scalar literals preserve surrounding registers only in scalar contexts.
            // Allocating copies preserve them for typed destinations as well.
            let scalar_literal = matches!(expr, Expr::ArrayLit(items) if !items.is_empty())
                && !matches!(
                    expected.map(Typ::canonical),
                    Some(Typ::Array(_) | Typ::Vector(_))
                );
            let depth = self.array_depth.get();
            let allocating_copy = matches!(expr,Expr::Unary{op,..} if crate::core_ir::array_copy::count(op).is_some());
            if scalar_literal || allocating_copy {
                let next = depth
                    .checked_add(1)
                    .ok_or("native-lower: scalar array spill depth overflow")?;
                self.array_depth.set(next);
                self.array_peak.set(self.array_peak.get().max(next));
            }
            let result = self.value_inner(expr, expected);
            self.array_depth.set(depth);
            result
        }
        fn value_inner(&self, expr: &Expr, expected: Option<&Typ>) -> Result<usize, String> {
            match expr {
                Expr::Unary { op, expr: source }
                    if crate::core_ir::array_copy::count(op).is_some() =>
                {
                    let typ = lower_expr::native_expr_type(source, self.ctx, self.functions);
                    self.value(source, typ.as_ref())
                }
                Expr::ArrayLit(items) => {
                    // An explicit Array destination does not use Vec staging. For
                    // ambiguous literals retain the previous conservative Vec plan.
                    let destination = expected.map(Typ::canonical);
                    if !matches!(destination, Some(Typ::Array(_))) {
                        let nominal = match &destination {
                            Some(Typ::Vector(element)) => match element.canonical() {
                                Typ::Named(name) => Some(name),
                                _ => None,
                            },
                            _ => items.first().and_then(|item| match item {
                                Expr::StructInit { name, .. } => Some(name.clone()),
                                _ => None,
                            }),
                        };
                        if let Some(name) = nominal {
                            let words = lower_util::native_param_abi_slots(
                                &[("value".to_string(), Typ::Named(name.clone()))],
                                self.ctx.structs,
                                self.fn_name,
                            )?;
                            let nested = max_nested_vector_element_words(
                                &Typ::Named(name),
                                self.ctx.structs,
                                self.fn_name,
                                &mut Vec::new(),
                                0,
                            )?;
                            self.vector.set(self.vector.get().max(words).max(nested));
                        }
                    }
                    let typ = expected
                        .cloned()
                        .or_else(|| lower_expr::native_expr_type(expr, self.ctx, self.functions));
                    let (element, staged) = match typ.map(|typ| typ.canonical()) {
                        Some(Typ::Array(element)) => {
                            let words = if expected.is_some() {
                                self.record_words(&element)?
                            } else {
                                self.record_words(&element).unwrap_or(0)
                            };
                            (Some(element), words)
                        }
                        Some(Typ::Vector(element)) => (Some(element), 0),
                        _ => (None, 0),
                    };
                    let nested = items.iter().try_fold(0usize, |peak, item| {
                        self.value(item, element.as_deref())
                            .map(|words| peak.max(words))
                    })?;
                    staged
                        .checked_add(nested)
                        .ok_or_else(|| "native-lower: record array staging size overflow".into())
                }
                Expr::Block { body, result } => Ok(self.body(body)?.max(
                    result
                        .as_ref()
                        .map(|value| self.value(value, expected))
                        .transpose()?
                        .unwrap_or(0),
                )),
                Expr::Conditional {
                    cond,
                    then_expr,
                    else_expr,
                } => Ok(self
                    .value(cond, None)?
                    .max(self.value(then_expr, expected)?)
                    .max(self.value(else_expr, expected)?)),
                Expr::MatchValue { scrutinee, arms } => {
                    let mut peak = self.value(scrutinee, None)?;
                    for arm in arms {
                        if let Some(guard) = &arm.guard {
                            peak = peak.max(self.value(guard, None)?);
                        }
                        peak = peak.max(self.value(&arm.value, expected)?);
                    }
                    Ok(peak)
                }
                Expr::StructInit { name, fields } => {
                    let schema = self.ctx.structs.get(name);
                    let mut peak = 0;
                    for (field, value) in fields {
                        let typ = schema
                            .and_then(|schema| schema.iter().find(|(name, _)| name == field))
                            .map(|(_, typ)| typ);
                        peak = peak.max(self.value(value, typ)?);
                    }
                    Ok(peak)
                }
                Expr::Call { callee, args } => {
                    let mut peak = self.value(callee, None)?;
                    let function = if let Expr::Ident(name) = callee.as_ref() {
                        self.functions.get(name)
                    } else {
                        None
                    };
                    for (index, arg) in args.iter().enumerate() {
                        peak = peak.max(
                            self.value(
                                arg,
                                function
                                    .and_then(|function| function.params.get(index))
                                    .map(|(_, typ)| typ),
                            )?,
                        );
                    }
                    Ok(peak)
                }
                // A closure owns another function frame after shared preparation.
                Expr::Closure { body, .. } => {
                    for stmt in body {
                        if let Stmt::Return(Some(Expr::StructInit { name, .. })) = stmt {
                            let words = lower_util::native_param_abi_slots(
                                &[("value".to_string(), Typ::Named(name.clone()))],
                                self.ctx.structs,
                                self.fn_name,
                            )?;
                            let nested = max_nested_vector_element_words(
                                &Typ::Named(name.clone()),
                                self.ctx.structs,
                                self.fn_name,
                                &mut Vec::new(),
                                0,
                            )?;
                            self.vector.set(self.vector.get().max(words).max(nested));
                        }
                    }
                    let depth = self.array_depth.replace(0);
                    let peak = self.array_peak.get();
                    let result = self.body(body);
                    self.array_depth.set(depth);
                    self.array_peak.set(peak);
                    result?;
                    Ok(0)
                }
                _ => {
                    let mut peak = Ok(0usize);
                    crate::core_ir::for_each_expr_child(expr, &mut |child| {
                        peak = peak.as_ref().map_err(Clone::clone).and_then(|current| {
                            self.value(child, None).map(|words| (*current).max(words))
                        });
                    });
                    peak
                }
            }
        }
        fn body(&self, body: &[Stmt]) -> Result<usize, String> {
            let mut peak = 0;
            for stmt in body {
                let words = match stmt {
                    Stmt::Let(name, typ, value) => {
                        let inferred = lower_expr::native_expr_type(
                            &Expr::Ident(name.clone()),
                            self.ctx,
                            self.functions,
                        );
                        self.value(value, typ.as_ref().or(inferred.as_ref()))?
                    }
                    Stmt::Assign(name, value) => {
                        let typ = lower_expr::native_expr_type(
                            &Expr::Ident(name.clone()),
                            self.ctx,
                            self.functions,
                        );
                        self.value(value, typ.as_ref())?
                    }
                    Stmt::Return(Some(value)) => self.value(value, Some(&self.ctx.ret_typ))?,
                    Stmt::Expr(value) | Stmt::Throw(value) => self.value(value, None)?,
                    Stmt::IndexAssign { base, index, value } => {
                        let element =
                            match lower_expr::native_expr_type(base, self.ctx, self.functions)
                                .map(|typ| typ.canonical())
                            {
                                Some(Typ::Array(element)) => Some(element),
                                _ => None,
                            };
                        let stage = element
                            .as_ref()
                            .map(|typ| self.record_words(typ))
                            .transpose()?
                            .unwrap_or(0);
                        let replacement = stage
                            .checked_add(self.value(value, element.as_deref())?)
                            .ok_or("native-lower: record array staging size overflow")?;
                        self.value(base, None)?
                            .max(self.value(index, None)?)
                            .max(replacement)
                    }
                    Stmt::FieldAssign { base, name, value } => {
                        let typ = lower_expr::native_expr_type(
                            &Expr::Field {
                                base: Box::new(base.clone()),
                                name: name.clone(),
                            },
                            self.ctx,
                            self.functions,
                        );
                        self.value(base, None)?
                            .max(self.value(value, typ.as_ref())?)
                    }
                    Stmt::If {
                        cond,
                        then_body,
                        else_body,
                    } => self
                        .value(cond, None)?
                        .max(self.body(then_body)?)
                        .max(self.body(else_body)?),
                    Stmt::Loop { cond, body, .. } => cond
                        .as_ref()
                        .map(|cond| self.value(cond, None))
                        .transpose()?
                        .unwrap_or(0)
                        .max(self.body(body)?),
                    Stmt::Match { scrutinee, arms } => {
                        let mut words = self.value(scrutinee, None)?;
                        for arm in arms {
                            if let Some(guard) = &arm.guard {
                                words = words.max(self.value(guard, None)?);
                            }
                            words = words.max(self.body(&arm.body)?);
                        }
                        words
                    }
                    Stmt::Try { body, catches, .. } => {
                        let mut words = self.body(body)?;
                        for catch in catches {
                            words = words.max(self.body(&catch.body)?);
                        }
                        words
                    }
                    Stmt::Declare(_, _)
                    | Stmt::Return(None)
                    | Stmt::Break
                    | Stmt::Continue
                    | Stmt::Propagate => 0,
                };
                peak = peak.max(words);
            }
            Ok(peak)
        }
    }
    let planner = Planner {
        ctx,
        functions,
        vector: std::cell::Cell::new(0),
        array_depth: std::cell::Cell::new(0),
        array_peak: std::cell::Cell::new(0),
        fn_name,
    };
    let record = planner.body(body)?;
    Ok(NativeStagingPlan {
        record,
        vector: planner.vector.get(),
        scalar_array_depth: planner.array_peak.get(),
    })
}

#[cfg(test)]
mod lower_tests;

#[cfg(test)]
mod global_data_assembly_tests {
    #[test]
    fn emits_data_pointer_slots_once_without_code_global_references() {
        let asm = super::build_assembly(&[], &[], &[], &[], &[99; 16], &[(0, 8)], &[], "main");
        assert!(asm.contains("_inrt_globals:"), "{asm}");
        assert_eq!(asm.matches(".quad _inrt_globals+8").count(), 1, "{asm}");
        assert_eq!(asm.matches(".byte 99").count(), 8, "{asm}");
        let raw = super::build_assembly(&[], &[], &[], &[], &[42], &[], &[], "main");
        assert!(raw.contains("_inrt_globals:\n\t.byte 42"), "{raw}");
    }
}

#[cfg(test)]
mod record_array_plan_tests {
    use super::*;

    fn row(x: Expr, y: Expr) -> Expr {
        Expr::StructInit {
            name: "Row".into(),
            fields: vec![("x".into(), x), ("y".into(), y)],
        }
    }
    fn plan(body: &[Stmt], params: &[(String, Typ)]) -> NativeStagingPlan {
        let structs = HashMap::from([(
            "Row".into(),
            vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
        )]);
        let string_offsets = HashMap::new();
        let globals = HashMap::new();
        let global_types = HashMap::new();
        let mut runtime = Vec::new();
        let mut strings = Vec::new();
        let mut ctx = LowerCtx::new(
            params,
            &structs,
            &string_offsets,
            &globals,
            &global_types,
            &mut runtime,
            &mut strings,
            "test",
        )
        .unwrap();
        let functions = HashMap::new();
        alloc_declared_locals(&mut ctx, body, "test", &functions).unwrap();
        native_staging_plan(body, &ctx, &functions, "test").unwrap()
    }
    fn array_typ() -> Typ {
        Typ::Array(Box::new(Typ::Named("Row".into())))
    }
    fn replace(value: Expr) -> Stmt {
        Stmt::IndexAssign {
            base: Expr::Ident("rows".into()),
            index: Expr::IntLit(0),
            value,
        }
    }

    #[test]
    fn allocating_array_copies_reserve_nested_preservation_even_for_typed_destinations() {
        let typ = Typ::Array(Box::new(Typ::Int));
        let copy = |value| Expr::Unary {
            op: crate::core_ir::array_copy::operation(1),
            expr: Box::new(value),
        };
        let nested = copy(copy(Expr::Ident("source".into())));
        let body = [
            Stmt::Let("result".into(), Some(typ.clone()), nested),
            Stmt::Expr(copy(Expr::Ident("source".into()))),
        ];
        let planned = plan(&body, &[("source".into(), typ)]);
        assert_eq!(
            planned.scalar_array_depth, 2,
            "nested allocations need independent register preservation; sequential copies reuse it"
        );
        assert_eq!(planned.record, 0);
        assert_eq!(planned.vector, 0);
    }

    #[test]
    fn fixed_record_literal_does_not_reserve_unrelated_vector_stage() {
        let body = [Stmt::Let(
            "rows".into(),
            Some(array_typ()),
            Expr::ArrayLit(vec![row(Expr::IntLit(20), Expr::IntLit(22))]),
        )];
        assert_eq!(
            plan(&body, &[]),
            NativeStagingPlan {
                record: 2,
                vector: 0,
                scalar_array_depth: 0,
            }
        );
    }

    #[test]
    fn nested_replacement_retains_outer_stage_but_siblings_reuse_it() {
        let inner = replace(row(Expr::IntLit(1), Expr::IntLit(2)));
        let outer = replace(row(
            Expr::IntLit(20),
            Expr::Block {
                body: vec![inner],
                result: Some(Box::new(Expr::IntLit(22))),
            },
        ));
        assert_eq!(plan(&[outer], &[("rows".into(), array_typ())]).record, 4);
        let sibling = replace(row(Expr::IntLit(20), Expr::IntLit(22)));
        assert_eq!(
            plan(&[sibling.clone(), sibling], &[("rows".into(), array_typ())]).record,
            2
        );
    }

    #[test]
    fn array_of_record_identifiers_uses_schema_sized_stage() {
        let body = [Stmt::Let(
            "rows".into(),
            Some(array_typ()),
            Expr::ArrayLit(vec![Expr::Ident("value".into())]),
        )];
        assert_eq!(
            plan(&body, &[("value".into(), Typ::Named("Row".into()))]),
            NativeStagingPlan {
                record: 2,
                vector: 0,
                scalar_array_depth: 0,
            }
        );
    }

    #[test]
    fn contextual_vector_still_plans_existing_aggregate_materializer() {
        let structs = HashMap::from([(
            "Row".into(),
            vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
        )]);
        let strings = HashMap::new();
        let globals = HashMap::new();
        let global_types = HashMap::new();
        let mut runtime = Vec::new();
        let mut pending_strings = Vec::new();
        let ctx = LowerCtx::new(
            &[],
            &structs,
            &strings,
            &globals,
            &global_types,
            &mut runtime,
            &mut pending_strings,
            "test",
        )
        .unwrap();
        let body = [Stmt::Let(
            "values".into(),
            Some(Typ::Vector(Box::new(Typ::Named("Row".into())))),
            Expr::ArrayLit(vec![row(Expr::IntLit(20), Expr::IntLit(22))]),
        )];
        assert_eq!(
            native_staging_plan(&body, &ctx, &HashMap::new(), "test").unwrap(),
            NativeStagingPlan {
                record: 0,
                vector: 2,
                scalar_array_depth: 0,
            }
        );
    }
}

#[cfg(test)]
mod scalar_array_frame_plan_tests {
    use super::*;

    #[test]
    fn plans_deep_scalar_arrays_independently_of_call_and_record_pools() {
        let mut value = "true".to_string();
        for _ in 0..8 {
            value = format!("![{value}]");
        }
        let source = format!(
            "struct Row{{Int x;Bool flag}}fn consume(a:Int,b:Int,c:Int,d:Int,e:Int,f:Int,g:Int,row:Row)->Int{{if row.flag{{return 7;}}return a+b+c+d+e+f+g+row.x;}}fn main()->Int{{return consume(1,2,3,4,5,6,7,Row{{x:14,flag:{value}}});}}"
        );
        let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
        lower_module(&module, "main", NativeLinkage::Executable)
            .expect("depth8 fits the existing frame subset");
        // A typed destination does not reserve the new scalar preservation area.
        let typed = crate::in_lang_parse::parse_in_source(
            "fn main()->Int{let values:[Int]=[20,22];return values[0]+values[1];}",
        )
        .unwrap();
        lower_module(&typed, "main", NativeLinkage::Executable).unwrap();
    }

    #[test]
    fn excessive_scalar_array_preservation_reports_existing_frame_limit() {
        let mut value = "true".to_string();
        for _ in 0..64 {
            value = format!("![{value}]");
        }
        let module =
            crate::in_lang_parse::parse_in_source(&format!("fn main()->Bool{{return {value};}}"))
                .unwrap();
        let error =
            lower_module(&module, "main", NativeLinkage::Executable).expect_err("frame limit");
        assert!(
            error.contains("stack frame exceeds the AArch64 immediate-offset subset"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod record_array_lowering_tests {
    use super::*;

    #[test]
    fn lowers_owned_record_array_value_and_place_flows() {
        for source in [
            "struct Row{Int x;Int y}\nfn main()->Int{let rows:[Row]=[Row{x:20,y:22}];return rows[0].x+rows[0].y;}",
            "struct Row{Int x;Int y}\nfn make()->[Row]{let value=Row{x:20,y:22};return [value,value];}\nfn sum(rows:[Row])->Int{let value=rows[0];rows[1]=value;rows[0].x=20;return rows[1].x+rows[1].y;}\nfn main()->Int{return sum(make());}",
            "struct Row{Int x;Int y}\nfn main()->Int{let rows:[Row]=[Row{x:1,y:2}];rows[0]=Row{x:20,y:{rows[0]=Row{x:3,y:4};22}};return rows[0].x+rows[0].y;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).expect("parse record array");
            lower_module(&module, "main", NativeLinkage::Executable).expect(source);
        }
    }

    #[test]
    fn retains_unsupported_record_array_schema_refusals() {
        for source in [
            "struct Empty{}\nfn main()->Int{let values:[Empty]=[Empty{}];return 42;}",
            "struct Inner{Int x}\nstruct Outer{Inner nested}\nfn main()->Int{let values:[Outer]=[Outer{nested:Inner{x:42}}];return 42;}",
            "struct Row{[Int] values}\nfn main()->Int{let values:[Row]=[Row{values:[42]}];return 42;}",
        ] {
            let module =
                crate::in_lang_parse::parse_in_source(source).expect("parse unsupported schema");
            assert!(
                lower_module(&module, "main", NativeLinkage::Executable).is_err(),
                "{source}"
            );
        }
    }
}

#[cfg(test)]
mod uninitialized_declaration_boundaries {
    use super::*;
    #[test]
    fn direct_backends_refuse_a_read_without_an_assignment() {
        let mut module =
            crate::in_lang_parse::parse_in_source("fn main()->Int{let value:Int=7;return value;}")
                .unwrap();
        let Decl::Function { body, .. } = &mut module.decls[0] else {
            panic!("function")
        };
        body[0] = Stmt::Declare("value".into(), Typ::Int);
        assert!(
            lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .starts_with("definite-initialization:")
        );
        assert!(
            crate::native_emit::x86_64_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .starts_with("definite-initialization:")
        );
        assert!(
            crate::native_emit::thumb_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .starts_with("definite-initialization:")
        );
        assert!(
            crate::compiler::mir_lower::lower_to_mir(&module)
                .err()
                .unwrap()
                .starts_with("definite-initialization:")
        );
        assert!(
            crate::native_emit::c_backend::emit_c_module(&module)
                .err()
                .unwrap()
                .starts_with("definite-initialization:")
        );
        for lang in [
            crate::native_emit::source_emit::SourceLang::Rust,
            crate::native_emit::source_emit::SourceLang::Go,
        ] {
            assert!(
                crate::native_emit::source_emit::emit_source_module(&module, lang)
                    .err()
                    .unwrap()
                    .starts_with("definite-initialization:")
            );
        }
        let Decl::Function { body, .. } = &mut module.decls[0] else {
            panic!("function")
        };
        body.insert(1, Stmt::Assign("value".into(), Expr::IntLit(42)));
        assert!(
            lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .is_ok()
        );
        assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_ok());
        assert!(crate::native_emit::thumb_lower::lower_module(&module, "main").is_ok());
        assert!(crate::compiler::mir_lower::lower_to_mir(&module).is_ok());
        assert!(crate::native_emit::c_backend::emit_c_module(&module).is_ok());
    }
}
