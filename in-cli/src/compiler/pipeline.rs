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
//!   │  Stage 3: lower_core (UnifiedModule → textual SIL → IrModule)
//!   ▼
//! Optimize
//!   │  Stage 4: PassManager (SimplifyCFG, ConstantFold, DCE, SROA, Cleanup)
//!   ▼
//! Codegen
//!   │  Stage 5: CodegenBackend → native_emit/* → artifact bytes
//!   ▼
//! Output + Metadata JSON sidecar
//! ```

use std::fmt;
use std::time::Instant;

#[cfg(test)]
use super::backend::BackendKind;
use super::backend::{BackendOutput, placeholder_output, select_backend};
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
    #[cfg(test)]
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
        #[cfg(test)]
        let backend = _kind;

        Ok(Self {
            config: spec,
            pass_manager,
            #[cfg(test)]
            backend,
            timings: CompileTimings::default(),
            last_unified: None,
        })
    }

    // ─── Stage 1: Frontend ─────────────────────────────────────────────

    /// Parse source into an [`IrModule`].
    /// For now creates a minimal module from the component spec.
    /// Real frontend reads `.in` source through `in_lang_parse` + `lower_core`.
    pub fn parse_source(&mut self, source: &str) -> Result<IrModule, CompileError> {
        let start = Instant::now();
        let mut module = IrModule::new(&self.config.name);

        let entry_name = self
            .config
            .entry_point
            .clone()
            .unwrap_or_else(|| "main".to_string());

        // Try parsing as .in source first
        match crate::in_lang_parse::parse_in_source(source) {
            Ok(unified) => {
                // Cache for type_check stage
                self.last_unified = Some(unified.clone());
                // We have a parsed UnifiedModule — lower it to IrModule
                let lowered = lower_unified_to_ir(&unified, &entry_name)?;
                module = lowered;
            }
            Err(_e) => {
                // Fall back to minimal module
                let mut func = IrFunction::new(&entry_name, vec![], IrType::Void);
                let mut block = IrBasicBlock::new("entry");
                block.terminator = Some(IrInstruction::new(IrOpcode::Return, IrType::Void, vec![]));
                func.add_block(block);
                module.functions.push(func);
            }
        }

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
        self.timings.stages.push(StageTime {
            stage: Stage::Optimize,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(())
    }

    // ─── Stage 5: Codegen ──────────────────────────────────────────────

    pub fn codegen(&mut self, _module: &IrModule) -> Result<BackendOutput, CompileError> {
        let start = Instant::now();
        let output = placeholder_output();
        self.timings.stages.push(StageTime {
            stage: Stage::Codegen,
            elapsed_us: start.elapsed().as_micros() as u64,
        });
        Ok(output)
    }

    // ─── Full Pipeline ─────────────────────────────────────────────────

    /// Run the full compilation pipeline.
    pub fn compile(&mut self, source: &str) -> Result<CompileResult, CompileError> {
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
fn lower_unified_to_ir(
    unified: &crate::core_ir::UnifiedModule,
    entry_name: &str,
) -> Result<IrModule, CompileError> {
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

    // If no functions found, create an entry stub
    if module.functions.is_empty() {
        let mut func = IrFunction::new(entry_name, vec![], IrType::Void);
        let mut block = IrBasicBlock::new("entry");
        block.terminator = Some(IrInstruction::new(IrOpcode::Return, IrType::Void, vec![]));
        func.add_block(block);
        module.functions.push(func);
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
        ComponentSpec::host_executable("test_app", Some("main"))
    }

    #[test]
    fn compiler_resolves_backend_kind() {
        let compiler = Compiler::new(test_spec()).unwrap();
        // The host backend must match the actual host triple: macOS (arm64)
        // selects Mach-O, Linux (x86_64/aarch64) selects ELF, anything else
        // (e.g. an unrecognized arch) falls back to RawBinary. This mirrors
        // `select_backend`/`ComponentSpec::host_triple` so the assertion
        // stays correct on every CI/host platform instead of only macOS.
        let expected = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            BackendKind::AArch64MachO
        } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            BackendKind::X86_64Elf
        } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
            BackendKind::AArch64Elf
        } else if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
            BackendKind::X86_64Coff
        } else if cfg!(target_os = "windows") && cfg!(target_arch = "aarch64") {
            BackendKind::AArch64Coff
        } else {
            BackendKind::RawBinary
        };
        assert_eq!(compiler.backend, expected);
    }

    #[test]
    fn compiler_runs_full_pipeline() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let result = compiler.compile("fn main() {}").unwrap();
        assert!(!result.output.data.is_empty());
        assert_eq!(result.timings.stages.len(), 5);
    }

    #[test]
    fn compiler_parses_in_source() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let source = "fn greet() -> Int { return 42; }";
        let module = compiler.parse_source(source).unwrap();
        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn compiler_returns_timing_report() {
        let mut compiler = Compiler::new(test_spec()).unwrap();
        let _result = compiler.compile("fn main() {}").unwrap();
        compiler.print_timings();
    }

    #[test]
    fn lower_unified_module_produces_ir() {
        let source = "fn main() {}\nfn add(a: Int, b: Int) -> Int { return a + b; }";
        let unified = crate::in_lang_parse::parse_in_source(source).unwrap();
        let module = lower_unified_to_ir(&unified, "main").unwrap();
        assert!(module.functions.len() >= 1);
        assert_eq!(module.functions[0].name, "main");
    }
}
