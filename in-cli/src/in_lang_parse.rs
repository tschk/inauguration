//! `.in` v0.2: top-level `struct` / `fn` with multiline struct bodies and minimal `fn` bodies.

pub mod decl;
pub mod expr;
pub mod lexer;
pub mod module;
pub mod stmt;
pub mod surface;
#[cfg(test)]
pub mod tests;
pub mod types;
pub mod util;
pub mod validate;

pub use module::{parse_in_file, parse_in_library_file, parse_in_library_source, parse_in_source};
pub use surface::{
    InAnnotationFact, InExternBinding, InOrchestrationFacts, InParallelTaskFact, InSemanticBinding,
    InSurfaceInfo, in_standard_import_bindings, parse_in_surface_info,
};
pub use validate::inline_const_values;

#[cfg(test)]
pub(crate) use crate::core_ir::{Decl, Expr, LoopKind, Stmt, Typ};
#[cfg(test)]
pub(crate) use expr::parse_expr;
#[cfg(test)]
pub(crate) use lexer::split_top_level_decl_blocks;
