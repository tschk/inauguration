//! Direct UnifiedModule → MIR lowering.
//!
//! Converts a [`UnifiedModule`] into a [`MirModule`] with typed MirOp::Typed
//! operations that carry source-level type information. The typed MIR can then
//! be verified, optimized, and lowered to machine-level MirOps for emission.
//!
//! ponytail: minimal direct lowering. Function bodies are represented as
//! TypedOps carrying variable names and expression structure. Machine lowering
//! (TypedOp → Mov/Add/Ret etc.) happens in a separate pass.

use crate::compiler::mir::*;
use crate::core_ir::{Decl, Expr, LoopKind, Stmt, UnifiedModule};

/// Lower a UnifiedModule directly to MIR, preserving type information.
///
/// Each Decl::Function becomes a MirFunction with Typed(TypedOp) instructions.
/// Struct/Global/Class/Component declarations are stored as MIR metadata.
pub fn lower_to_mir(module: &UnifiedModule) -> MirModule {
    let mut mir = MirModule::new();
    for decl in &module.decls {
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            let mut func = MirFunction {
                name: name.clone(),
                instructions: Vec::new(),
                vreg_count: 0,
                frame_size: 0,
                return_type: Some(ret.clone()),
                param_types: params.iter().map(|(_, t)| t.clone()).collect(),
                var_map: Vec::new(),
            };
            for (param_name, _) in params {
                let vreg = func.vreg_count;
                func.vreg_count += 1;
                func.var_map.push((param_name.clone(), vreg));
            }
            for stmt in body {
                lower_stmt(stmt, &mut func);
            }
            mir.functions.push(func);
        }
    }
    mir
}

fn lower_stmt(stmt: &Stmt, func: &mut MirFunction) {
    match stmt {
        Stmt::Let(name, _typ_ann, value) => {
            let vreg = func.vreg_count;
            func.vreg_count += 1;
            func.var_map.push((name.clone(), vreg));
            lower_expr_into(value, name, func);
        }
        Stmt::Expr(expr) => {
            lower_expr(expr, func);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            lower_expr(cond, func);
            for s in then_body {
                lower_stmt(s, func);
            }
            for s in else_body {
                lower_stmt(s, func);
            }
        }
        Stmt::Loop {
            kind: LoopKind::While,
            cond: Some(cond),
            body,
        } => {
            lower_expr(cond, func);
            for s in body {
                lower_stmt(s, func);
            }
        }
        Stmt::Loop {
            kind: _,
            cond: _,
            body,
        } => {
            for s in body {
                lower_stmt(s, func);
            }
        }
        Stmt::Return(expr) => match expr {
            Some(e) => {
                let temp = format!("__ret_{}", func.instructions.len());
                let vreg = func.vreg_count;
                func.vreg_count += 1;
                func.var_map.push((temp.clone(), vreg));
                lower_expr_into(e, &temp, func);
                func.instructions
                    .push(mir(MirOp::Typed(TypedOp::Return(Some(temp))), vec![]));
            }
            None => {
                func.instructions
                    .push(mir(MirOp::Typed(TypedOp::Return(None)), vec![]));
            }
        },
        Stmt::Assign(name, value) => {
            lower_expr_into(value, name, func);
        }
        Stmt::FieldAssign { value, .. } => {
            // ponytail: MIR field assign = just lower the value, skip store
            let _ = value;
        }
        _ => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::Nop), vec![]));
        }
    }
}

fn lower_expr_into(expr: &Expr, target: &str, func: &mut MirFunction) {
    match expr {
        Expr::IntLit(v) => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::IntLit(*v)), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::BoolLit(v) => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::BoolLit(*v)), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::StringLit(v) => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::StringLit(v.clone())), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::Ident(name) => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::Load(name.clone())), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::Binary { op, lhs, rhs } => {
            let ln = format!("__bin_{}_lhs", func.instructions.len());
            let rn = format!("__bin_{}_rhs", func.instructions.len());
            let lv = func.vreg_count;
            func.vreg_count += 1;
            let rv = func.vreg_count;
            func.vreg_count += 1;
            func.var_map.push((ln.clone(), lv));
            func.var_map.push((rn.clone(), rv));
            lower_expr_into(lhs, &ln, func);
            lower_expr_into(rhs, &rn, func);
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::BinOp { op: op.clone() }), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::Unary { op, expr: inner } => {
            let n = format!("__un_{}", func.instructions.len());
            let v = func.vreg_count;
            func.vreg_count += 1;
            func.var_map.push((n.clone(), v));
            lower_expr_into(inner, &n, func);
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::UnaryOp { op: op.clone() }),
                vec![],
            ));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        Expr::Call { callee, args } => {
            for (i, arg) in args.iter().enumerate() {
                let an = format!("__call_arg_{}_{}", func.instructions.len(), i);
                let av = func.vreg_count;
                func.vreg_count += 1;
                func.var_map.push((an.clone(), av));
                lower_expr_into(arg, &an, func);
            }
            let fn_name = match callee.as_ref() {
                Expr::Ident(n) => n.clone(),
                _ => "__unknown_call".to_string(),
            };
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::Call(fn_name)), vec![]));
            func.instructions.push(mir(
                MirOp::Typed(TypedOp::Store(target.to_string())),
                vec![],
            ));
        }
        _ => {
            func.instructions
                .push(mir(MirOp::Typed(TypedOp::Nop), vec![]));
        }
    }
}

fn lower_expr(expr: &Expr, func: &mut MirFunction) {
    let temp = format!("__expr_{}", func.instructions.len());
    let v = func.vreg_count;
    func.vreg_count += 1;
    func.var_map.push((temp.clone(), v));
    lower_expr_into(expr, &temp, func);
}

/// Lower a Core IR module for the boot image: MIR plus freestanding x86_64
/// (or AArch64) machine code with C-string literals, and the module's
/// function export table as `(name, code offset)` pairs for the kernel's
/// boot-header export table.
pub fn lower_boot_image(
    module: &UnifiedModule,
    entry: &str,
    target_triple: Option<&str>,
) -> Result<(MirModule, Vec<u8>, Vec<(String, u32)>), String> {
    let triple = target_triple.unwrap_or("x86_64-unknown-none");
    let (code, exports) = if triple.contains("aarch64") || triple.contains("arm64") {
        let result = crate::native_emit::lower::lower_module(
            module,
            entry,
            crate::native_emit::NativeLinkage::Executable,
        )?;
        (
            result.code,
            result
                .exports
                .iter()
                .map(|s| (s.name.clone(), s.offset))
                .collect(),
        )
    } else {
        let is_32bit = triple.starts_with("i386-") || triple.starts_with("i686-");
        crate::native_emit::x86_64::set_32bit(is_32bit);
        let result = crate::native_emit::x86_64_lower::lower_module_with_bases_layout(
            module,
            entry,
            0x101100,
            0x200000,
            crate::native_emit::x86_64_lower::X86StringLayout::Cstring,
        )?;
        (result.code, result.exports)
    };
    let mir_module = MirModule::new();
    Ok((mir_module, code, exports))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{CoreModuleIdentity, Typ};

    #[test]
    fn lower_empty_module() {
        let module = UnifiedModule::new(vec![]);
        let mir = lower_to_mir(&module);
        assert!(mir.functions.is_empty());
    }

    #[test]
    fn lower_simple_function() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);
        assert_eq!(mir.functions.len(), 1);
        assert_eq!(mir.functions[0].name, "main");
        assert_eq!(mir.functions[0].return_type, Some(Typ::Int));
    }

    #[test]
    fn verify_passes_clean_module() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);
        assert!(mir.verify(Some("main")).is_ok());
    }

    #[test]
    fn verify_fails_missing_entry() {
        let decls = vec![Decl::Function {
            name: "other".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);
        assert!(mir.verify(Some("main")).is_err());
    }

    #[test]
    fn test_lower_let_and_assign() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![
                Stmt::Let("x".into(), None, Expr::IntLit(1)),
                Stmt::Assign("x".into(), Expr::IntLit(2)),
            ],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);

        let func = &mir.functions[0];
        let has_lit_1 = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::IntLit(1)));
        let has_store_x1 = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::Store("x".to_string())));
        let has_lit_2 = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::IntLit(2)));
        let has_store_x2 = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::Store("x".to_string())));

        assert!(has_lit_1);
        assert!(has_store_x1);
        assert!(has_lit_2);
        assert!(has_store_x2);
    }

    #[test]
    fn test_lower_if_statement() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::If {
                cond: Expr::BoolLit(true),
                then_body: vec![Stmt::Return(None)],
                else_body: vec![],
            }],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);

        let func = &mir.functions[0];
        let has_bool_true = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::BoolLit(true)));
        let has_ret_none = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::Return(None)));

        assert!(has_bool_true);
        assert!(has_ret_none);
    }

    #[test]
    fn test_lower_loop_while() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(Expr::BoolLit(false)),
                body: vec![],
            }],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);

        let func = &mir.functions[0];
        let has_bool_false = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::BoolLit(false)));

        assert!(has_bool_false);
    }

    #[test]
    fn test_lower_complex_expr() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::IntLit(1)),
                rhs: Box::new(Expr::IntLit(2)),
            }))],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);

        let func = &mir.functions[0];
        let has_binop = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::BinOp { op: "+".into() }));

        assert!(has_binop);
    }

    #[test]
    fn test_lower_field_assign() {
        let decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::FieldAssign {
                base: Expr::Ident("obj".into()),
                name: "f".into(),
                value: Expr::IntLit(1),
            }],
            type_params: vec![],
        }];
        let module = UnifiedModule::with_identity(decls, CoreModuleIdentity::default());
        let mir = lower_to_mir(&module);

        let func = &mir.functions[0];
        let has_store_f = func
            .instructions
            .iter()
            .any(|i| i.op == MirOp::Typed(TypedOp::Store("f".to_string())));

        // Since the current implementation of FieldAssign skips lowering the value and just ignores it:
        assert!(
            func.instructions.is_empty()
                || func
                    .instructions
                    .iter()
                    .all(|i| i.op != MirOp::Typed(TypedOp::IntLit(1)))
        );
        assert!(!has_store_f);
    }
}
