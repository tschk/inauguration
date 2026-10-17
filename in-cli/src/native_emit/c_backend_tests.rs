//! Unit tests for `c_backend` (split out so emitter stays under 1k).

use super::*;
use crate::core_ir::{
    CatchArm, CoreModuleIdentity, Decl, Expr, LoopKind, MatchArm, Stmt, Typ, UnifiedModule,
};

#[test]
fn emits_fib_style_function() {
    let module = UnifiedModule::with_identity(
        vec![Decl::Function {
            name: "fib".into(),
            params: vec![("n".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![
                Stmt::If {
                    cond: Expr::Binary {
                        op: "le".into(),
                        lhs: Box::new(Expr::Ident("n".into())),
                        rhs: Box::new(Expr::IntLit(1)),
                    },
                    then_body: vec![Stmt::Return(Some(Expr::Ident("n".into())))],
                    else_body: vec![],
                },
                Stmt::Let("a".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Let("b".into(), Some(Typ::Int), Expr::IntLit(1)),
                Stmt::Let("i".into(), Some(Typ::Int), Expr::IntLit(2)),
                Stmt::Loop {
                    kind: LoopKind::While,
                    cond: Some(Expr::Binary {
                        op: "le".into(),
                        lhs: Box::new(Expr::Ident("i".into())),
                        rhs: Box::new(Expr::Ident("n".into())),
                    }),
                    body: vec![
                        Stmt::Let(
                            "t".into(),
                            Some(Typ::Int),
                            Expr::Binary {
                                op: "add".into(),
                                lhs: Box::new(Expr::Ident("a".into())),
                                rhs: Box::new(Expr::Ident("b".into())),
                            },
                        ),
                        Stmt::Assign("a".into(), Expr::Ident("b".into())),
                        Stmt::Assign("b".into(), Expr::Ident("t".into())),
                        Stmt::Assign(
                            "i".into(),
                            Expr::Binary {
                                op: "add".into(),
                                lhs: Box::new(Expr::Ident("i".into())),
                                rhs: Box::new(Expr::IntLit(1)),
                            },
                        ),
                    ],
                },
                Stmt::Return(Some(Expr::Ident("b".into()))),
            ],
            type_params: vec![],
        }],
        CoreModuleIdentity {
            package: Some("fib".into()),
            module: Some("fib.main".into()),
        },
    );
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("int64_t fib(int64_t n)"), "sig missing:\n{c}");
    assert!(c.contains("while"), "loop missing:\n{c}");
    assert!(c.contains("package: fib"), "identity missing:\n{c}");
    assert!(c.contains("__attribute__((used))"), "used attr:\n{c}");
}

#[test]
fn emits_struct_and_field_assign() {
    let module = UnifiedModule::new(vec![
        Decl::Struct {
            name: "Point".into(),
            fields: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
            type_params: vec![],
        },
        Decl::Function {
            name: "bump".into(),
            params: vec![("p".into(), Typ::Named("Point".into()))],
            ret: Typ::Named("Point".into()),
            body: vec![
                Stmt::FieldAssign {
                    base: Expr::Ident("p".into()),
                    name: "x".into(),
                    value: Expr::Binary {
                        op: "add".into(),
                        lhs: Box::new(Expr::Field {
                            base: Box::new(Expr::Ident("p".into())),
                            name: "x".into(),
                        }),
                        rhs: Box::new(Expr::IntLit(1)),
                    },
                },
                Stmt::Return(Some(Expr::Ident("p".into()))),
            ],
            type_params: vec![],
        },
    ]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("struct Point"), "struct:\n{c}");
    assert!(c.contains("(p).x ="), "field assign:\n{c}");
}

#[test]
fn emits_method_call_as_free_fn() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "run".into(),
        params: vec![("c".into(), Typ::Named("Calc".into()))],
        ret: Typ::Int,
        body: vec![Stmt::Return(Some(Expr::Call {
            callee: Box::new(Expr::Field {
                base: Box::new(Expr::Ident("c".into())),
                name: "add".into(),
            }),
            args: vec![Expr::IntLit(3)],
        }))],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("add(c, INT64_C(3))"), "method call:\n{c}");
}

#[test]
fn emits_match_int_arms() {
    use crate::core_ir::MatchArm;
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "classify".into(),
        params: vec![("x".into(), Typ::Int)],
        ret: Typ::Int,
        body: vec![Stmt::Match {
            scrutinee: Expr::Ident("x".into()),
            arms: vec![
                MatchArm {
                    guard: None,
                    pattern: "0".into(),
                    body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                },
                MatchArm {
                    guard: None,
                    pattern: "_".into(),
                    body: vec![Stmt::Return(Some(Expr::IntLit(2)))],
                },
            ],
        }],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("== INT64_C(0)"), "match arm:\n{c}");
    assert!(c.contains("else if"), "else if chain:\n{c}");
}

#[test]
fn emits_try_throw_propagate() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "risky".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![
            Stmt::Try {
                body: vec![Stmt::Throw(Expr::IntLit(9))],
                catches: vec![CatchArm {
                    pattern: "e".into(),
                    body: vec![Stmt::Return(Some(Expr::Ident("e".into())))],
                }],
            },
            Stmt::Propagate,
            Stmt::Return(Some(Expr::IntLit(0))),
        ],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("__in_err"), "err flag:\n{c}");
    assert!(c.contains("/* try */"), "try block:\n{c}");
    assert!(c.contains("return __in_err_val"), "propagate:\n{c}");
}

#[test]
fn emits_for_in_index_walk() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "sum".into(),
        params: vec![("xs".into(), Typ::Named("Vec".into()))],
        ret: Typ::Int,
        body: vec![
            Stmt::Let("acc".into(), Some(Typ::Int), Expr::IntLit(0)),
            Stmt::Loop {
                kind: LoopKind::For {
                    binding: "v".into(),
                },
                cond: Some(Expr::Ident("xs".into())),
                body: vec![Stmt::Assign(
                    "acc".into(),
                    Expr::Binary {
                        op: "add".into(),
                        lhs: Box::new(Expr::Ident("acc".into())),
                        rhs: Box::new(Expr::Ident("v".into())),
                    },
                )],
            },
            Stmt::Return(Some(Expr::Ident("acc".into()))),
        ],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("struct InVec"), "InVec typedef/use:\n{c}");
    assert!(c.contains(".ptr"), "vec.ptr:\n{c}");
    assert!(c.contains(".len"), "vec.len:\n{c}");
    assert!(c.contains("for (int64_t"), "for loop:\n{c}");
    assert!(c.contains("int64_t v;"), "binding local:\n{c}");
}

#[test]
fn hoists_zero_capture_closure() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![
            Stmt::Let(
                "f".into(),
                None,
                Expr::Closure {
                    params: vec![("x".into(), Typ::Int)],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
                    captures: vec![],
                },
            ),
            Stmt::Return(Some(Expr::Ident("f".into()))),
        ],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("static int64_t __closure_"), "hoisted:\n{c}");
    assert!(c.contains("(uint64_t)(__closure_"), "fn ptr:\n{c}");
}

fn write_temp(name: &str, src: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-c-emit-{}-{}-{}",
        std::process::id(),
        unique,
        name
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(name);
    std::fs::write(&path, src).expect("write");
    path
}

#[test]
fn polyglot_in_control_flow_emits_if_while() {
    let path = write_temp(
        "control_flow.in",
        r#"fn helper(value: Int) -> Int { return value; }
fn main() -> Int {
  let value: Int = 1;
  value = value + 2;
  helper(value);
  if value > 2 { value = value - 1; } else { value = 0; }
  while value < 4 { value = value + 1; }
  return value;
}
"#,
    );
    let module = crate::in_lang_parse::parse_in_file(&path).expect("parse .in");
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("int64_t helper(int64_t value)"), "helper:\n{c}");
    assert!(c.contains("while"), "while:\n{c}");
    assert!(c.contains("if ("), "if:\n{c}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn polyglot_c_and_go_answer_main() {
    use crate::parser_registry::ParserId;
    let c_path = write_temp(
        "sample.c",
        "int answer(void) { return 42; }\nint main(void) { return 0; }\n",
    );
    let go_path = write_temp(
        "sample.go",
        "package main\nfunc answer() int { return 42 }\nfunc main() {}\n",
    );
    let c_mod =
        crate::compiler::tree_front::parse_polyglot_file(ParserId::C, &c_path).expect("parse c");
    let go_mod =
        crate::compiler::tree_front::parse_polyglot_file(ParserId::Go, &go_path).expect("parse go");
    let mut c_mod = crate::typecheck::normalize_module(ParserId::C, &c_mod);
    crate::core_opt::map_decl_exprs_mut(&mut c_mod.decls, &mut |expr| {
        *expr = crate::core_opt::fold_expr(expr.clone());
    });
    let c_out = emit_c_module(&c_mod).expect("emit c");
    let go_out = emit_c_module(&go_mod).expect("emit go");
    assert!(c_out.contains("int64_t answer()"), "c answer:\n{c_out}");
    assert!(c_out.contains("return INT64_C(42)"), "c lit:\n{c_out}");
    assert!(go_out.contains("int64_t answer()"), "go answer:\n{go_out}");
    assert!(go_out.contains("return INT64_C(42)"), "go lit:\n{go_out}");
    let _ = std::fs::remove_dir_all(c_path.parent().unwrap());
    let _ = std::fs::remove_dir_all(go_path.parent().unwrap());
}

#[test]
fn polyglot_icore_min_assigns_local() {
    let path = write_temp(
        "min.icore",
        r#"{
  "icoreVersion": 2,
  "decls": [
{"kind": "function", "name": "helper", "params": [], "return": "Int",
 "body": [{"kind": "return", "value": 7}]},
{"kind": "function", "name": "main", "params": [], "return": "Void",
 "body": [
   {"kind": "assign", "target": "value",
    "value": {"kind": "call", "callee": "helper"}},
   {"kind": "return"}
 ]}
  ]
}
"#,
    );
    let module = crate::compiler::icore::parse_icore_file(&path).expect("parse icore");
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("int64_t value;"), "local:\n{c}");
    assert!(c.contains("value = helper()"), "assign:\n{c}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn emits_multi_catch_chain() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "risky".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![Stmt::Try {
            body: vec![Stmt::Throw(Expr::IntLit(1))],
            catches: vec![
                CatchArm {
                    pattern: "0".into(),
                    body: vec![Stmt::Return(Some(Expr::IntLit(10)))],
                },
                CatchArm {
                    pattern: "e".into(),
                    body: vec![Stmt::Return(Some(Expr::Ident("e".into())))],
                },
            ],
        }],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("else if"), "multi-catch:\n{c}");
    assert!(c.contains("== INT64_C(0)"), "int catch:\n{c}");
}

#[test]
fn hoists_capturing_closure_env() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![
            Stmt::Let("y".into(), Some(Typ::Int), Expr::IntLit(7)),
            Stmt::Let(
                "f".into(),
                None,
                Expr::Closure {
                    params: vec![("x".into(), Typ::Int)],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::Binary {
                        op: "add".into(),
                        lhs: Box::new(Expr::Ident("x".into())),
                        rhs: Box::new(Expr::Ident("y".into())),
                    }))],
                    captures: vec!["y".into()],
                },
            ),
            Stmt::Return(Some(Expr::Ident("f".into()))),
        ],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(c.contains("_env"), "env struct:\n{c}");
    assert!(c.contains("static struct"), "static env:\n{c}");
    assert!(
        c.contains(".y = y") || c.contains("_env.y = y"),
        "env init:\n{c}"
    );
    assert!(c.contains("(uint64_t)("), "fn ptr value:\n{c}");
}

#[test]
fn skips_invec_typedef_when_unused() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "answer".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
        type_params: vec![],
    }]);
    let c = emit_c_module(&module).expect("emit");
    assert!(!c.contains("typedef struct InVec"), "no invec:\n{c}");
}

#[test]
fn rejects_extreme_nesting() {
    let mut body = Stmt::Return(Some(Expr::IntLit(1)));
    for _ in 0..300 {
        body = Stmt::If {
            cond: Expr::BoolLit(true),
            then_body: vec![body],
            else_body: vec![],
        };
    }
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "deep".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![body],
        type_params: vec![],
    }]);
    let err = emit_c_module(&module).expect_err("depth guard");
    assert!(err.contains("nesting"), "err={err}");
}

#[test]
fn unresolved_complement_is_refused_instead_of_emitting_identity() {
    let mut module =
        crate::compiler::rust_front::parse_rust_source("fn main() -> f64 { return !1.25; }")
            .unwrap();
    crate::lower_core::desugar_module(&mut module);
    let error = emit_c_module(&module).unwrap_err();
    assert!(
        error.contains("complement requires a resolved Int or Bool operand"),
        "{error}"
    );
}

#[test]
fn typed_complements_survive_c_emission_and_execution() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::compiler::rust_front::parse_rust_source("fn inverse(value: i64) -> i64 { return !value; } fn predicate(value: bool) -> bool { return !value; } fn answer() -> i64 { if predicate(false) { return inverse(1) + 44; } else { return 0; } }").unwrap();
    crate::lower_core::desugar_module(&mut module);
    let source = format!(
        "{}\nint main(void) {{ return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-complement-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("complement.c");
    let binary = dir.join("complement");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn guarded_match_lowering_has_linear_emitted_size() {
    fn module(count: usize) -> UnifiedModule {
        let mut arms: Vec<MatchArm> = (0..count)
            .map(|index| MatchArm {
                pattern: index.to_string(),
                guard: Some(Expr::BoolLit(false)),
                body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
            })
            .collect();
        arms.push(MatchArm {
            pattern: "_".into(),
            guard: None,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
        });
        UnifiedModule::new(vec![Decl::Function {
            name: "answer".into(),
            params: vec![("value".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![Stmt::Match {
                scrutinee: Expr::Ident("value".into()),
                arms,
            }],
            type_params: vec![],
        }])
    }
    let small = emit_c_module(&module(100)).unwrap().len();
    let large = emit_c_module(&module(1000)).unwrap().len();
    assert!(large > small);
    assert!(
        large < small * 12,
        "100 arms: {small} bytes, 1000 arms: {large} bytes"
    );
}

#[test]
fn guarded_match_effects_survive_c_emission_and_execution() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source(include_str!(
        "../../../conformance/control-flow/match-guard-effects.in"
    ))
    .unwrap();
    for decl in &mut module.decls {
        if let Decl::Function { name, .. } = decl
            && name == "main"
        {
            *name = "answer".into();
        }
    }
    let source = format!(
        "{}\nint main(void) {{ return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-guard-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("guard.c");
    let binary = dir.join("guard");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn saturating_float_projection_clamps_and_evaluates_once() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next() -> Float { calls += 1; return 41.9; }\nfn main() -> Int { return 0; }").unwrap();
    if let Decl::Function { name, body, .. } = &mut module.decls[2] {
        *name = "answer".into();
        *body = vec![
            Stmt::Let(
                "result".into(),
                Some(Typ::Int),
                Expr::Unary {
                    op: "float-saturate-u8".into(),
                    expr: Box::new(Expr::Call {
                        callee: Box::new(Expr::Ident("next".into())),
                        args: vec![],
                    }),
                },
            ),
            Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident("result".into())),
                rhs: Box::new(Expr::Ident("calls".into())),
            })),
        ];
    }
    for target in ["i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64"] {
        let source =
            format!("fn cast_{target}(value: f64) -> i64 {{ return (value as {target}) as i64; }}");
        module.decls.extend(
            crate::compiler::rust_front::parse_rust_source(&source)
                .unwrap()
                .decls,
        );
    }
    let code = format!(
        "{}\nint main(void) {{ if (cast_i8(1000.0) != 127 || cast_i8(-1000.0) != -128 || cast_u8(-1.0) != 0 || cast_u8(1000.0) != 255 || cast_i16(1e100) != 32767 || cast_u16(1e100) != 65535 || cast_i32(1e100) != INT32_MAX || cast_u32(1e100) != UINT32_MAX || cast_i64(1e100) != INT64_MAX || cast_i64(-1e100) != INT64_MIN || cast_u64(1e100) != -1 || cast_u64(9223372036854775808.0) != INT64_MIN || cast_i64(0.0 / 0.0) != 0) return 99; return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let dir = std::env::temp_dir().join(format!(
        "inauguration-saturating-c-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("cast.c");
    let binary = dir.join("cast");
    std::fs::write(&path, code).unwrap();
    let build = Command::new("clang")
        .arg("-std=c11")
        .arg(&path)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn integer_cast_projection_executes_operand_once() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next() -> Int { calls += 1; return 297; }\nfn main() -> Int { return next() + calls; }").unwrap();
    let Decl::Function { name, body, .. } = &mut module.decls[2] else {
        panic!("function");
    };
    *name = "answer".into();
    let Stmt::Return(Some(Expr::Binary { lhs, .. })) = &mut body[0] else {
        panic!("return");
    };
    **lhs = Expr::Unary {
        op: "int-cast-u8".into(),
        expr: lhs.clone(),
    };
    for target in [
        "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "isize", "usize",
    ] {
        let source =
            format!("fn cast_{target}(value: i64) -> i64 {{ return (value as {target}) as i64; }}");
        module.decls.extend(
            crate::compiler::rust_front::parse_rust_source(&source)
                .unwrap()
                .decls,
        );
    }
    module.decls.extend(
        crate::compiler::rust_front::parse_rust_source(
            "fn cast_bool(value: bool) -> i64 { return (value as u8) as i64; }",
        )
        .unwrap()
        .decls,
    );
    let source = format!(
        "{}\nint main(void) {{ if (cast_i8(170) != -86 || cast_i8(-129) != 127 || cast_u8(-1) != 255 || cast_i16(65535) != -1 || cast_u16(-1) != 65535 || cast_i32(2147483648LL) != -2147483648LL || cast_u32(-1) != 4294967295LL || cast_i64(INT64_MIN) != INT64_MIN || cast_u64(INT64_MIN) != INT64_MIN || cast_isize(-1) != -1 || cast_usize(-1) != -1 || cast_bool(1) != 1 || cast_bool(0) != 0) return 99; return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-integer-cast-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("cast.c");
    let binary = dir.join("cast");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn floating_width_projection_rounds_and_evaluates_once() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next() -> Float { calls += 1; return 16777217.0; }\nfn main() -> Int { let value: Float = next(); if value == 16777216.0 && calls == 1 { return 42; } return 0; }").unwrap();
    let Decl::Function { name, body, .. } = &mut module.decls[2] else {
        panic!("function");
    };
    *name = "answer".into();
    let Stmt::Let(_, _, value) = &mut body[0] else {
        panic!("let");
    };
    *value = Expr::Unary {
        op: "float-cast-f32".into(),
        expr: Box::new(value.clone()),
    };
    let source = format!(
        "{}\nint main(void) {{ return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-float-width-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("cast.c");
    let binary = dir.join("cast");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn signed_right_shift_projection_preserves_boundary_signs() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source(
        "fn main(value: Int, count: Int) -> Int { return value >> count; }",
    )
    .unwrap();
    let Decl::Function { name, .. } = &mut module.decls[0] else {
        panic!("function");
    };
    *name = "shift".into();
    let source = format!(
        "{}\nint main(void) {{ if (shift(-1, 1) != -1 || shift(INT64_MIN, 63) != -1 || shift(INT64_MAX, 63) != 0) return 99; return (int)-shift(-84, 1); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-signed-shift-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("shift.c");
    let binary = dir.join("shift");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn in_index_compound_projection_preserves_place_and_read_order() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let mut module = crate::in_lang_parse::parse_in_source(include_str!(
        "../../../conformance/control-flow/in-index-compound-effects.in"
    ))
    .unwrap();
    for decl in &mut module.decls {
        if let Decl::Function { name, .. } = decl
            && name == "main"
        {
            *name = "answer".into();
        }
    }
    let source = format!(
        "{}\nint main(void) {{ return (int)answer(); }}\n",
        emit_c_module(&module).unwrap()
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-place-first-c-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let c = dir.join("place.c");
    let binary = dir.join("place");
    std::fs::write(&c, source).unwrap();
    let build = Command::new("clang")
        .arg(&c)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(status.code(), Some(42));
}

fn assert_inferred_c_executes(module: &UnifiedModule, entry_body: &str) {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    let original = module.clone();
    let emitted = emit_c_module(module).unwrap();
    assert_eq!(module, &original, "C emission mutated its input");
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Wall-clock resolution can repeat across concurrently running tests.
    let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "inauguration-c-local-types-{}-{stamp}-{serial}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("locals.c");
    let binary = dir.join("locals");
    std::fs::write(
        &source,
        format!("{emitted}\n#include <string.h>\nint main(void){{{entry_body}}}\n"),
    )
    .unwrap();
    let build = Command::new("clang")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}: {}",
        source.display(),
        String::from_utf8_lossy(&build.stderr)
    );
    let status = Command::new(&binary).status().unwrap();
    assert_eq!(status.code(), Some(42), "{}", source.display());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inferred_c_locals_preserve_values_aliases_and_lexical_types() {
    for source in [
        "fn main()->Int{let ratio=1.5;if ratio==1.5{return 42;}return 0;}",
        "var original:Float=1.5;\nfn amount()->Float{return original;}\nfn main()->Int{let first=amount();let second=first;original=2.5;if second==1.5{return 42;}return 0;}",
        "fn main()->Int{let value=1.5;if true{let value=0;if value!=0{return 0;}}if value==1.5{return 42;}return 0;}",
        "var value:Float=1.5;\nfn echo(value:Float)->Int{if true{let value:Int=0;}let copy=value;if copy==1.5{return 42;}return 0;}\nfn main()->Int{return echo(value);}",
        "struct Pair{Int x;Float weight}\nfn main()->Int{let pair=Pair{x:42,weight:1.5};let copy=pair;pair.x=0;if copy.weight==1.5{return copy.x;}return 0;}",
        "struct Pair{Int x;Int y}\nfn main()->Int{let pair=Pair{x:20,y:22};match pair{whole{pair.x=0;return whole.x+whole.y;}}return 0;}",
        "fn array_len(values:[Int])->Int{return 42;}\nfn main()->Int{let values=[1,2];return array_len(values);}",
        "fn main()->Int{let values=[1.5,2.5];let copy=values[0];if copy==1.5 && values[1]==2.5{return 42;}return 0;}",
        "fn main()->Int{let ready=true;match ready{item{let copy=item;if copy{return 42;}}}return 0;}",
    ] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        assert_inferred_c_executes(&module, "return (int)answer();");
    }
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "answer".into(),
        params: vec![],
        ret: Typ::Int,
        type_params: vec![],
        body: vec![
            Stmt::Assign(
                "ratio".into(),
                Expr::FloatLit(crate::core_ir::FloatVal(1.5)),
            ),
            Stmt::If {
                cond: Expr::Binary {
                    op: "==".into(),
                    lhs: Box::new(Expr::Ident("ratio".into())),
                    rhs: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(1.5))),
                },
                then_body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                else_body: vec![],
            },
            Stmt::Return(Some(Expr::IntLit(0))),
        ],
    }]);
    assert_inferred_c_executes(&module, "return (int)answer();");
}

#[test]
fn inferred_c_closure_locals_use_typed_parameters() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "answer".into(),
        params: vec![],
        ret: Typ::Int,
        type_params: vec![],
        body: vec![Stmt::Return(Some(Expr::Closure {
            params: vec![("value".into(), Typ::Float)],
            ret: Typ::Float,
            captures: vec![],
            body: vec![
                Stmt::Let("copy".into(), None, Expr::Ident("value".into())),
                Stmt::Return(Some(Expr::Ident("copy".into()))),
            ],
        }))],
    }]);
    assert_inferred_c_executes(
        &module,
        "return ((double(*)(double))(uintptr_t)answer())(1.5)==1.5 ? 42 : 0;",
    );
}

#[test]
fn inferred_c_capture_fields_and_aliases_preserve_snapshot_types() {
    let pair = |x| Expr::StructInit {
        name: "Pair".into(),
        fields: vec![
            ("x".into(), Expr::IntLit(x)),
            (
                "weight".into(),
                Expr::FloatLit(crate::core_ir::FloatVal(1.5)),
            ),
        ],
    };
    for (typ, initial, replacement, entry) in [
        (
            Typ::Int,
            Expr::IntLit(42),
            Expr::IntLit(0),
            "return (int)((int64_t(*)(void))(uintptr_t)answer())();",
        ),
        (
            Typ::Float,
            Expr::FloatLit(crate::core_ir::FloatVal(1.5)),
            Expr::FloatLit(crate::core_ir::FloatVal(2.5)),
            "return ((double(*)(void))(uintptr_t)answer())()==1.5 ? 42 : 0;",
        ),
        (
            Typ::String,
            Expr::StringLit("λ雪".into()),
            Expr::StringLit("changed".into()),
            "return strcmp(((const char*(*)(void))(uintptr_t)answer())(),\"λ雪\")==0 ? 42 : 0;",
        ),
        (
            Typ::Bool,
            Expr::BoolLit(true),
            Expr::BoolLit(false),
            "return ((uint8_t(*)(void))(uintptr_t)answer())() ? 42 : 0;",
        ),
        (
            Typ::Named("Pair".into()),
            pair(42),
            pair(0),
            "struct Pair value=((struct Pair(*)(void))(uintptr_t)answer())();return value.x==42 && value.weight==1.5 ? 42 : 0;",
        ),
    ] {
        let module = UnifiedModule::new(vec![
            Decl::Struct {
                name: "Pair".into(),
                fields: vec![("x".into(), Typ::Int), ("weight".into(), Typ::Float)],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Let("value".into(), None, initial),
                    Stmt::Let(
                        "f".into(),
                        None,
                        Expr::Closure {
                            params: vec![],
                            ret: typ,
                            captures: vec!["value".into()],
                            body: vec![
                                Stmt::Let("copy".into(), None, Expr::Ident("value".into())),
                                Stmt::Return(Some(Expr::Ident("copy".into()))),
                            ],
                        },
                    ),
                    Stmt::Assign("value".into(), replacement),
                    Stmt::Return(Some(Expr::Ident("f".into()))),
                ],
            },
        ]);
        assert_inferred_c_executes(&module, entry);
    }
}

#[test]
fn c_local_inference_refuses_unknown_values_and_missing_indirect_signatures() {
    for (source, reason) in [
        (
            "fn main()->Int{let value=unknown_target();return value;}",
            "cannot infer local",
        ),
        (
            "fn main()->Int{let f=fn()->Int{return 42;};return f();}",
            "requires a function signature",
        ),
        (
            "fn main()->Int{let answer=0;for item in [42]{answer+=item;}return answer;}",
            "length-bearing iterable",
        ),
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        let error = emit_c_module(&module).unwrap_err();
        assert!(error.contains(reason), "{source}: {error}");
    }
}

#[test]
fn inferred_c_hardware_intrinsic_alias_retains_integer_type() {
    let module =
        crate::in_lang_parse::parse_in_source("fn main()->Int{let value=read_cr2();return value;}")
            .unwrap();
    let source = emit_c_module(&module).unwrap();
    assert!(source.contains("int64_t value;"), "{source}");
    assert!(source.contains("mov %%cr2"), "{source}");
}

#[test]
fn c_string_comparisons_use_distinct_writable_buffers() {
    let source = "fn equal(a:String,b:String)->Bool{return a==b;}\nfn different(a:String,b:String)->Bool{return a!=b;}\nfn main()->Int{return 0;}";
    let canonical = crate::in_canonical::canonicalize_in_source(source).unwrap();
    for source in [source, canonical.as_str()] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        assert_inferred_c_executes(
            &module,
            r#"
            char left[] = {'h',(char)0xc3,(char)0xa9,0};
            char right[] = {'h',(char)0xc3,(char)0xa9,0};
            char empty_left[] = {0}; char empty_right[] = {0};
            if (!equal(left,right) || different(left,right)) return 1;
            if (!equal(empty_left,empty_right) || different(empty_left,empty_right)) return 2;
            right[0]='x';
            if (equal(left,right) || !different(left,right)) return 3;
            if (equal(left,empty_left) || !different(left,empty_right)) return 4;
            return 42;
        "#,
        );
    }
}

#[test]
fn c_string_comparison_aliases_and_numeric_comparisons_execute() {
    for (op, equal_result, different_result) in [("eq", true, false), ("neq", false, true)] {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "compare".into(),
            params: vec![("left".into(), Typ::String), ("right".into(), Typ::String)],
            ret: Typ::Bool,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::Ident("left".into())),
                rhs: Box::new(Expr::Ident("right".into())),
            }))],
        }]);
        assert_inferred_c_executes(
            &module,
            &format!(
                "char a[]={{65,0}};char b[]={{65,0}};char c[]={{66,0}};if(compare(a,b)!={})return 1;if(compare(a,c)!={})return 2;return 42;",
                u8::from(equal_result),
                u8::from(different_result)
            ),
        );
    }
    let mut module = crate::in_lang_parse::parse_in_source(
        "fn main()->Int{let a=20;let b=22;if a!=b && a+b==42{return 42;}return 0;}",
    )
    .unwrap();
    if let Decl::Function { name, .. } = &mut module.decls[0] {
        *name = "answer".into();
    }
    assert_inferred_c_executes(&module, "return (int)answer();");
}

#[test]
fn c_string_comparisons_preserve_effect_order_and_lazy_contexts() {
    let source = "var calls:Int=0;\nfn left()->String{calls=calls*10+1;return \"hé\";}\nfn right()->String{calls=calls*10+2;return \"hé\";}\nfn main()->Int{if false && left()==right(){return 1;}if true || left()!=right(){}else{return 2;}if calls!=0{return 3;}if left()==right(){if calls==12{return 42;}}return 4;}";
    let canonical = crate::in_canonical::canonicalize_in_source(source).unwrap();
    for source in [source, canonical.as_str()] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
                let comparison = Expr::Binary {
                    op: "==".into(),
                    lhs: Box::new(Expr::Call {
                        callee: Box::new(Expr::Ident("left".into())),
                        args: vec![],
                    }),
                    rhs: Box::new(Expr::Call {
                        callee: Box::new(Expr::Ident("right".into())),
                        args: vec![],
                    }),
                };
                // Both unselected conditional arms must keep the comparison lazy.
                for (cond, then_expr, else_expr) in [
                    (false, comparison.clone(), Expr::BoolLit(true)),
                    (true, Expr::BoolLit(true), comparison),
                ] {
                    body.insert(
                        0,
                        Stmt::Expr(Expr::Conditional {
                            cond: Box::new(Expr::BoolLit(cond)),
                            then_expr: Box::new(then_expr),
                            else_expr: Box::new(else_expr),
                        }),
                    );
                }
            }
        }
        assert_inferred_c_executes(&module, "return (int)answer();");
    }
}

#[test]
fn c_string_literal_matches_decode_escapes_and_avoid_temporary_collisions() {
    // Emit compare first so an unreserved fresh counter deterministically collides at0/1/2.
    let source = r#"fn compare(__str_left_0:String,__str_right_1:String)->Int{let __str_equal_2:Int=20;let __scr_0:Int=22;if __str_left_0==__str_right_1{return __str_equal_2+__scr_0;}return 0;}
fn matched(text:String)->Int{match text{"a\"é\n{" {return 42;} _ {return 0;}}return 0;}
fn main()->Int{return 0;}"#;
    let canonical = crate::in_canonical::canonicalize_in_source(source).unwrap();
    for source in [source, canonical.as_str()] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        assert_inferred_c_executes(
            &module,
            r#"
            char text[]={'a','"',(char)0xc3,(char)0xa9,'\n','{',0};
            char other[]={'a','"',(char)0xc3,(char)0xa9,'\n','{',0};
            if(matched(text)!=42 || compare(text,other)!=42)return 1;
            text[0]='x';if(matched(text)!=0)return 2;
            return 42;
        "#,
        );
    }
}

#[test]
fn c_string_literal_global_comparisons_are_valid_file_scope_constants() {
    for op in ["==", "eq", "!=", "neq"] {
        let inequality = matches!(op, "!=" | "neq");
        let decls = [
            ("unicode", "hé", "hé", true),
            ("unequal", "a", "b", false),
            ("empty", "", "", true),
        ]
        .into_iter()
        .map(|(name, left, right, _)| Decl::Global {
            name: name.into(),
            typ: Typ::Bool,
            mutable: true,
            constant_value: false,
            init: Some(Box::new(Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::StringLit(left.into())),
                rhs: Box::new(Expr::StringLit(right.into())),
            })),
        })
        .collect();
        let module = UnifiedModule::new(decls);
        let equal = u8::from(!inequality);
        let unequal = u8::from(inequality);
        assert_inferred_c_executes(
            &module,
            &format!(
                "if(unicode!={equal} || unequal!={unequal} || empty!={equal})return 1;return 42;"
            ),
        );
    }
    let nested = UnifiedModule::new(vec![Decl::Global {
        name: "nested".into(),
        typ: Typ::Bool,
        mutable: true,
        constant_value: false,
        init: Some(Box::new(Expr::Binary {
            op: "==".into(),
            lhs: Box::new(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::StringLit("λ".into())),
                rhs: Box::new(Expr::StringLit("雪".into())),
            }),
            rhs: Box::new(Expr::StringLit("λ雪".into())),
        })),
    }]);
    assert_inferred_c_executes(&nested, "return nested ? 42 : 0;");
    let module = UnifiedModule::new(vec![
        Decl::Function {
            name: "text".into(),
            params: vec![],
            ret: Typ::String,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::StringLit("a".into())))],
        },
        Decl::Global {
            name: "dynamic".into(),
            typ: Typ::Bool,
            mutable: true,
            constant_value: false,
            init: Some(Box::new(Expr::Binary {
                op: "==".into(),
                lhs: Box::new(Expr::Call {
                    callee: Box::new(Expr::Ident("text".into())),
                    args: vec![],
                }),
                rhs: Box::new(Expr::StringLit("a".into())),
            })),
        },
    ]);
    let error = emit_c_module(&module).unwrap_err();
    assert!(error.contains("global initializer"), "{error}");
}

#[test]
fn raw_native_invoke_c_uses_nonthrowing_integer_abi_and_ordered_operands() {
    for source in [
        "fn target()->Int{return 42;}\nfn main()->Int{return invoke-native(target);}",
        "fn target(value:Int)->Int{return value;}\nfn main()->Int{return invoke_native1(target,42);}",
        "var calls:Int=0;\nfn target(a:Int,b:Int)->Int{if calls==123{return a+b;}return 0;}\nfn pointer()->Int{calls=calls*10+1;return target;}\nfn first()->Int{calls=calls*10+2;return 20;}\nfn second()->Int{calls=calls*10+3;return 22;}\nfn main()->Int{return invoke-native2(pointer(),first(),second());}",
        "fn invoke-native(value:Bool)->Int{if value{return 42;}return 0;}\nfn main()->Int{return invoke-native(true);}",
    ] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        assert_inferred_c_executes(&module, "return (int)answer();");
    }
    let module = crate::in_lang_parse::parse_in_source(
        "fn target()->Int{return 42;}\nfn main()->Int{return invoke_native(target);}",
    )
    .unwrap();
    let c = emit_c_module(&module).unwrap();
    assert!(c.contains("int64_t(*)(void)"), "{c}");
    assert!(c.contains("__builtin_trap()"), "{c}");
}

#[test]
fn raw_native_invoke_c_refuses_wrong_types_signatures_and_throwing_targets() {
    for source in [
        "fn main()->Int{return invoke-native1(1,true);}",
        "fn target(value:Bool)->Int{return 42;}\nfn main()->Int{return invoke-native1(target,1);}",
        "fn target()->Bool{return true;}\nfn main()->Int{return invoke-native(target);}",
        "fn target()->Int{throw 9;return 42;}\nfn main()->Int{return invoke-native(target);}",
        "fn main()->Int{return invoke-native2(1,2);}",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        assert!(emit_c_module(&module).is_err(), "{source}");
    }
}

#[test]
fn raw_native_invoke_c_refuses_nonconstant_global_initializers() {
    for (name, args) in [
        ("invoke-native", vec![Expr::IntLit(42)]),
        ("invoke_native1", vec![Expr::IntLit(42), Expr::IntLit(1)]),
        (
            "invoke-native2",
            vec![Expr::IntLit(42), Expr::IntLit(1), Expr::IntLit(2)],
        ),
    ] {
        let module = UnifiedModule::new(vec![Decl::Global {
            name: "value".into(),
            typ: Typ::Int,
            mutable: true,
            constant_value: false,
            init: Some(Box::new(Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args,
            })),
        }]);
        let error = emit_c_module(&module).unwrap_err();
        assert!(error.contains("global initializer"), "{name}: {error}");
    }
}

#[test]
fn unsigned_c_projection_executes_high_bits_and_orders_operands_once() {
    let binary = |op: &str, left: Expr, right: Expr| Expr::Binary {
        op: op.into(),
        lhs: Box::new(left),
        rhs: Box::new(right),
    };
    let call = |name: &str| Expr::Call {
        callee: Box::new(Expr::Ident(name.into())),
        args: vec![],
    };
    let mut decls = vec![Decl::Global {
        name: "trace".into(),
        typ: Typ::Int,
        init: Some(Box::new(Expr::IntLit(0))),
        mutable: true,
        constant_value: false,
    }];
    for (name, digit, value) in [("left", 1, -1), ("right", 2, 2)] {
        decls.push(Decl::Function {
            name: name.into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Assign(
                    "trace".into(),
                    binary(
                        "+",
                        binary("*", Expr::Ident("trace".into()), Expr::IntLit(10)),
                        Expr::IntLit(digit),
                    ),
                ),
                Stmt::Return(Some(Expr::IntLit(value))),
            ],
        });
    }
    for op in ["udiv", "urem", "lshr"] {
        decls.push(Decl::Function {
            name: format!("check_{op}"),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(binary(op, call("left"), call("right"))))],
        });
    }
    decls.push(Decl::Function {
        name: "shift_count".into(),
        params: vec![("count".into(), Typ::Int)],
        ret: Typ::Int,
        type_params: vec![],
        body: vec![Stmt::Return(Some(binary(
            "lshr",
            Expr::IntLit(-1),
            Expr::Ident("count".into()),
        )))],
    });
    assert_inferred_c_executes(
        &UnifiedModule::new(decls),
        "
        trace=0; if(check_udiv()!=INT64_MAX || trace!=12)return 1;
        trace=0; if(check_urem()!=1 || trace!=12)return 2;
        trace=0; if(check_lshr()!=INT64_C(4611686018427387903) || trace!=12)return 3;
        if(shift_count(64)!=-1 || shift_count(65)!=INT64_MAX)return 4;
        return 42;
    ",
    );
}

#[test]
fn unsigned_global_initializers_require_folding_before_c_projection() {
    for (op, expected) in [
        ("udiv", i64::MAX),
        ("urem", 1),
        ("lshr", i64::MAX),
        ("<<", -4),
        (">>", -1),
    ] {
        let global = |name: &str, init: Expr| Decl::Global {
            name: name.into(),
            typ: Typ::Int,
            init: Some(Box::new(init)),
            mutable: true,
            constant_value: false,
        };
        let operation = |right| Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::IntLit(-1)),
            rhs: Box::new(right),
        };
        let module = UnifiedModule::new(vec![
            global("count", Expr::IntLit(2)),
            global("result", operation(Expr::Ident("count".into()))),
        ]);
        let error =
            emit_c_module(&module).expect_err("file-scope statement expression must refuse");
        assert!(
            error.contains("unsigned operation in a global initializer requires constant folding"),
            "{op}: {error}"
        );
        let count = if op == "lshr" { 1 } else { 2 };
        let module = UnifiedModule::new(vec![global("result", operation(Expr::IntLit(count)))]);
        assert_inferred_c_executes(
            &module,
            &format!("if(result != INT64_C({expected})) return 7; return 42;"),
        );
    }
}

#[test]
fn checked_unsigned_c_failure_exits_current_function_after_ordered_operands() {
    let binary = |op: &str, lhs, rhs| Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    };
    let call = |name: &str| Expr::Call {
        callee: Box::new(Expr::Ident(name.into())),
        args: vec![],
    };
    for op in ["udiv", "urem"] {
        let mut decls = vec![];
        for name in ["trace", "after"] {
            decls.push(Decl::Global {
                name: name.into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::IntLit(0))),
                mutable: true,
                constant_value: false,
            });
        }
        for (name, digit, value) in [("left", 1, 42), ("zero", 2, 0)] {
            decls.push(Decl::Function {
                name: name.into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Assign(
                        "trace".into(),
                        binary(
                            "+",
                            binary("*", Expr::Ident("trace".into()), Expr::IntLit(10)),
                            Expr::IntLit(digit),
                        ),
                    ),
                    Stmt::Return(Some(Expr::IntLit(value))),
                ],
            });
        }
        let operation = || binary(op, call("left"), call("zero"));
        let mut failing_body = vec![
            Stmt::Expr(operation()),
            Stmt::Assign("after".into(), Expr::IntLit(1)),
        ];
        for (name, ret, result) in [
            ("fail_word", Typ::Int, Some(Expr::IntLit(7))),
            ("fail_void", Typ::Void, None),
            ("fail_bool", Typ::Bool, Some(Expr::BoolLit(false))),
        ] {
            let mut body = failing_body.clone();
            body.push(Stmt::Return(result));
            decls.push(Decl::Function {
                name: name.into(),
                params: vec![],
                ret,
                type_params: vec![],
                body,
            });
        }
        decls.push(Decl::Function {
            name: "caller".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(binary(
                "+",
                call("fail_word"),
                Expr::IntLit(41),
            )))],
        });
        failing_body.push(Stmt::Return(Some(Expr::IntLit(7))));
        decls.push(Decl::Function {
            name: "make_closure".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Closure {
                params: vec![],
                ret: Typ::Int,
                body: failing_body,
                captures: vec![],
            }))],
        });
        assert_inferred_c_executes(
            &UnifiedModule::new(decls),
            "
            if(caller()!=42 || trace!=12 || after!=0)return 2;
            trace=0; fail_void(); if(trace!=12 || after!=0)return 3;
            trace=0; if(!fail_bool() || trace!=12 || after!=0)return 4;
            trace=0; int64_t (*closure)(void)=(int64_t (*)(void))(uintptr_t)make_closure();
            if(closure()!=1 || trace!=12 || after!=0)return 5;
            return 42;
        ",
        );
    }
}

#[test]
fn checked_unsigned_c_operations_refuse_unsupported_failure_return_abis() {
    for op in ["udiv", "urem"] {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "unsupported".into(),
            params: vec![],
            ret: Typ::String,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(42)),
                    rhs: Box::new(Expr::IntLit(0)),
                }),
                Stmt::Return(Some(Expr::StringLit("text".into()))),
            ],
        }]);
        let error = emit_c_module(&module).expect_err("unsupported failure ABI");
        assert!(
            error.contains("checked unsigned division requires"),
            "{op}: {error}"
        );
    }
}

#[test]
fn private_print_cannot_be_sanitized_into_a_source_override() {
    let module = crate::in_lang_parse::parse_in_source(
        "fn _print_int(value:Int)->Int{return 42;}fn main()->Int{#print_int(7);return 0;}",
    )
    .unwrap();
    let error = emit_c_module(&module).unwrap_err();
    assert!(
        error.contains("private print requires a hosted runtime"),
        "{error}"
    );
}

#[test]
fn word_shift_projection_is_sanitizer_clean_and_sequences_operands() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let source = "var trace:Int=0; fn left()->Int{trace=trace*10+1;return -84;} fn right()->Int{trace=trace*10+2;return 65;} fn shift(value:Int,count:Int)->Int{return value<<count;} fn effects()->Int{return left()<<right();} fn skipped()->Int{if false&&((left()<<right())==0){return 7;}return trace;} fn main()->Int{return 42;}";
    for op in ["<<", ">>", "lshr", "shl", "shr"] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Binary { op: current, .. } = expr
                && current == "<<"
            {
                *current = op.into();
            }
        });
        let before = module.clone();
        let emitted = emit_c_module(&module).unwrap();
        assert_eq!(module, before);
        let expected = |value: i64, count: i64| match op {
            "<<" | "shl" => value.wrapping_shl(count as u32),
            "lshr" => (value as u64).wrapping_shr(count as u32) as i64,
            _ => value.wrapping_shr(count as u32),
        };
        let literal = |value: i64| {
            if value == i64::MIN {
                "INT64_MIN".to_owned()
            } else {
                format!("INT64_C({value})")
            }
        };
        let mut checks = String::new();
        for value in [0i64, 1, -84, i64::MIN, i64::MAX] {
            for count in [0i64, 1, 63, 64, 65, -1] {
                checks.push_str(&format!(
                    "{{volatile int64_t value={};volatile int64_t count={};if(shift(value,count)!={})return 7;}}\n",
                    literal(value), literal(count), literal(expected(value, count))));
            }
        }
        checks.push_str(&format!(
            "trace=0;if(effects()!={}||trace!=12)return 8;trace=0;if(skipped()!=0||trace!=0)return 9;return 42;",
            literal(expected(-84, 65))));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "inauguration-c-shifts-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("shifts.c");
        let binary = directory.join("shifts");
        std::fs::write(&path, format!("{emitted}\nint main(void){{{checks}}}\n")).unwrap();
        let build = Command::new("clang")
            .args([
                "-std=gnu11",
                "-O2",
                "-fsanitize=undefined",
                "-fno-sanitize-recover=undefined",
            ])
            .arg(&path)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{op}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let result = Command::new(&binary).output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(42),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.stderr.is_empty(),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn out_of_range_global_word_shifts_refuse_before_c_emission() {
    for op in ["<<", ">>", "lshr"] {
        for count in [-1, 64] {
            let module = UnifiedModule::new(vec![Decl::Global {
                name: "result".into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(1)),
                    rhs: Box::new(Expr::IntLit(count)),
                })),
                mutable: true,
                constant_value: false,
            }]);
            let error = emit_c_module(&module).unwrap_err();
            assert!(
                error.contains("global initializer requires constant folding"),
                "{error}"
            );
        }
    }
}

#[test]
fn word_shift_projection_requires_integer_operands() {
    for op in ["<<", ">>", "lshr", "shl", "shr"] {
        for invalid in [
            Typ::Float,
            Typ::Bool,
            Typ::String,
            Typ::Named("Unknown".into()),
        ] {
            for side in [0usize, 1] {
                let mut module = crate::in_lang_parse::parse_in_source(
                    "fn shift(left:Int,right:Int)->Int{return left<<right;}fn main()->Int{return shift(21,1);}").unwrap();
                if let Decl::Function { params, body, .. } = &mut module.decls[0] {
                    params[side].1 = invalid.clone();
                    if let Stmt::Return(Some(Expr::Binary { op: current, .. })) = &mut body[0] {
                        *current = op.into();
                    }
                }
                let error = emit_c_module(&module).unwrap_err();
                assert!(
                    error.contains("word shift requires resolved Int operands"),
                    "{op} {invalid:?} {side}: {error}"
                );
            }
        }
    }
}

#[test]
fn word_shift_projection_preserves_resolved_integer_aliases() {
    for alias in ["i32", "u32"] {
        for op in ["<<", ">>", "lshr", "shl", "shr"] {
            let mut module = crate::in_lang_parse::parse_in_source(
                "fn shift(left:Int,right:Int)->Int{return left<<right;}fn main()->Int{return shift(21,1);}").unwrap();
            if let Decl::Function { params, body, .. } = &mut module.decls[0] {
                for (_, typ) in params {
                    *typ = Typ::Named(alias.into());
                }
                if let Stmt::Return(Some(Expr::Binary { op: current, .. })) = &mut body[0] {
                    *current = op.into();
                }
            }
            if let Decl::Function { name, body, .. } = &mut module.decls[1] {
                *name = "answer".into();
                if let Stmt::Return(Some(Expr::Call { args, .. })) = &mut body[0] {
                    if !matches!(op, "<<" | "shl") {
                        args[0] = Expr::IntLit(84);
                    }
                }
            }
            assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
        }
    }
}

#[test]
fn integer_arithmetic_projection_wraps_without_ubsan_and_retains_float_semantics() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let source = "var trace:Int=0;fn left()->Int{trace=trace*10+1;return 9223372036854775807;}fn right()->Int{trace=trace*10+2;return 1;}fn calc(value:Int,other:Int)->Int{return value+other;}fn nested_symbolic(value:Int,other:Int)->Int{return (value+0)+other;}fn nested_alias(value:Int,other:Int)->Int{return (value+0)+other;}fn negate(value:Int)->Int{return -value;}fn unary_effect()->Int{return -left();}fn unary_skipped()->Int{if false&&((-left())==0){return 7;}return trace;}fn effects()->Int{return left()+right();}fn skipped()->Int{if false&&((left()+right())==0){return 7;}return trace;}fn float_calc(value:Float,other:Float)->Float{return value+other;}fn float_negate(value:Float)->Float{return -value;}fn main()->Int{return 42;}";
    for op in ["+", "add", "-", "sub", "*", "mul"] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl {
                if name == "main" {
                    *name = "answer".into();
                }
                if name == "nested_alias" && matches!(op, "*" | "mul") {
                    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(decl), &mut |expr| {
                        if let Expr::IntLit(value) = expr
                            && *value == 0
                        {
                            *value = 1;
                        }
                    });
                }
                if let Decl::Function { name, .. } = decl
                    && matches!(
                        name.as_str(),
                        "negate" | "unary_effect" | "unary_skipped" | "float_negate"
                    )
                {
                    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(decl), &mut |expr| {
                        if let Expr::Unary { op: current, .. } = expr
                            && current == "-"
                        {
                            *current = if matches!(op, "add" | "sub" | "mul") {
                                "neg"
                            } else {
                                "-"
                            }
                            .into();
                        }
                    });
                }
                if let Decl::Function { name, .. } = decl
                    && matches!(
                        name.as_str(),
                        "calc" | "effects" | "skipped" | "float_calc" | "nested_alias"
                    )
                {
                    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(decl), &mut |expr| {
                        if let Expr::Binary { op: current, .. } = expr
                            && current == "+"
                        {
                            *current = op.into();
                        }
                    });
                }
            }
        }
        let before = module.clone();
        let emitted = emit_c_module(&module).unwrap();
        assert_eq!(module, before);
        let expected = |left: i64, right: i64| match op {
            "+" | "add" => left.wrapping_add(right),
            "-" | "sub" => left.wrapping_sub(right),
            _ => left.wrapping_mul(right),
        };
        let literal = |value: i64| {
            if value == i64::MIN {
                "INT64_MIN".to_owned()
            } else {
                format!("INT64_C({value})")
            }
        };
        let mut checks = String::new();
        for left in [0i64, 1, -1, -84, i64::MIN, i64::MAX] {
            for right in [0i64, 1, -1, 2, i64::MIN, i64::MAX] {
                checks.push_str(&format!("{{volatile int64_t left={};volatile int64_t right={};if(calc(left,right)!={})return 7;}}\n",literal(left),literal(right),literal(expected(left,right))));
            }
            checks.push_str(&format!(
                "{{volatile int64_t value={};if(negate(value)!={})return 8;}}\n",
                literal(left),
                literal(left.wrapping_neg())
            ));
        }
        for left in [i64::MIN, i64::MAX, -84] {
            checks.push_str(&format!("{{volatile int64_t left={};volatile int64_t right=1;if(nested_alias(left,right)!={})return 13;if(nested_symbolic(left,right)!={})return 14;}}\n",literal(left),literal(expected(left,1)),literal(left.wrapping_add(1))));
        }
        checks.push_str(&format!("trace=0;if(unary_effect()!={}||trace!=1)return 15;trace=0;if(unary_skipped()!=0||trace!=0)return 16;",literal(i64::MAX.wrapping_neg())));
        let float_expected = match op {
            "+" | "add" => 4.0,
            "-" | "sub" => -1.0,
            _ => 3.75,
        };
        checks.push_str(&format!("trace=0;if(effects()!={}||trace!=12)return 9;trace=0;if(skipped()!=0||trace!=0)return 10;if(float_calc(1.5,2.5)!={float_expected})return 11;double negative_zero=float_negate(0.0);uint64_t zero_bits;memcpy(&zero_bits,&negative_zero,sizeof(zero_bits));if(zero_bits!=UINT64_C(0x8000000000000000))return 12;return 42;",literal(expected(i64::MAX,1))));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "inauguration-c-arithmetic-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("arithmetic.c");
        let binary = directory.join("arithmetic");
        std::fs::write(
            &path,
            format!("#include <string.h>\n{emitted}\nint main(void){{{checks}}}\n"),
        )
        .unwrap();
        let build = Command::new("clang")
            .args([
                "-std=gnu11",
                "-O2",
                "-fsanitize=undefined",
                "-fno-sanitize-recover=undefined",
            ])
            .arg(&path)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{op}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let result = Command::new(&binary).output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(42),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.stderr.is_empty(),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn overflowing_global_integer_arithmetic_requires_folding_before_projection() {
    for (op, left, right) in [("+", i64::MAX, 1), ("-", i64::MIN, 1), ("*", i64::MAX, 2)] {
        let module = UnifiedModule::new(vec![Decl::Global {
            name: "result".into(),
            typ: Typ::Int,
            init: Some(Box::new(Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::IntLit(left)),
                rhs: Box::new(Expr::IntLit(right)),
            })),
            mutable: true,
            constant_value: false,
        }]);
        assert!(
            emit_c_module(&module)
                .unwrap_err()
                .contains("integer arithmetic in a global initializer requires constant folding")
        );
    }
    let module = UnifiedModule::new(vec![Decl::Global {
        name: "result".into(),
        typ: Typ::Int,
        init: Some(Box::new(Expr::Unary {
            op: "-".into(),
            expr: Box::new(Expr::IntLit(i64::MIN)),
        })),
        mutable: true,
        constant_value: false,
    }]);
    assert!(
        emit_c_module(&module)
            .unwrap_err()
            .contains("integer arithmetic in a global initializer requires constant folding")
    );
    let module = UnifiedModule::new(vec![Decl::Global {
        name: "result".into(),
        typ: Typ::Int,
        init: Some(Box::new(Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::IntLit(20)),
            rhs: Box::new(Expr::IntLit(22)),
        })),
        mutable: true,
        constant_value: false,
    }]);
    assert_inferred_c_executes(&module, "if(result!=42)return 7;return 42;");
}

#[test]
fn malformed_nested_string_arithmetic_does_not_gain_integer_casts() {
    for typ in [Typ::Int, Typ::Named("i32".into()), Typ::Named("u32".into())] {
        let inner = Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::Ident("text".into())),
            rhs: Box::new(Expr::Ident("value".into())),
        };
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "bad".into(),
            params: vec![("text".into(), Typ::String), ("value".into(), typ)],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(inner),
                rhs: Box::new(Expr::IntLit(1)),
            }))],
        }]);
        let emitted = emit_c_module(&module).unwrap();
        let function = &emitted[emitted.rfind("int64_t bad(").unwrap()..];
        assert!(
            !function.contains("(uint64_t)"),
            "unsupported pointer arithmetic gained integer coercion: {function}"
        );
    }
}

#[test]
fn arithmetic_primitive_aliases_preserve_float_results_and_checked_types() {
    let checker = TypeChecker::new();
    let facts = checker.collect_decl_facts(&[]);
    for op in ["+", "add", "-", "sub", "*", "mul"] {
        let expression = Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::Ident("left".into())),
            rhs: Box::new(Expr::Ident("right".into())),
        };
        for (left, right, expected) in [
            (
                Typ::Named("f64".into()),
                Typ::Named("i32".into()),
                Some(Typ::Float),
            ),
            (
                Typ::Named("i32".into()),
                Typ::Named("u32".into()),
                Some(Typ::Int),
            ),
            (Typ::Named("i32".into()), Typ::String, None),
            (Typ::Bool, Typ::Int, None),
        ] {
            let env = HashMap::from([("left".into(), left), ("right".into(), right)]);
            assert_eq!(
                checker.checked_expr_type(&expression, &facts, &env),
                expected,
                "{op}"
            );
        }
    }
    let mut module = crate::in_lang_parse::parse_in_source(
        "fn calculate(left:Float,right:Int)->Float{return (left+right)+1;}fn main()->Int{return 42;}",
    ).unwrap();
    for decl in &mut module.decls {
        if let Decl::Function { name, params, .. } = decl {
            if name == "main" {
                *name = "answer".into();
            }
            if name == "calculate" {
                params[0].1 = Typ::Named("f64".into());
                params[1].1 = Typ::Named("i32".into());
            }
        }
    }
    assert_inferred_c_executes(&module, "if(calculate(1.5,1)!=3.5)return 7;return 42;");
}

#[test]
fn signed_division_projection_matches_native_failure_and_overflow_without_ubsan() {
    use std::process::Command;
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let source = "var trace:Int=0;var after:Int=0;fn left()->Int{trace=trace*10+1;return 84;}fn right()->Int{trace=trace*10+2;return 0;}fn calc(left:Int,right:Int)->Int{let q:Int=left/right;after=1;return q;}fn boolean(left:Int,right:Int)->Bool{let q:Int=left/right;after=1;return q==99;}fn nothing(left:Int,right:Int)->Void{let q:Int=left/right;after=q+10;return;}fn floating(left:Int,right:Int)->Float{let q:Int=left/right;after=1;if q==42{return 1.5;}return 2.5;}fn nested(left:Int,right:Int)->Int{return (left/right)/right;}fn effects()->Int{return left()/right()+100;}fn skipped()->Int{if false&&((left()/right())==0){return 7;}return trace;}fn float_operands(left:Float,right:Float)->Float{return left/right;}fn main()->Int{return 42;}";
    for op in ["/", "div", "%", "mod", "udiv", "urem"] {
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl {
                if name == "main" {
                    *name = "answer".into();
                }
                if name != "float_operands" {
                    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(decl), &mut |expr| {
                        if let Expr::Binary { op: current, .. } = expr
                            && current == "/"
                        {
                            *current = op.into();
                        }
                    });
                }
            }
        }
        let emitted = emit_c_module(&module).unwrap();
        let rem = matches!(op, "%" | "mod" | "urem");
        let unsigned = matches!(op, "udiv" | "urem");
        let expected = |left: i64, right: i64| {
            if unsigned {
                if rem {
                    ((left as u64) % (right as u64)) as i64
                } else {
                    ((left as u64) / (right as u64)) as i64
                }
            } else if left == i64::MIN && right == -1 {
                if rem { 0 } else { i64::MIN }
            } else if rem {
                left % right
            } else {
                left / right
            }
        };
        let literal = |value: i64| {
            if value == i64::MIN {
                "INT64_MIN".to_owned()
            } else {
                format!("INT64_C({value})")
            }
        };
        let mut checks = String::new();
        for left in [i64::MIN, i64::MAX, -84, -1, 0, 84] {
            for right in [-1, 1, 2, i64::MIN, i64::MAX] {
                let result = expected(left, right);
                checks.push_str(&format!("{{volatile int64_t l={};volatile int64_t r={};after=0;if(calc(l,r)!={}||after!=1)return 2;after=0;if(boolean(l,r)!={}||after!=1)return 3;after=0;nothing(l,r);if(after!={})return 4;after=0;if(floating(l,r)!={}||after!=1)return 5;}}\n", literal(left), literal(right), literal(result), if result==99 {1}else{0}, literal(result.wrapping_add(10)), if result==42 {"1.5"}else{"2.5"}));
            }
        }
        // Zero exits the containing function before subsequent statements; Float
        // carries native integer-register failure bits 1, not numerical 1.0.
        checks.push_str("volatile int64_t zero=0;after=0;if(calc(84,zero)!=1||after!=0)return 6;if(!boolean(84,zero)||after!=0)return 7;nothing(84,zero);if(after!=0)return 8;double f=floating(84,zero);uint64_t bits;memcpy(&bits,&f,sizeof(bits));if(bits!=UINT64_C(1)||after!=0)return 9;trace=0;if(effects()!=1||trace!=12)return 10;trace=0;if(skipped()!=0||trace!=0)return 11;if(float_operands(3.0,2.0)!=1.5)return 12;");
        // Both layers use the alias under test; a nested unknown result must not
        // silently fall through to signed C division at the outer layer.
        let inner = expected(i64::MIN, -1);
        checks.push_str(&format!("volatile int64_t minimum=INT64_MIN;volatile int64_t minus_one=-1;if(nested(minimum,minus_one)!={})return 13;return 42;",literal(expected(inner,-1))));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "inauguration-c-signed-div-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("division.c");
        let binary = directory.join("division");
        std::fs::write(
            &path,
            format!("#include <string.h>\n{emitted}\nint main(void){{{checks}}}\n"),
        )
        .unwrap();
        let build = Command::new("clang")
            .args([
                "-std=gnu11",
                "-O2",
                "-fsanitize=undefined",
                "-fno-sanitize-recover=undefined",
            ])
            .arg(&path)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{op}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let result = Command::new(&binary).output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(42),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.stderr.is_empty(),
            "{op}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn signed_division_globals_fold_or_explicitly_refuse_and_string_failure_abi_refuses() {
    for op in ["/", "div", "%", "mod"] {
        let global = |left, right| {
            UnifiedModule::new(vec![Decl::Global {
                name: "result".into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(left)),
                    rhs: Box::new(Expr::IntLit(right)),
                })),
                mutable: true,
                constant_value: false,
            }])
        };
        let expected = if matches!(op, "%" | "mod") { 0 } else { 42 };
        assert_inferred_c_executes(
            &global(84, 2),
            &format!("if(result!={expected})return 7;return 42;"),
        );
        for (left, right) in [(84, 0), (i64::MIN, -1)] {
            let error = emit_c_module(&global(left, right)).unwrap_err();
            assert!(
                error.contains("signed division in a global initializer requires constant folding"),
                "{op}: {error}"
            );
        }
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "unsupported".into(),
            params: vec![],
            ret: Typ::String,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(42)),
                    rhs: Box::new(Expr::IntLit(0)),
                }),
                Stmt::Return(Some(Expr::StringLit("text".into()))),
            ],
        }]);
        let error = emit_c_module(&module).unwrap_err();
        assert!(
            error.contains("checked signed division requires"),
            "{op}: {error}"
        );
    }
}

#[test]
fn word_shift_projection_checks_nested_operand_types_and_preserves_integer_aliases() {
    for op in ["<<", ">>", "lshr", "shl", "shr"] {
        for invalid in ["(\"hello\"+1)", "(true+20)"] {
            for side in [0usize, 1] {
                let expression = if side == 0 {
                    format!("{invalid}<<1")
                } else {
                    format!("84<<{invalid}")
                };
                let mut module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main()->Int{{return {expression};}}"
                ))
                .unwrap();
                crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                    if let Expr::Binary { op: current, .. } = expr
                        && current == "<<"
                    {
                        *current = op.into();
                    }
                });
                let error = emit_c_module(&module).unwrap_err();
                assert!(
                    error.contains("word shift requires resolved Int operands"),
                    "{op} {side} {invalid}: {error}"
                );
            }
        }
        for alias in ["Int", "i32", "u32"] {
            // Exercise internal Core IR aliases without claiming .in type syntax.
            let mut module=crate::in_lang_parse::parse_in_source("fn shift(left:Int,right:Int)->Int{return (left+0)<<(right+0);}fn main()->Int{return shift(21,1);}").unwrap();
            if alias != "Int" {
                if let Decl::Function { params, .. } = &mut module.decls[0] {
                    for (_, typ) in params {
                        *typ = Typ::Named(alias.into());
                    }
                }
            }
            for decl in &mut module.decls {
                if let Decl::Function { name, body, .. } = decl
                    && name == "main"
                {
                    *name = "answer".into();
                    if !matches!(op, "<<" | "shl") {
                        if let Stmt::Return(Some(Expr::Call { args, .. })) = &mut body[0] {
                            args[0] = Expr::IntLit(84);
                        }
                    }
                }
            }
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let Expr::Binary { op: current, .. } = expr
                    && current == "<<"
                {
                    *current = op.into();
                }
            });
            assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
        }
    }
}

#[test]
fn unsigned_division_projection_checks_operand_trees_and_preserves_integer_aliases() {
    let nested = |left| Expr::Binary {
        op: "+".into(),
        lhs: Box::new(left),
        rhs: Box::new(Expr::IntLit(1)),
    };
    for op in ["udiv", "urem"] {
        for bad in [
            Expr::FloatLit(crate::core_ir::FloatVal(2.5)),
            Expr::BoolLit(true),
            Expr::StringLit("hello".into()),
            nested(Expr::StringLit("hello".into())),
            nested(Expr::BoolLit(true)),
            Expr::Ident("unknown".into()),
        ] {
            for side in [0usize, 1] {
                let mut args = [Expr::IntLit(84), Expr::IntLit(2)];
                args[side] = bad.clone();
                let module = UnifiedModule::new(vec![Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Int,
                    type_params: vec![],
                    body: vec![Stmt::Return(Some(Expr::Binary {
                        op: op.into(),
                        lhs: Box::new(args[0].clone()),
                        rhs: Box::new(args[1].clone()),
                    }))],
                }]);
                let error = emit_c_module(&module).unwrap_err();
                assert!(
                    error.contains("unsigned division requires resolved Int operands"),
                    "{op} {side} {bad:?}: {error}"
                );
            }
        }
        for alias in ["Int", "i32", "u32"] {
            let mut module=crate::in_lang_parse::parse_in_source("fn calc(left:Int,right:Int)->Int{return (left+0)/(right+0);}fn main()->Int{return calc(84,2);}").unwrap();
            if let Decl::Function { params, body, .. } = &mut module.decls[0] {
                if alias != "Int" {
                    for (_, typ) in params {
                        *typ = Typ::Named(alias.into());
                    }
                }
                if let Stmt::Return(Some(Expr::Binary { op: current, .. })) = &mut body[0] {
                    *current = op.into();
                }
            }
            if let Decl::Function { name, body, .. } = &mut module.decls[1] {
                *name = "answer".into();
                if op == "urem" {
                    if let Stmt::Return(Some(Expr::Call { args, .. })) = &mut body[0] {
                        *args = vec![Expr::IntLit(85), Expr::IntLit(43)];
                    }
                }
            }
            assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
        }
    }
}

#[test]
fn scalar_binary_projection_matches_owned_primitive_operand_contract() {
    for op in ["&", "|", "xor", "==", "!=", "<", "<=", ">", ">="] {
        for (left_type, left_value) in [
            ("Int", "42"),
            ("Bool", "true"),
            ("Float", "1.5"),
            ("String", "\"hello\""),
        ] {
            for (right_type, right_value) in [
                ("Int", "42"),
                ("Bool", "true"),
                ("Float", "1.5"),
                ("String", "\"hello\""),
            ] {
                let bitwise = matches!(op, "&" | "|" | "xor");
                let same = left_type == right_type;
                let valid = same
                    && if bitwise {
                        matches!(left_type, "Int" | "Bool")
                    } else if matches!(op, "==" | "!=") {
                        true
                    } else {
                        matches!(left_type, "Int" | "Float")
                    };
                let boolean = !bitwise || (left_type == "Bool" && right_type == "Bool");
                let ret = if boolean { "Bool" } else { "Int" };
                let symbol = if op == "xor" { "^" } else { op };
                let expected = if boolean {
                    if matches!(op, "==" | "<=" | ">=" | "&" | "|") {
                        "true"
                    } else {
                        "false"
                    }
                } else if op == "xor" {
                    "0"
                } else {
                    "42"
                };
                let source = format!(
                    "fn test(left:{left_type},right:{right_type})->{ret}{{return left{symbol}right;}}fn main()->Int{{if test({left_value},{right_value})=={expected}{{return 42;}}return 7;}}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                if op == "xor" {
                    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                        if let Expr::Binary { op, .. } = expr
                            && op == "^"
                        {
                            *op = "xor".into();
                        }
                    });
                }
                if valid {
                    if let Decl::Function { name, .. } = &mut module.decls[1] {
                        *name = "answer".into();
                    }
                    assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
                } else {
                    let error = emit_c_module(&module).unwrap_err();
                    assert!(
                        error.contains(
                            "scalar binary operation requires resolved compatible operands"
                        ),
                        "{op} {left_type}/{right_type}: {error}"
                    );
                }
            }
        }
    }
}

#[test]
fn scalar_binary_projection_validates_nested_operands_existing_aliases_and_unknowns() {
    for op in ["==", "!=", "<", "<=", ">", ">=", "&", "|", "^"] {
        for bad in ["(true+1)", "(\"hello\"+1)", "unknown"] {
            for side in [0, 1] {
                let expression = if side == 0 {
                    format!("{bad}{op}1")
                } else {
                    format!("1{op}{bad}")
                };
                let module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main()->Int{{let value={expression};return 42;}}"
                ))
                .unwrap();
                assert!(emit_c_module(&module).is_err(), "{expression}");
            }
        }
    }
    for (alias, symbol) in [
        ("eq", "=="),
        ("neq", "!="),
        ("lt", "<"),
        ("le", "<="),
        ("gt", ">"),
        ("ge", ">="),
        ("band", "&"),
        ("bor", "|"),
    ] {
        let boolean = !matches!(alias, "band" | "bor");
        let ret = if boolean { "Bool" } else { "Int" };
        let expected = if boolean {
            if matches!(alias, "eq" | "le" | "ge") {
                "true"
            } else {
                "false"
            }
        } else {
            "42"
        };
        let mut module=crate::in_lang_parse::parse_in_source(&format!("fn test(left:Int,right:Int)->{ret}{{return left{symbol}right;}}fn main()->Int{{if test(42,42)=={expected}{{return 42;}}return 7;}}")).unwrap();
        crate::core_opt::map_decl_exprs_mut(
            std::slice::from_mut(&mut module.decls[0]),
            &mut |expr| {
                if let Expr::Binary { op, .. } = expr
                    && op == symbol
                {
                    *op = alias.into();
                }
            },
        );
        if let Decl::Function { name, .. } = &mut module.decls[1] {
            *name = "answer".into();
        }
        assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
    }
}

#[test]
fn scalar_comparison_projection_preserves_raw_alias_identity_and_refuses_generic_proof() {
    for (left, right, op, valid) in [
        (Typ::Named("i32".into()), Typ::Int, "==", false),
        (
            Typ::Named("i32".into()),
            Typ::Named("i32".into()),
            "==",
            true,
        ),
        (Typ::Named("i32".into()), Typ::Int, "<", true),
        (Typ::Named("f64".into()), Typ::Float, "<", true),
        (
            Typ::Named("f64".into()),
            Typ::Named("f64".into()),
            "<",
            false,
        ),
        (
            Typ::Generic("T".into()),
            Typ::Generic("T".into()),
            "==",
            false,
        ),
        (
            Typ::Named("Any".into()),
            Typ::Named("Any".into()),
            "==",
            false,
        ),
    ] {
        let mut module = crate::in_lang_parse::parse_in_source(&format!(
            "fn test(left:Int,right:Int)->Bool{{return left{op}right;}}fn main()->Int{{return 42;}}"
        ))
        .unwrap();
        if let Decl::Function { params, .. } = &mut module.decls[0] {
            params[0].1 = left.clone();
            params[1].1 = right.clone();
        }
        let result = emit_c_module(&module);
        assert_eq!(result.is_ok(), valid, "{left:?} {op} {right:?}: {result:?}");
    }
}

#[test]
fn boolean_negation_inferred_locals_keep_negative_integer_and_single_evaluation() {
    let source = "var calls:Int=0;fn next()->Bool{calls+=1;return true;}fn norm(value:Bool)->Int{let result=-value;return result;}fn compared(value:Bool)->Int{if -value == -1{return -1;}return 7;}fn effects()->Int{let result=-next();return result;}fn skipped()->Int{if false&&(-next()==-1){return 7;}return calls;}fn main()->Int{return 42;}";
    let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
    let native = crate::core_ir_verifier::verify_for_entry(&module, "main");
    assert!(native.ok, "{native:?}");
    if let Decl::Function { name, .. } = module.decls.last_mut().unwrap() {
        *name = "answer".into();
    }
    assert_inferred_c_executes(
        &module,
        "if(norm(1)!=-1||norm(0)!=0||compared(1)!=-1)return 7;calls=0;if(effects()!=-1||calls!=1)return 8;calls=0;if(skipped()!=0||calls!=0)return 9;return 42;",
    );
}

#[test]
fn complement_projection_checks_operand_trees_and_retains_integer_aliases() {
    for op in ["~", "bnot"] {
        for operand in [
            "true",
            "1.5",
            "\"hello\"",
            "unknown",
            "(true+1)",
            "(\"hello\"+1)",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(&format!(
                "fn main()->Int{{return ~{operand};}}"
            ))
            .unwrap();
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let Expr::Unary { op: current, .. } = expr
                    && current == "~"
                {
                    *current = op.into();
                }
            });
            let error = emit_c_module(&module).unwrap_err();
            assert!(
                error.contains("bitwise complement requires a resolved Int operand"),
                "{op} {operand}: {error}"
            );
        }
        for alias in ["Int", "i32", "u32"] {
            // Internal Core IR aliases do not imply public .in type syntax support.
            let mut module = crate::in_lang_parse::parse_in_source(
                "fn invert(value:Int)->Int{return ~value;}fn main()->Int{return invert(-43);}",
            )
            .unwrap();
            if alias != "Int" {
                if let Decl::Function { params, .. } = &mut module.decls[0] {
                    params[0].1 = Typ::Named(alias.into());
                }
            }
            if let Decl::Function { name, .. } = &mut module.decls[1] {
                *name = "answer".into();
            }
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let Expr::Unary { op: current, .. } = expr
                    && current == "~"
                {
                    *current = op.into();
                }
            });
            assert_inferred_c_executes(&module, "if(answer()!=42)return 7;return 42;");
        }
    }
}

#[test]
fn complement_projection_evaluates_once_and_preserves_scalar_logical_not() {
    for op in ["~", "bnot"] {
        let mut module=crate::in_lang_parse::parse_in_source("var calls:Int=0;fn next()->Int{calls+=1;return -43;}fn main()->Int{return ~next();}fn skipped()->Int{if false&&(~next()==42){return 7;}return calls;}fn not_float(value:Float)->Bool{return !value;}fn not_string(value:String)->Bool{return !value;}fn not_bool(value:Bool)->Bool{return !value;}").unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "answer".into();
            }
        }
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Unary { op: current, .. } = expr
                && current == "~"
            {
                *current = op.into();
            }
        });
        assert_inferred_c_executes(
            &module,
            "calls=0;if(answer()!=42||calls!=1)return 7;calls=0;if(skipped()!=0||calls!=0)return 8;if(!not_float(-0.0)||not_float(1.5)||not_string(\"\")||not_string(\"hello\")||!not_bool(0)||not_bool(1))return 9;return 42;",
        );
    }
}

#[test]
fn numeric_negation_requires_recursively_checked_scalar_operands() {
    for op in ["-", "neg"] {
        for typ in [
            Typ::String,
            Typ::Array(Box::new(Typ::Int)),
            Typ::Vector(Box::new(Typ::Int)),
            Typ::Named("Unknown".into()),
            Typ::Generic("T".into()),
        ] {
            let module = UnifiedModule::new(vec![Decl::Function {
                name: "main".into(),
                params: vec![("value".into(), typ.clone())],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::Unary {
                    op: op.into(),
                    expr: Box::new(Expr::Ident("value".into())),
                }))],
            }]);
            assert!(emit_c_module(&module).is_err(), "{op} {typ:?}");
        }
    }
    for source in [
        r#"fn main()->Int{return -("hello"+1);}"#,
        "fn main()->Int{return -(true+1);}",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        assert!(emit_c_module(&module).is_err(), "{source}");
    }
}
