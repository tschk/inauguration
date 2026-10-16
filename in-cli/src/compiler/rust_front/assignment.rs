//! Rust assignment produces Unit; tuple assignees stage all values before stores.
use super::*;

fn ungroup(mut expr: &syn::Expr) -> &syn::Expr {
    loop {
        expr = match expr {
            syn::Expr::Paren(value) => &value.expr,
            syn::Expr::Group(value) => &value.expr,
            _ => return expr,
        };
    }
}

fn field_name(field: &syn::Member) -> String {
    match field {
        syn::Member::Unnamed(index) => format!("_{}", index.index),
        syn::Member::Named(name) => name.to_string(),
    }
}

fn unsupported(compound: bool) -> Stmt {
    Stmt::Expr(Expr::Call {
        callee: Box::new(Expr::Ident(
            if compound {
                crate::core_ir::RUST_COMPOUND_ASSIGNMENT_TARGET
            } else {
                crate::core_ir::RUST_ASSIGNMENT_TARGET
            }
            .into(),
        )),
        args: Vec::new(),
    })
}

fn pure_field_base(expr: &syn::Expr) -> bool {
    match ungroup(expr) {
        syn::Expr::Path(_) => true,
        syn::Expr::Field(field) => pure_field_base(&field.base),
        _ => false,
    }
}

fn store(target: &syn::Expr, value: Expr, types: &mut HashMap<String, String>) -> Option<Stmt> {
    match ungroup(target) {
        syn::Expr::Path(path) => {
            assign_lhs_name(&syn::Expr::Path(path.clone())).map(|name| Stmt::Assign(name, value))
        }
        syn::Expr::Field(field) => Some(Stmt::FieldAssign {
            base: lower_expr_with_types(&field.base, types),
            name: field_name(&field.member),
            value,
        }),
        syn::Expr::Index(index) => Some(Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INDEX_ASSIGN_VALUE_FIRST.into())),
            args: vec![
                lower_expr_with_types(&index.expr, types),
                lower_expr_with_types(&index.index, types),
                value,
            ],
        })),
        syn::Expr::Infer(_) => Some(Stmt::Expr(value)),
        _ => None,
    }
}

fn tuple_leaves<'a>(
    target: &'a syn::Expr,
    value: Expr,
    staged: &mut Vec<Stmt>,
    leaves: &mut Vec<(&'a syn::Expr, Expr)>,
) -> bool {
    match ungroup(target) {
        syn::Expr::Tuple(tuple) => {
            let rest = tuple.elems.iter().position(|expr| matches!(ungroup(expr),syn::Expr::Range(range) if range.start.is_none()&&range.end.is_none()&&matches!(range.limits,syn::RangeLimits::HalfOpen(_))));
            if tuple
                .elems
                .iter()
                .filter(|expr| matches!(ungroup(expr), syn::Expr::Range(_)))
                .count()
                != usize::from(rest.is_some())
            {
                return false;
            }
            staged.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::tuples::LET_CHECK.into())),
                args: vec![
                    value.clone(),
                    Expr::IntLit((tuple.elems.len() - usize::from(rest.is_some())) as i64),
                    Expr::BoolLit(rest.is_some()),
                ],
            }));
            for (index, target) in tuple.elems.iter().enumerate() {
                if Some(index) == rest {
                    continue;
                }
                let from_end = rest.is_some_and(|rest| index > rest);
                let field_index = if from_end {
                    tuple.elems.len() - index - 1
                } else {
                    index
                };
                let field = Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::tuples::LET_FIELD.into())),
                    args: vec![
                        value.clone(),
                        Expr::IntLit(field_index as i64),
                        Expr::BoolLit(from_end),
                    ],
                };
                if !tuple_leaves(target, field, staged, leaves) {
                    return false;
                }
            }
            true
        }
        syn::Expr::Infer(_) => true,
        syn::Expr::Path(_) | syn::Expr::Field(_) | syn::Expr::Index(_) => {
            // Rust source identifiers cannot contain '#'. The enclosing Block
            // scopes these names independently of nested assignments.
            let name = format!("#tuple-let-assignment-leaf-{}", leaves.len());
            staged.push(Stmt::Let(name.clone(), None, value));
            leaves.push((target, Expr::Ident(name)));
            true
        }
        _ => false,
    }
}

pub(super) fn lower_statement(
    assign: &syn::ExprAssign,
    out: &mut Vec<Stmt>,
    types: &mut HashMap<String, String>,
) {
    if matches!(ungroup(&assign.left), syn::Expr::Tuple(_)) {
        let rhs_name = "#tuple-let-assignment-rhs";
        let mut staged = Vec::new();
        let mut leaves = Vec::new();
        if !tuple_leaves(
            &assign.left,
            Expr::Ident(rhs_name.into()),
            &mut staged,
            &mut leaves,
        ) {
            out.push(unsupported(false));
            return;
        }
        let mut body = vec![Stmt::Let(
            rhs_name.into(),
            None,
            lower_expr_with_types(&assign.right, types),
        )];
        body.extend(staged);
        for (target, value) in leaves {
            let Some(stmt) = store(target, value, types) else {
                out.push(unsupported(false));
                return;
            };
            body.push(stmt);
        }
        out.push(Stmt::Expr(Expr::Block {
            body,
            result: Some(Box::new(Expr::UnitLit)),
        }));
    } else if let syn::Expr::Field(field) = ungroup(&assign.left)
        && !pure_field_base(&field.base)
    {
        // Ordinary Rust assignment evaluates RHS before a computed assignee.
        // Keep simple local/global field chains on their existing fast path.
        let value_name = "#tuple-let-assignment-value";
        out.push(Stmt::Expr(Expr::Block {
            body: vec![
                Stmt::Let(
                    value_name.into(),
                    None,
                    lower_expr_with_types(&assign.right, types),
                ),
                Stmt::FieldAssign {
                    base: lower_expr_with_types(&field.base, types),
                    name: field_name(&field.member),
                    value: Expr::Ident(value_name.into()),
                },
            ],
            result: Some(Box::new(Expr::UnitLit)),
        }));
    } else {
        let value = lower_expr_with_types(&assign.right, types);
        out.push(store(&assign.left, value, types).unwrap_or_else(|| unsupported(false)));
    }
}

pub(super) fn lower_compound(
    assign: &syn::ExprBinary,
    out: &mut Vec<Stmt>,
    types: &mut HashMap<String, String>,
) {
    let Some(op) = assign_op_from_binop(&assign.op) else {
        out.push(unsupported(true));
        return;
    };
    let target = ungroup(&assign.left);
    let stmt = match target {
        syn::Expr::Index(index) => Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INDEX_MODIFY_VALUE_FIRST.into())),
            args: vec![
                lower_expr_with_types(&index.expr, types),
                lower_expr_with_types(&index.index, types),
                lower_expr_with_types(&assign.right, types),
                Expr::StringLit(op.into()),
            ],
        }),
        syn::Expr::Field(field) => Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::FIELD_MODIFY_VALUE_FIRST.into())),
            args: vec![
                lower_expr_with_types(&field.base, types),
                Expr::StringLit(field_name(&field.member)),
                lower_expr_with_types(&assign.right, types),
                Expr::StringLit(op.into()),
            ],
        }),
        syn::Expr::Path(_) => {
            let Some(name) = assign_lhs_name(target) else {
                out.push(unsupported(true));
                return;
            };
            let rhs = "#tuple-let-compound-rhs";
            Stmt::Expr(Expr::Block {
                body: vec![
                    Stmt::Let(
                        rhs.into(),
                        None,
                        lower_expr_with_types(&assign.right, types),
                    ),
                    Stmt::Assign(
                        name.clone(),
                        Expr::Binary {
                            op: op.into(),
                            lhs: Box::new(Expr::Ident(name)),
                            rhs: Box::new(Expr::Ident(rhs.into())),
                        },
                    ),
                ],
                result: Some(Box::new(Expr::UnitLit)),
            })
        }
        _ => unsupported(true),
    };
    out.push(stmt);
}

pub(super) fn expression(expr: &syn::Expr, types: &mut HashMap<String, String>) -> Expr {
    let mut body = Vec::new();
    match expr {
        syn::Expr::Assign(assign) => lower_statement(assign, &mut body, types),
        syn::Expr::Binary(assign) => lower_compound(assign, &mut body, types),
        _ => body.push(unsupported(false)),
    }
    Expr::Block {
        body,
        result: Some(Box::new(Expr::UnitLit)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lower(source: &str) -> Expr {
        expression(
            &syn::parse_str::<syn::Expr>(source).unwrap(),
            &mut HashMap::new(),
        )
    }
    #[test]
    fn tuple_stages_every_leaf_before_sequential_repeated_target_stores() {
        let expr = lower("(a,(b,a),_) = make()");
        let Expr::Block {
            body,
            result: Some(unit),
        } = expr
        else {
            panic!("assignment block")
        };
        assert_eq!(*unit, Expr::UnitLit);
        let [
            Stmt::Expr(Expr::Block {
                body,
                result: Some(unit),
            }),
        ] = body.as_slice()
        else {
            panic!("tuple scope")
        };
        assert_eq!(unit.as_ref(), &Expr::UnitLit);
        assert!(
            matches!(&body[0],Stmt::Let(_,_,Expr::Call{callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="make"))
        );
        let first_store = body
            .iter()
            .position(|stmt| matches!(stmt, Stmt::Assign(..)))
            .unwrap();
        assert_eq!(body[..first_store].iter().filter(|stmt| matches!(stmt,Stmt::Let(name,_,_) if name.starts_with("#tuple-let-assignment-leaf-"))).count(),3);
        let targets = body[first_store..]
            .iter()
            .map(|stmt| match stmt {
                Stmt::Assign(name, _) => name.as_str(),
                _ => panic!("leaf stores"),
            })
            .collect::<Vec<_>>();
        assert_eq!(targets, ["a", "b", "a"]);
    }
    #[test]
    fn assignments_and_compounds_in_value_context_return_unit() {
        for source in [
            "(a)=42",
            "((values[at()]))=next()",
            "(row.x)+=next()",
            "a+=2",
            "_=effect()",
        ] {
            assert!(
                matches!(lower(source),Expr::Block{body,result:Some(unit)} if !body.is_empty()&&*unit==Expr::UnitLit),
                "{source}"
            );
        }
    }
    #[test]
    fn computed_field_stages_rhs_before_base_but_pure_fields_keep_fast_path() {
        let expr = lower("backing()[at()].x = rhs()");
        let Expr::Block { body, .. } = expr else {
            panic!("expression scope")
        };
        let [Stmt::Expr(Expr::Block { body, .. })] = body.as_slice() else {
            panic!("computed place scope")
        };
        assert!(
            matches!(&body[0],Stmt::Let(_,_,Expr::Call{callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="rhs"))
        );
        assert!(matches!(
            &body[1],
            Stmt::FieldAssign {
                base: Expr::Index { .. },
                value: Expr::Ident(_),
                ..
            }
        ));
        let Expr::Block { body, .. } = lower("row.inner.x=42") else {
            panic!("expression scope")
        };
        assert!(matches!(body.as_slice(), [Stmt::FieldAssign { .. }]));
    }
    #[test]
    fn tuple_rest_uses_existing_typed_from_end_projection() {
        let expr = lower("(a,..,b)=(20,0,22)");
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Expr(expr)],
            type_params: vec![],
        }];
        let mut from_end = false;
        crate::core_opt::map_decl_exprs_mut(&mut decls, &mut |expr| {
            from_end |= matches!(expr,Expr::Call{callee,args} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::tuples::LET_FIELD)&&matches!(args.as_slice(),[_,Expr::IntLit(0),Expr::BoolLit(true)]));
        });
        assert!(from_end);
    }
    #[test]
    fn unsupported_targets_and_bad_tuple_shapes_cannot_disappear() {
        for source in [
            "*pointer=effect()",
            "[a,b]=[20,22]",
            "Row{x:a}=value",
            "(a,..,..)=value",
            "*pointer+=effect()",
        ] {
            let expr = lower(source);
            let mut decls = vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Expr(expr)],
                type_params: vec![],
            }];
            let mut refused = false;
            crate::core_opt::map_decl_exprs_mut(&mut decls, &mut |expr| {
                refused |=
                    matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-"));
            });
            assert!(refused, "{source}");
        }
        let parsed =
            parse_rust_source("fn main()->i64{let mut a=0;let mut b=0;(a,b)=42;42}").unwrap();
        let normalized =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(!crate::core_ir_verifier::verify_for_entry(&normalized, "main").ok);
    }
}

#[cfg(test)]
mod refusal_optimization_tests {
    #[test]
    fn unsupported_assignments_remain_refused_after_optimization() {
        for source in [
            "fn main()->i64{let mut value=20;let target=&mut value;*target=42;42}",
            "fn main()->i64{let mut value=20;let target=&mut value;*target+=22;42}",
            "fn main()->i64{let mut a=0;let mut b=0;let ignored=([a,b]=[20,22]);42}",
        ] {
            let parsed = super::super::parse_rust_source(source).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut module = crate::typecheck::normalize_module(
                    crate::parser_registry::ParserId::Rust,
                    &parsed,
                );
                crate::lower_core::desugar_module(&mut module);
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
                assert!(!report.ok, "{source} {profile:?}: {report:?}");
                assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
            }
        }
    }
}
