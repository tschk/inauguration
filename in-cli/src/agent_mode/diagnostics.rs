use super::types::{
    AgentDiagnostic, AgentDiagnosticSeverity, AgentSourceSpan, DiagnosticExplanation,
    SourceExcerptBounds,
};
use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
use crate::native_emit::lower::{DEGRADATION_SKIPPED_FUNCTION, DEGRADATION_UNRESOLVED_CALL};
use crate::package_manifest::PackageSymbolIndexEntry;

pub(super) fn diagnostic(
    code: &str,
    severity: AgentDiagnosticSeverity,
    span: Option<AgentSourceSpan>,
    parser_id: Option<&str>,
    expected_shape: Option<&str>,
    source_excerpt_bounds: Option<SourceExcerptBounds>,
    repair_hint: Option<&str>,
    message: &str,
) -> AgentDiagnostic {
    AgentDiagnostic {
        code: code.to_string(),
        severity,
        span,
        parser_id: parser_id.map(str::to_string),
        expected_shape: expected_shape.map(str::to_string),
        source_excerpt_bounds,
        repair_hint: repair_hint.map(str::to_string),
        message: message.to_string(),
    }
}

pub(super) fn core_diagnostics(
    module: &UnifiedModule,
    parser_id: Option<&str>,
    source: &str,
) -> Vec<AgentDiagnostic> {
    let has_main = module
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "main"));
    if has_main {
        Vec::new()
    } else {
        vec![diagnostic(
            "AGENT_NO_MAIN",
            AgentDiagnosticSeverity::Warning,
            None,
            parser_id,
            Some("function named main for entrypoint-shaped reports"),
            excerpt_bounds(source, None),
            Some("Add a top-level main function or confirm this module is intended as a library"),
            "Core IR module has no main function",
        )]
    }
}

pub(super) fn dependency_symbol_call_diagnostics(
    module: &UnifiedModule,
    symbols: &[PackageSymbolIndexEntry],
    bound_aliases: &std::collections::BTreeSet<&str>,
    parser_id: Option<&str>,
    source: &str,
) -> Vec<AgentDiagnostic> {
    if symbols.is_empty() {
        return Vec::new();
    }
    let dependency_symbols = symbols
        .iter()
        .map(|symbol| {
            (
                symbol.name.as_str(),
                symbol.source_import.as_str(),
                symbol.dependency.as_str(),
            )
        })
        .collect::<Vec<_>>();
    let functions = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut calls = std::collections::BTreeSet::new();
    collect_dependency_symbol_calls(
        module,
        &dependency_symbols,
        &functions,
        bound_aliases,
        &mut calls,
    );
    calls
        .into_iter()
        .map(|(name, source_import, dependency)| {
            diagnostic(
                "INPKG002",
                AgentDiagnosticSeverity::Warning,
                None,
                parser_id,
                Some("call through an explicit runtime binding, generated adapter, or local wrapper"),
                excerpt_bounds(source, None),
                Some("Keep the dependency import for graph identity, then add an explicit wrapper before calling it"),
                &format!(
                    "dependency symbol `{name}` from semantic import `{source_import}` resolves to package dependency `{dependency}`, but runtime binding is not implemented"
                ),
            )
        })
        .collect()
}

fn collect_dependency_symbol_calls<'a>(
    module: &'a UnifiedModule,
    symbols: &[(&'a str, &'a str, &'a str)],
    functions: &std::collections::BTreeSet<&'a str>,
    bound_aliases: &std::collections::BTreeSet<&'a str>,
    out: &mut std::collections::BTreeSet<(&'a str, &'a str, &'a str)>,
) {
    for decl in &module.decls {
        if let Decl::Function { body, .. } = decl {
            for stmt in body {
                collect_dependency_symbol_calls_from_stmt(
                    stmt,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
    }
}

fn collect_dependency_symbol_calls_from_stmt<'a>(
    stmt: &'a Stmt,
    symbols: &[(&'a str, &'a str, &'a str)],
    functions: &std::collections::BTreeSet<&'a str>,
    bound_aliases: &std::collections::BTreeSet<&'a str>,
    out: &mut std::collections::BTreeSet<(&'a str, &'a str, &'a str)>,
) {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Return(Some(expr))
        | Stmt::Expr(expr) => {
            collect_dependency_symbol_calls_from_expr(expr, symbols, functions, bound_aliases, out);
        }
        Stmt::FieldAssign { base, value, .. } => {
            collect_dependency_symbol_calls_from_expr(base, symbols, functions, bound_aliases, out);
            collect_dependency_symbol_calls_from_expr(
                value,
                symbols,
                functions,
                bound_aliases,
                out,
            );
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            collect_dependency_symbol_calls_from_expr(base, symbols, functions, bound_aliases, out);
            collect_dependency_symbol_calls_from_expr(
                index,
                symbols,
                functions,
                bound_aliases,
                out,
            );
            collect_dependency_symbol_calls_from_expr(
                value,
                symbols,
                functions,
                bound_aliases,
                out,
            );
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            collect_dependency_symbol_calls_from_expr(cond, symbols, functions, bound_aliases, out);
            for nested in then_body {
                collect_dependency_symbol_calls_from_stmt(
                    nested,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
            for nested in else_body {
                collect_dependency_symbol_calls_from_stmt(
                    nested,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                collect_dependency_symbol_calls_from_expr(
                    cond,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
            for nested in body {
                collect_dependency_symbol_calls_from_stmt(
                    nested,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            collect_dependency_symbol_calls_from_expr(
                scrutinee,
                symbols,
                functions,
                bound_aliases,
                out,
            );
            for arm in arms {
                for nested in &arm.body {
                    collect_dependency_symbol_calls_from_stmt(
                        nested,
                        symbols,
                        functions,
                        bound_aliases,
                        out,
                    );
                }
            }
        }
        Stmt::Return(None) => {}
        Stmt::Break => {}
        Stmt::Throw(_) | Stmt::Try { .. } | Stmt::Propagate => {}
    }
}

fn collect_dependency_symbol_calls_from_expr<'a>(
    expr: &'a Expr,
    symbols: &[(&'a str, &'a str, &'a str)],
    functions: &std::collections::BTreeSet<&'a str>,
    bound_aliases: &std::collections::BTreeSet<&'a str>,
    out: &mut std::collections::BTreeSet<(&'a str, &'a str, &'a str)>,
) {
    match expr {
        Expr::Unary { expr, .. } => {
            collect_dependency_symbol_calls_from_expr(expr, symbols, functions, bound_aliases, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_dependency_symbol_calls_from_expr(lhs, symbols, functions, bound_aliases, out);
            collect_dependency_symbol_calls_from_expr(rhs, symbols, functions, bound_aliases, out);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_dependency_symbol_calls_from_expr(
                    expr,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
        Expr::Field { base, .. } => {
            collect_dependency_symbol_calls_from_expr(base, symbols, functions, bound_aliases, out);
        }
        Expr::ArrayLit(items) => {
            for item in items {
                collect_dependency_symbol_calls_from_expr(
                    item,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
        Expr::Index { base, index, .. } => {
            collect_dependency_symbol_calls_from_expr(base, symbols, functions, bound_aliases, out);
            collect_dependency_symbol_calls_from_expr(
                index,
                symbols,
                functions,
                bound_aliases,
                out,
            );
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_ref()
                && !functions.contains(name.as_str())
                && !bound_aliases.contains(name.as_str())
                && let Some(symbol) = symbols
                    .iter()
                    .find(|(symbol_name, _, _)| symbol_name == &name.as_str())
            {
                out.insert(*symbol);
            }
            collect_dependency_symbol_calls_from_expr(
                callee,
                symbols,
                functions,
                bound_aliases,
                out,
            );
            for arg in args {
                collect_dependency_symbol_calls_from_expr(
                    arg,
                    symbols,
                    functions,
                    bound_aliases,
                    out,
                );
            }
        }
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
        Expr::Closure { .. } => {}
    }
}

pub fn explain_diagnostic(code: &str) -> Option<DiagnosticExplanation> {
    // Lowerer codes are matched through their constants so the explanation cannot
    // drift from the code the compiler actually emits.
    if code == DEGRADATION_SKIPPED_FUNCTION {
        return Some(DiagnosticExplanation {
            code: code.to_string(),
            severity: AgentDiagnosticSeverity::Warning,
            expected_shape: "a function the AArch64 lowerer can compile".to_string(),
            repair_hint: "Rewrite the function to stay inside the native subset, or move it behind an extern binding".to_string(),
            meaning: "The native lowerer could not compile this function, so its body is a trap that reports the reason and exits with the trap status if reached".to_string(),
            fix: "Simplify the function's types and constructs until it lowers, or call it through a boundary that is not compiled natively".to_string(),
        });
    }
    if code == DEGRADATION_UNRESOLVED_CALL {
        return Some(DiagnosticExplanation {
            code: code.to_string(),
            severity: AgentDiagnosticSeverity::Warning,
            expected_shape: "a call target with a definition in the compiled unit".to_string(),
            repair_hint: "Add the missing definition, import it, or declare it as an extern binding".to_string(),
            meaning: "A call names a function with no definition in the compiled unit, so the call site is a trap".to_string(),
            fix: "Define the callee, resolve its package or crate, or declare it as an extern binding".to_string(),
        });
    }
    let (severity, expected_shape, repair_hint, meaning, fix) = match code {
        "AGENT_NO_MAIN" | "INAGENT010" => (
            AgentDiagnosticSeverity::Warning,
            "function named main for entrypoint-shaped reports",
            "Add a top-level main function or confirm this module is intended as a library",
            "Core IR module has no explicit main entrypoint",
            "Add a top-level main function or treat the module as a library",
        ),
        "AGENT_PARSE_FAILED" | "INAGENT020" => (
            AgentDiagnosticSeverity::Error,
            "source accepted by the resolved parser and lowered to Core IR",
            "Check the parser decision, source extension, magic parser line, and frontend-supported syntax",
            "The resolved parser could not convert the source into Core IR",
            "Check parser selection, extension, magic parser line, and frontend-supported syntax",
        ),
        "AGENT_SWIFT_SIL_ROUTE" | "INAGENT030" => (
            AgentDiagnosticSeverity::Info,
            "Core IR parser route for agent-mode reports",
            "Pass a .in/.icore/polyglot source with a registered Core IR frontend or force parser=in/icore",
            "Parser resolution selected the Swift SIL emit route",
            "Use a registered Core IR route for agent-mode JSON reporting",
        ),
        "AGENT_IO_ERROR" | "INAGENT040" => (
            AgentDiagnosticSeverity::Error,
            "readable source file",
            "Check that the source path exists and is readable",
            "The agent-mode module could not read the requested source file",
            "Check that the path exists and is readable from the invocation directory",
        ),
        "AGENT_MISSING_CAPABILITY" | "INAGENT050" => (
            AgentDiagnosticSeverity::Warning,
            "top-level capability declaration matching an extern binding requirement",
            "Add the missing top-level capability declaration or remove the extern requirement",
            "An .in extern binding declares a required capability that the module did not declare",
            "Add the missing top-level capability declaration",
        ),
        "INPKG001" => (
            AgentDiagnosticSeverity::Warning,
            "top-level use declaration matching a dependency in the nearest inauguration.package",
            "Declare the dependency in inauguration.package or remove the semantic import",
            "An .in semantic package import does not resolve against the nearest package manifest",
            "Add the missing dependency to inauguration.package",
        ),
        "INPKG002" => (
            AgentDiagnosticSeverity::Warning,
            "call through an explicit runtime binding, generated adapter, or local wrapper",
            "Keep the dependency import for graph identity, then add an explicit wrapper before calling it",
            "An .in source calls a resolved dependency symbol directly, but dependency runtime binding is not implemented",
            "Add an explicit local function or extern binding that wraps the dependency",
        ),
        _ => return None,
    };
    Some(DiagnosticExplanation {
        code: code.to_string(),
        severity,
        expected_shape: expected_shape.to_string(),
        repair_hint: repair_hint.to_string(),
        meaning: meaning.to_string(),
        fix: fix.to_string(),
    })
}

pub(super) fn excerpt_bounds(
    source: &str,
    span: Option<&AgentSourceSpan>,
) -> Option<SourceExcerptBounds> {
    if source.is_empty() {
        return None;
    }
    let (start, end) = match span {
        Some(span) => (span.byte_start, span.byte_end),
        None => (0, source.len().min(240)),
    };
    let start = start.min(source.len());
    let end = end.max(start).min(source.len());
    Some(SourceExcerptBounds {
        byte_start: start,
        byte_end: end,
    })
}

pub(super) fn missing_capability_from_message(message: &str) -> Option<String> {
    message
        .rsplit_once(" missing capability ")
        .map(|(_, capability)| capability.trim().to_string())
        .filter(|capability| !capability.is_empty())
}
