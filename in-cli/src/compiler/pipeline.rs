//! Inauguration Compiler Pipeline — multi-stage compilation driver.
//!
//! The [`Compiler`] struct drives the full pipeline:
//!
//! ```text
//! Source (.in)
//!   │  Stage 1: Frontend (in_lang_parse → UnifiedModule → IrModule)
//!   ▼
//! Type Check
//!   │  Stage 2: typecheck
//!   ▼
//! Lower
//!   │  Stage 3: lower_core (UnifiedModule → analysis IrModule)
//!   ▼
//! Optimize
//!   │  Stage 4: PassManager (SimplifyCFG, ConstantFold, DCE, SROA, Cleanup)
//!   ▼
//! Codegen
//!   │  Stage 5: Core IR → owned native emission → artifact bytes
//!   ▼
//! Output + Metadata JSON sidecar
//! ```

use std::fmt;
use std::time::Instant;

use super::backend::{BackendKind, BackendOutput, select_backend};
use super::core::{IrBasicBlock, IrFunction, IrInstruction, IrModule, IrOpcode, IrType};
use super::metadata::{ComponentMetadata, ComponentSpec, OptimizationLevel};
use super::passes::PassManager;
use crate::core_ir::UnifiedModule;
use crate::typecheck::TypeChecker;

/// Error from the compilation pipeline.
#[derive(Debug, Clone)]
pub enum CompileError {
    Frontend(String),
    TypeCheck(String),
    Lowering(String),
    Pass(String),
    Backend(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Frontend(msg) => write!(f, "frontend error: {msg}"),
            CompileError::TypeCheck(msg) => write!(f, "type check error: {msg}"),
            CompileError::Lowering(msg) => write!(f, "lowering error: {msg}"),
            CompileError::Pass(msg) => write!(f, "pass error: {msg}"),
            CompileError::Backend(msg) => write!(f, "backend error: {msg}"),
        }
    }
}

/// Name of each pipeline stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Frontend,
    TypeCheck,
    Lower,
    Optimize,
    Codegen,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Frontend => "frontend",
            Stage::TypeCheck => "typecheck",
            Stage::Lower => "lower",
            Stage::Optimize => "optimize",
            Stage::Codegen => "codegen",
        }
    }
}

/// Timing for a single stage.
#[derive(Debug, Clone)]
pub struct StageTime {
    pub stage: Stage,
    pub elapsed_us: u64,
}

impl Default for StageTime {
    fn default() -> Self {
        Self {
            stage: Stage::Frontend,
            elapsed_us: 0,
        }
    }
}

/// Full compilation timing report.
#[derive(Debug, Clone, Default)]
pub struct CompileTimings {
    pub stages: Vec<StageTime>,
    pub total_us: u64,
}

/// Result of a full compilation.
#[derive(Debug)]
pub struct CompileResult {
    pub module: IrModule,
    pub output: BackendOutput,
    pub metadata: ComponentMetadata,
    pub timings: CompileTimings,
}

// ─── The Compiler ────────────────────────────────────────────────────────

/// The Inauguration compiler — drives the multi-stage pipeline.
pub struct Compiler {
    config: super::metadata::ComponentSpec,
    pass_manager: PassManager,
    backend: BackendKind,
    timings: CompileTimings,
    last_unified: Option<UnifiedModule>,
}

impl Compiler {
    /// Create a new compiler from a component spec.
    pub fn new(spec: ComponentSpec) -> Result<Self, CompileError> {
        // Build pass pipeline based on optimization level
        let pass_manager = match spec.optimization_level {
            OptimizationLevel::None => PassManager::new(),
            OptimizationLevel::Less => PassManager::with_standard_passes(),
            OptimizationLevel::Default => PassManager::with_standard_passes(),
            OptimizationLevel::Aggressive => PassManager::with_aggressive_passes(),
        };

        // Validate backend
        let _kind = select_backend(&spec).map_err(|e| CompileError::Backend(format!("{e}")))?;
        let backend = _kind;

        Ok(Self {
            config: spec,
            pass_manager,
            backend,
            timings: CompileTimings::default(),
            last_unified: None,
        })
    }

    // ─── Stage 1: Frontend ─────────────────────────────────────────────

    /// Parse source into an analysis module while retaining Core IR for codegen.
    pub fn parse_source(&mut self, source: &str) -> Result<IrModule, CompileError> {
        let start = Instant::now();
        self.last_unified = None;
        let mut unified = crate::in_lang_parse::parse_in_library_source(source)
            .map_err(CompileError::Frontend)?;
        crate::lower_core::desugar_module_with_semantics(
            &mut unified,
            crate::target::integer_semantics(Some(&self.config.target)),
        );
        let mut module = lower_unified_to_ir(&unified)?;
        self.last_unified = Some(unified);

        module.component = Some(self.config.clone());

        self.timings.stages.push(StageTime {
            stage: Stage::Frontend,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(module)
    }

    // ─── Stage 2: Type Check ───────────────────────────────────────────

    pub fn type_check(&mut self, _module: &mut IrModule) -> Result<(), CompileError> {
        let start = Instant::now();
        if let Some(unified) = &self.last_unified {
            let checker = TypeChecker::new();
            if let Err(errors) = checker.check_module(unified) {
                for e in &errors {
                    eprintln!("  type error: {e:?}");
                }
                self.timings.stages.push(StageTime {
                    stage: Stage::TypeCheck,
                    elapsed_us: start.elapsed().as_micros() as u64,
                });
                return Err(CompileError::TypeCheck(format!(
                    "{} type error(s) found",
                    errors.len()
                )));
            }
        }
        self.timings.stages.push(StageTime {
            stage: Stage::TypeCheck,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(())
    }

    // ─── Stage 3: Lower ────────────────────────────────────────────────

    pub fn lower(&mut self, _module: &mut IrModule) -> Result<(), CompileError> {
        let start = Instant::now();
        // Additional lowering transforms are not currently specified.
        // The main lowering from AST to IR happens in `lower_unified_to_ir`.
        self.timings.stages.push(StageTime {
            stage: Stage::Lower,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(())
    }

    // ─── Stage 4: Optimize ─────────────────────────────────────────────

    pub fn optimize(&mut self, module: &mut IrModule) -> Result<(), CompileError> {
        let start = Instant::now();
        self.pass_manager
            .run_all(module)
            .map_err(CompileError::Pass)?;
        if self.config.optimization_level != OptimizationLevel::None
            && let Some(unified) = &mut self.last_unified
        {
            crate::core_opt::optimize_with_linkage_and_semantics(
                &mut unified.decls,
                self.config.entry_point.as_deref().or(Some("main")),
                crate::emit_profile::EmitProfile::Default,
                false,
                crate::target::integer_semantics(Some(&self.config.target)),
            );
        }
        self.timings.stages.push(StageTime {
            stage: Stage::Optimize,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(())
    }

    // ─── Stage 5: Codegen ──────────────────────────────────────────────

    pub fn codegen(&mut self, _module: &IrModule) -> Result<BackendOutput, CompileError> {
        let start = Instant::now();
        if self.config.artifact_kind != super::metadata::ArtifactKind::Executable {
            return Err(CompileError::Backend(
                "component driver currently supports executable emission only".into(),
            ));
        }
        let unified = self
            .last_unified
            .as_ref()
            .ok_or_else(|| CompileError::Backend("parse source before codegen".into()))?;
        let entry = self.config.entry_point.as_deref().unwrap_or("main");
        if !unified.decls.iter().any(
            |decl| matches!(decl, crate::core_ir::Decl::Function { name, .. } if name == entry),
        ) {
            return Err(CompileError::Backend(format!(
                "entry point `{entry}` not found"
            )));
        }
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("in-component-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir)
            .map_err(|e| CompileError::Backend(format!("create emission directory: {e}")))?;
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _temp = TempDir(dir.clone());
        let out = dir.join("artifact");
        let host_macho = self.backend == BackendKind::AArch64MachO
            && cfg!(all(target_os = "macos", target_arch = "aarch64"));
        if !host_macho && crate::owned_compile::pure_entry_exit_code(unified, entry).is_none() {
            return Err(CompileError::Backend(
                "cross-target executable emission requires a pure scalar exit entry".into(),
            ));
        }
        let request = crate::owned_compile::OwnedCompileRequest {
            path: dir.join("source.in"),
            module_id: self.config.name.clone(),
            parser: crate::parser_registry::ParserCli::In,
            target: crate::owned_compile::CompileTarget::Native,
            entry: self.config.entry_point.clone(),
            out: Some(out.clone()),
            linkage: crate::native_emit::NativeLinkage::Executable,
            target_triple: (!host_macho).then(|| self.config.target.clone()),
            jobs: 1,
            debug: self.config.debug_info,
            profile: crate::emit_profile::EmitProfile::Default,
            emit: None,
            base: None,
        };
        let result = crate::owned_compile::compile_native(unified, &self.config.name, &request)
            .map_err(CompileError::Backend)?;
        if !result.degradations.is_empty() {
            return Err(CompileError::Backend(format!(
                "native lowering degraded: {:?}",
                result.degradations
            )));
        }
        let output = BackendOutput {
            data: std::fs::read(&out)
                .map_err(|e| CompileError::Backend(format!("read emitted artifact: {e}")))?,
            extension: if matches!(
                self.backend,
                BackendKind::X86_64Coff | BackendKind::AArch64Coff
            ) {
                "exe"
            } else {
                "bin"
            },
            entry_offset: None,
            symbol_table: Vec::new(),
        };
        self.timings.stages.push(StageTime {
            stage: Stage::Codegen,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(output)
    }

    // ─── Full Pipeline ─────────────────────────────────────────────────

    /// Run the full compilation pipeline.
    pub fn compile(&mut self, source: &str) -> Result<CompileResult, CompileError> {
        self.timings = CompileTimings::default();
        let total_start = Instant::now();

        // Stage 1: Frontend
        let mut module = self.parse_source(source)?;

        // Stage 2: Type check
        self.type_check(&mut module)?;

        // Stage 3: Lower
        self.lower(&mut module)?;

        // Stage 4: Optimize
        self.optimize(&mut module)?;

        // Stage 5: Codegen
        let output = self.codegen(&module)?;

        // Build component metadata with real code sizes, source hash, separated capabilities
        let metadata = ComponentMetadata::build(&self.config, &module, &output, source);

        self.timings.total_us = total_start.elapsed().as_micros() as u64;

        Ok(CompileResult {
            module,
            output,
            metadata,
            timings: self.timings.clone(),
        })
    }

    /// Print a timing report.
    pub fn print_timings(&self) {
        println!("    Compiler pipeline stages:");
        for stage_time in &self.timings.stages {
            println!(
                "      {:12} {:.3}ms",
                stage_time.stage.label(),
                (stage_time.elapsed_us as f64) / 1000.0
            );
        }
        println!(
            "      {:12} {:.3}ms",
            "total",
            (self.timings.total_us as f64) / 1000.0
        );
    }
}

// ─── UnifiedModule → IrModule lowering ───────────────────────────────────

/// Lower a [`crate::core_ir::UnifiedModule`] to an [`IrModule`].
/// Extracts functions, structs, and strings from the AST-level representation
/// into the SSA-like compiler IR.
fn lower_unified_to_ir(unified: &crate::core_ir::UnifiedModule) -> Result<IrModule, CompileError> {
    let mut module = IrModule::new(unified.identity.module.as_deref().unwrap_or("module"));

    // Extract struct types
    for decl in &unified.decls {
        if let crate::core_ir::Decl::Struct { name, fields, .. } = decl {
            let ir_fields: Vec<(String, IrType)> = fields
                .iter()
                .map(|(n, t)| (n.clone(), lower_typ(t)))
                .collect();
            module.struct_types.push((name.clone(), ir_fields));
        }
    }

    // Extract functions with body awareness
    for decl in &unified.decls {
        if let crate::core_ir::Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            let ir_params: Vec<(String, IrType)> = params
                .iter()
                .map(|(n, t)| (n.clone(), lower_typ(t)))
                .collect();
            let ir_ret = lower_typ(ret);
            let ir_ret_for_block = ir_ret.clone();

            let mut func = IrFunction::new(name, ir_params, ir_ret);
            let mut block = IrBasicBlock::new("entry");

            let has_return = body
                .iter()
                .any(|s| matches!(s, crate::core_ir::Stmt::Return(_)));

            if !has_return && ir_ret_for_block == IrType::Void {
                block.terminator = Some(IrInstruction::new(IrOpcode::Return, IrType::Void, vec![]));
            } else {
                block.terminator = Some(IrInstruction::new(
                    IrOpcode::Return,
                    ir_ret_for_block,
                    vec![],
                ));
            }

            func.add_block(block);
            module.functions.push(func);
        }
    }

    Ok(module)
}

/// Lower a [`crate::core_ir::Typ`] to an [`IrType`].
fn lower_typ(typ: &crate::core_ir::Typ) -> IrType {
    match typ {
        crate::core_ir::Typ::Int => IrType::I64,
        crate::core_ir::Typ::Bool => IrType::Bool,
        crate::core_ir::Typ::String => IrType::Slice(Box::new(IrType::I8)),
        crate::core_ir::Typ::Float => IrType::F64,
        crate::core_ir::Typ::Void => IrType::Void,
        crate::core_ir::Typ::Array(elem) => IrType::Array(Box::new(lower_typ(elem)), 0),
        crate::core_ir::Typ::Vector(elem) => IrType::Array(Box::new(lower_typ(elem)), 0),
        crate::core_ir::Typ::Named(name) => IrType::Named(name.clone()),
        crate::core_ir::Typ::Generic(name) => IrType::Named(name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> ComponentSpec {
        let mut spec = ComponentSpec::host_executable("test_app", Some("main"));
        spec.target = "x86_64-unknown-linux-gnu".to_string();
        spec
    }

    #[test]
    fn compiler_resolves_backend_kind() {
        let compiler = Compiler::new(test_spec()).unwrap();
        assert_eq!(compiler.backend, BackendKind::X86_64Elf);
    }

    #[test]
    fn compiler_runs_full_pipeline() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let result = compiler.compile("fn main() -> Int { return 11; }").unwrap();
        assert_eq!(&result.output.data[..4], b"\x7fELF");
        assert_eq!(result.timings.stages.len(), 5);
    }

    #[test]
    fn compiler_parses_in_source() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let source = "fn greet() -> Int { return 42; }";
        let module = compiler.parse_source(source).unwrap();
        assert_eq!(module.functions.len(), 1);
        assert_eq!(module.functions[0].name, "greet");
    }

    #[test]
    fn compiler_returns_timing_report() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let _result = compiler.compile("fn main() -> Int { return 11; }").unwrap();
        compiler.print_timings();
    }

    #[test]
    fn malformed_source_clears_previous_module() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        compiler.parse_source("fn main() {}").unwrap();
        assert!(matches!(
            compiler.parse_source("fn main( {"),
            Err(CompileError::Frontend(_))
        ));
        assert!(compiler.last_unified.is_none());
    }

    #[test]
    fn repeated_compilation_resets_stage_timings() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        compiler.compile("fn main() -> Int { return 11; }").unwrap();
        assert_eq!(
            compiler
                .compile("fn main() {}")
                .unwrap()
                .timings
                .stages
                .len(),
            5
        );
    }

    #[test]
    fn codegen_rejects_missing_entry_and_unsupported_artifact() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        assert!(matches!(
            compiler.compile("fn greet() {}"),
            Err(CompileError::Backend(_))
        ));
        compiler.config.artifact_kind = super::super::metadata::ArtifactKind::StaticLibrary;
        assert!(matches!(
            compiler.compile("fn main() {}"),
            Err(CompileError::Backend(_))
        ));
    }

    #[test]
    fn cross_target_codegen_rejects_entry_side_effects() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let module = compiler
            .parse_source("var count: Int = 0;\nfn main() -> Int { count = 1; return 11; }")
            .unwrap();
        assert!(matches!(
            compiler.codegen(&module),
            Err(CompileError::Backend(_))
        ));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn host_pipeline_emits_runnable_native_program() {
        for (source, expected) in [
            ("fn main() -> Int { return 11; }", 11),
            (
                include_str!("../../../conformance/types/mutable-global.in"),
                13,
            ),
        ] {
            let mut compiler =
                Compiler::new(ComponentSpec::host_executable("test_app", Some("main"))).unwrap();
            let result = compiler.compile(source).unwrap();
            assert_eq!(&result.output.data[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
            let path =
                std::env::temp_dir().join(format!("in-pipeline-execution-{}", std::process::id()));
            std::fs::write(&path, result.output.data).unwrap();
            crate::owned_compile::set_native_artifact_permissions(
                &path,
                crate::native_emit::NativeLinkage::Executable,
            )
            .unwrap();
            let status = std::process::Command::new(&path).status();
            std::fs::remove_file(path).unwrap();
            assert_eq!(status.unwrap().code(), Some(expected), "{source}");
        }
    }

    #[test]
    fn lower_unified_module_produces_ir() {
        let source = "fn main() {}\nfn add(a: Int, b: Int) -> Int { return a + b; }";
        let unified = crate::in_lang_parse::parse_in_source(source).unwrap();
        let module = lower_unified_to_ir(&unified).unwrap();
        assert!(module.functions.len() >= 1);
        assert_eq!(module.functions[0].name, "main");
    }
}
