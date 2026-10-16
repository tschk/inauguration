use crate::compiler::mir::{MirModule, MirOp, TypedOp};
use crate::compiler::mir_lower::lower_to_mir;
use crate::core_ir::Typ;
use std::collections::HashMap;

fn lower(source: &str) -> MirModule {
    let module = inauguration_source(source);
    lower_to_mir(&module).unwrap_or_else(|error| panic!("{source}: {error}"))
}
fn inauguration_source(source: &str) -> crate::core_ir::UnifiedModule {
    crate::in_lang_parse::parse_in_source(source).unwrap()
}

// Test-only semantic oracle for explicit typed dataflow/CFG. This does not
// execute generated machine code and does not stand in for backend acceptance.
fn run(module: &MirModule, entry: &str) -> (Option<i64>, Vec<String>) {
    fn call(
        module: &MirModule,
        name: &str,
        args: &[i64],
        fuel: &mut usize,
        trace: &mut Vec<String>,
    ) -> Option<i64> {
        let function = module
            .functions
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        if function.is_external {
            trace.push(name.into());
            return Some(match name {
                "left" => 20,
                "right" => 22,
                "condition" => i64::from(args[0] < 3),
                _ => panic!("unexpected external {name}"),
            });
        }
        let mut regs = vec![None; function.value_types.len()];
        let mut locals = vec![None; function.local_types.len()];
        for (slot, value) in regs.iter_mut().zip(args) {
            *slot = Some(*value);
        }
        let labels: HashMap<_, _> = function
            .instructions
            .iter()
            .enumerate()
            .filter_map(|(index, inst)| match &inst.op {
                MirOp::Label(name) => Some((name.as_str(), index)),
                _ => None,
            })
            .collect();
        let mut pc = 0;
        loop {
            assert!(*fuel > 0, "typed interpreter budget");
            *fuel -= 1;
            let inst = &function.instructions[pc];
            pc += 1;
            let MirOp::Typed(op) = &inst.op else {
                assert!(matches!(inst.op, MirOp::Label(_)));
                continue;
            };
            let read = |reg: u32| regs[reg as usize].expect("defined value");
            let mut result = None;
            match op {
                TypedOp::ScalarInt { dst, value } => result = Some((*dst, *value)),
                TypedOp::ScalarBool { dst, value } => result = Some((*dst, i64::from(*value))),
                TypedOp::LocalStore { local, src } => locals[*local as usize] = Some(read(*src)),
                TypedOp::LocalLoad { dst, local } => {
                    result = Some((*dst, locals[*local as usize].expect("initialized local")))
                }
                TypedOp::Binary { dst, op, lhs, rhs } => {
                    let (a, b) = (read(*lhs), read(*rhs));
                    let value = match op.as_str() {
                        "+" | "add" => a.wrapping_add(b),
                        "-" | "sub" => a.wrapping_sub(b),
                        "*" | "mul" => a.wrapping_mul(b),
                        "/" | "div" => a.checked_div(b).expect("valid division"),
                        "%" | "rem" => a.checked_rem(b).expect("valid remainder"),
                        "udiv" => ((a as u64) / (b as u64)) as i64,
                        "urem" => ((a as u64) % (b as u64)) as i64,
                        "&" | "bitand" => a & b,
                        "|" | "bitor" => a | b,
                        "^" | "xor" | "bitxor" => a ^ b,
                        "<<" | "shl" => a.wrapping_shl(b as u32),
                        ">>" | "shr" => a.wrapping_shr(b as u32),
                        "lshr" => (a as u64).wrapping_shr(b as u32) as i64,
                        "==" | "eq" => i64::from(a == b),
                        "!=" | "neq" => i64::from(a != b),
                        "<" => i64::from(a < b),
                        "<=" => i64::from(a <= b),
                        ">" => i64::from(a > b),
                        ">=" => i64::from(a >= b),
                        _ => panic!("binary {op}"),
                    };
                    result = Some((*dst, value));
                }
                TypedOp::Unary { dst, op, src } => {
                    let value = read(*src);
                    result = Some((
                        *dst,
                        match op.as_str() {
                            "-" | "neg" => value.wrapping_neg(),
                            "~" | "bitnot" => !value,
                            "!" | "not" => i64::from(value == 0),
                            _ => panic!("unary"),
                        },
                    ));
                }
                TypedOp::DirectCall { dst, target, args } => {
                    let values: Vec<_> = args.iter().map(|reg| read(*reg)).collect();
                    let returned = call(module, target, &values, fuel, trace);
                    match (dst, returned) {
                        (Some(dst), Some(value)) => result = Some((*dst, value)),
                        (None, None) => {}
                        _ => panic!("call result"),
                    }
                }
                TypedOp::Jump(label) => pc = labels[label.as_str()],
                TypedOp::BranchIf {
                    cond,
                    then_label,
                    else_label,
                } => {
                    pc = labels[if read(*cond) != 0 {
                        then_label.as_str()
                    } else {
                        else_label.as_str()
                    }]
                }
                TypedOp::ReturnValue(value) => return value.map(read),
                _ => panic!("legacy typed op"),
            }
            if let Some((dst, value)) = result {
                regs[dst as usize] = Some(value);
            }
        }
    }
    let mut trace = vec![];
    let value = call(module, entry, &[], &mut 10000, &mut trace);
    (value, trace)
}

#[test]
fn scalar_typed_values_calls_and_branching_preserve_results() {
    for source in [
        "fn add(a:Int,b:Int)->Int{return a+b;}fn main()->Int{return add(20,22);}",
        "fn main()->Int{if false{return 7;}return (20+22)*(3-2);}",
        "fn main()->Int{let i:Int=0;let result:Int=0;while i<3{i+=1;if i<3{continue;}result+=42;break;}return result;}",
        "fn main()->Int{let x:Int=20;if true{let x:Int=x+22;if x!=42{return 0;}}return x+22;}",
        "fn main()->Int{let value:Int=if true {20}else{0};return value+22;}",
        "fn main()->Int{return {return 42;0};}",
        "fn main()->Int{if !0{return 42;}return 0;}",
        "fn main()->Int{let i:Int=0;let total:Int=0;while i<3{i+=1;let j:Int=0;while j<3{j+=1;if j==1{continue;}total+=7;break;}}return total*2;}",
        "fn main()->Int{let value:Int=if true {return 42;0}else{7};return value;}",
        "fn main()->Int{if (false||{return 42;false}){return 0;}return 0;}",
    ] {
        let module = lower(source);
        assert!(module.verify(Some("main")).is_ok());
        assert_eq!(run(&module, "main").0, Some(42), "{source}");
    }
}
#[test]
fn scalar_typed_cfg_keeps_lazy_and_argument_evaluation_order() {
    let module = lower(
        "extern c fn left()->Int;extern c fn right()->Int;fn add(a:Int,b:Int)->Int{return a+b;}fn main()->Int{return add(left(),right());}",
    );
    assert_eq!(
        run(&module, "main"),
        (Some(42), vec!["left".into(), "right".into()])
    );
    let effectful = lower(
        "extern c fn condition(value:Int)->Bool;fn main()->Int{let i:Int=0;while condition(i){i+=1;}return i*14;}",
    );
    assert_eq!(
        run(&effectful, "main"),
        (Some(42), vec!["condition".into(); 4])
    );
    let module = lower(
        "extern c fn fail()->Bool;fn main()->Int{if false&&fail(){return 0;}if true||fail(){return 42;}return 0;}",
    );
    assert_eq!(run(&module, "main"), (Some(42), vec![]));
    assert!(module.functions.iter().any(|function| {
        function
            .instructions
            .iter()
            .any(|inst| matches!(inst.op, MirOp::Typed(TypedOp::BranchIf { .. })))
    }));
}
#[test]
fn scalar_typed_definitions_void_and_externals_have_distinct_metadata() {
    let module = lower("extern c fn pending(value:Int)->Int;fn main()->Void{return;}");
    assert!(module.functions[0].is_external);
    assert!(module.verify(Some("pending")).is_err());
    assert!(module.functions[0].instructions.is_empty());
    assert_eq!(module.functions[0].value_types, vec![Typ::Int]);
    assert_eq!(module.functions[0].local_types, vec![Typ::Int]);
    assert!(!module.functions[1].is_external);
    assert_eq!(run(&module, "main"), (None, vec![]));
    let module = lower("fn main()->Int{while true{continue;}}");
    assert!(module.verify(Some("main")).is_ok()); // compile/CFG only, not run
}
#[test]
fn scalar_typed_unsupported_shapes_refuse_instead_of_disappearing() {
    for source in [
        "fn main()->Float{return 42.0;}",
        "fn main()->String{return \"text\";}",
        "struct Row{Int x}fn main()->Int{let row:Row=Row{x:1};row.x=42;return row.x;}",
        "fn main()->Int{let values=[42];return values[0];}",
        "fn main()->Int{throw 42;}",
        "fn main()->Int{return 42;let values=[1,2];}",
    ] {
        assert!(
            lower_to_mir(&inauguration_source(source)).is_err(),
            "{source}"
        );
    }
}
#[test]
fn scalar_typed_verifier_checks_explicit_use_def_types_and_local_targets() {
    let original = lower("fn main()->Int{let value:Int=42;return value;}");
    let mut invalid = original.clone();
    let function = &mut invalid.functions[0];
    let load = function
        .instructions
        .iter_mut()
        .find(|inst| matches!(inst.op, MirOp::Typed(TypedOp::LocalLoad { .. })))
        .unwrap();
    if let MirOp::Typed(TypedOp::LocalLoad { local, .. }) = &mut load.op {
        *local = 999;
    }
    assert!(invalid.verify(None).is_err());
    let mut invalid = original.clone();
    invalid.functions[0].instructions[0].op = MirOp::Typed(TypedOp::Nop);
    assert!(
        invalid
            .verify(None)
            .unwrap_err()
            .iter()
            .any(|error| error.contains("legacy"))
    );
    let mut invalid = original.clone();
    invalid.functions[0].instructions[0].op = MirOp::Typed(TypedOp::Jump("absent".into()));
    assert!(invalid.verify(None).is_err());
    let mut invalid = original.clone();
    invalid.functions[0].value_types[0] = Typ::Bool;
    assert!(invalid.verify(None).is_err());
    let mut invalid = original.clone();
    invalid.functions[0]
        .instructions
        .retain(|inst| !matches!(inst.op, MirOp::Typed(TypedOp::LocalStore { .. })));
    assert!(
        invalid
            .verify(None)
            .unwrap_err()
            .iter()
            .any(|error| error.contains("initialization"))
    );
}

#[test]
fn scalar64_boundary_refuses_unrepresented_widths_in_signatures_and_local_annotations() {
    use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
    for name in [
        "i8",
        "i16",
        "i32",
        "u8",
        "u16",
        "u32",
        "u64",
        "int",
        "isize",
        "usize",
        "i128",
        "u128",
        "#integer-u8",
        "#rust-integer-i64",
    ] {
        let typ = Typ::Named(name.into());
        assert!(super::scalar_type(&typ).is_err(), "{name}");
        let function = |params, ret, body| {
            UnifiedModule::new(vec![Decl::Function {
                name: "main".into(),
                params,
                ret,
                body,
                type_params: vec![],
            }])
        };
        for module in [
            function(
                vec![("value".into(), typ.clone())],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                vec![],
                typ.clone(),
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                vec![],
                Typ::Int,
                vec![
                    Stmt::Let("value".into(), Some(typ.clone()), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
            ),
        ] {
            assert!(lower_to_mir(&module).is_err(), "{name}: {module:?}");
        }
    }
    for typ in [
        Typ::Float,
        Typ::String,
        Typ::Array(Box::new(Typ::Int)),
        Typ::Named("Row".into()),
        Typ::Generic("T".into()),
    ] {
        assert!(super::scalar_type(&typ).is_err(), "{typ:?}");
    }
}
#[test]
fn scalar64_boundary_keeps_normalized_primitives_and_explicit_safe_aliases() {
    use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
    for typ in [Typ::Int, Typ::Bool, Typ::Void] {
        assert_eq!(super::scalar_type(&typ).unwrap(), typ);
    }
    for (name, expected) in [
        ("Int", Typ::Int),
        ("i64", Typ::Int),
        ("Bool", Typ::Bool),
        ("bool", Typ::Bool),
        ("Void", Typ::Void),
        ("void", Typ::Void),
        ("Unit", Typ::Void),
        ("unit", Typ::Void),
        ("()", Typ::Void),
    ] {
        let source_type = Typ::Named(name.into());
        let (params, body) = if expected == Typ::Void {
            (vec![], vec![Stmt::Return(None)])
        } else {
            (
                vec![("value".into(), source_type.clone())],
                vec![Stmt::Return(Some(Expr::Ident("value".into())))],
            )
        };
        let core = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params,
            ret: source_type,
            body,
            type_params: vec![],
        }]);
        let result = lower_to_mir(&core).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(result.functions[0].return_type, Some(expected));
        assert!(result.verify(Some("main")).is_ok());
    }
}

#[test]
fn harden_private_unreachable_is_a_verified_typed_terminator() {
    use crate::compiler::mir::{MirOperand, mir};
    use crate::compiler::mir_scalar_lower::{ScalarTarget, lower_scalar_typed};
    let mut source = inauguration_source("fn main()->Int{return 42;}");
    crate::core_opt::optimize_with_profile(
        &mut source.decls,
        Some("main"),
        crate::emit_profile::EmitProfile::Harden,
    );
    let typed = lower_to_mir(&source).unwrap();
    assert!(
        typed
            .functions
            .iter()
            .flat_map(|f| &f.instructions)
            .any(|inst| matches!(inst.op, MirOp::Typed(TypedOp::Trap)))
    );
    for target in [ScalarTarget::Aarch64, ScalarTarget::X86_64] {
        let machine = lower_scalar_typed(&typed, target).unwrap();
        assert!(machine.verify_machine().is_ok());
        assert!(
            machine
                .functions
                .iter()
                .flat_map(|f| &f.instructions)
                .any(|inst| matches!(inst.op, MirOp::Trap))
        );
    }
    let mut only_trap = lower("fn main()->Int{return 42;}");
    let function = &mut only_trap.functions[0];
    function.value_types.clear();
    function.local_types.clear();
    function.vreg_count = 0;
    function.instructions = vec![mir(MirOp::Typed(TypedOp::Trap), vec![])];
    assert!(only_trap.verify(Some("main")).is_ok());
    only_trap.functions[0].instructions[0]
        .operands
        .push(MirOperand::Imm(0));
    assert!(only_trap.verify(Some("main")).is_err());
    only_trap.functions[0].instructions = vec![mir(MirOp::Label("entry".into()), vec![])];
    assert!(only_trap.verify(Some("main")).is_err());
}

#[test]
fn canonical_xor_alias_survives_lua_normalization_and_typed_mir() {
    let mut source = inauguration_source(
        "fn mix(left:Int,right:Int)->Int{return left^right;}fn main()->Int{return mix(63,21);}",
    );
    let mut rewritten = 0;
    crate::core_opt::map_decl_exprs_mut(&mut source.decls, &mut |expr| {
        if let crate::core_ir::Expr::Binary { op, .. } = expr
            && op == "^"
        {
            *op = "~".into();
            rewritten += 1;
        }
    });
    assert_eq!(rewritten, 1);
    let normalized =
        crate::typecheck::normalize_module(crate::parser_registry::ParserId::Lua, &source);
    let repeated =
        crate::typecheck::normalize_module(crate::parser_registry::ParserId::Lua, &normalized);
    assert_eq!(normalized.decls, repeated.decls);
    let module = lower_to_mir(&normalized).unwrap();
    assert!(module.verify(Some("main")).is_ok());
    assert!(
        module
            .functions
            .iter()
            .any(|function| function.instructions.iter().any(|inst| {
                matches!(&inst.op, MirOp::Typed(TypedOp::Binary { op, .. }) if op == "xor")
            }))
    );
    assert_eq!(run(&module, "main"), (Some(42), vec![]));
    for target in [
        crate::compiler::mir_scalar_lower::ScalarTarget::Aarch64,
        crate::compiler::mir_scalar_lower::ScalarTarget::X86_64,
    ] {
        let machine =
            crate::compiler::mir_scalar_lower::lower_scalar_typed(&module, target).unwrap();
        assert!(machine.functions.iter().any(|function| {
            function
                .instructions
                .iter()
                .any(|inst| matches!(inst.op, MirOp::Xor))
        }));
    }
}
