//! Inauguration compiler — multi-language frontends, Core IR, passes, and backends.
//!
//! This is the proper compiler that replaces LLVM:
//!
//! 1. **Frontend**: `.in` parser (`in_lang_parse`), Tree-sitter polyglot (`tree_front`),
//!    language-specific parsers all produce [`crate::core_ir::UnifiedModule`]
//! 2. **Core IR**: [`compiler::pipeline::lower_unified_to_ir`] converts AST to SSA-like `IrModule`
//! 3. **Type Check**: type checking on the IR
//! 4. **Lower**: additional IR transforms
//! 5. **Optimize**: [`PassManager`] runs `SimplifyCFG`, `ConstantFolding`, `DCE`, `SROA`, `Cleanup`
//! 6. **Codegen**: [`CodegenBackend`] emits machine code via `native_emit/*`
//! 7. **Metadata**: [`ComponentMetadata`] JSON sidecar with imports, exports, capabilities

pub mod backend;
pub mod boundary_common;
pub mod core;
pub mod driver;
pub mod icore;
pub mod metadata;
pub mod mir;
pub mod mir_emit;
#[cfg(test)]
pub mod mir_emit_tests;
pub mod mir_emit_x86;
pub mod mir_lower;
pub mod mir_scalar_lower;
pub mod passes;
pub mod pipeline;
pub mod rust_front;
pub mod tree_front;
