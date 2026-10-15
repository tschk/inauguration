//! Distinguish prefix negation from binary subtraction without changing precedence.
use inauguration::{
    core_ir::{Decl, Expr, Stmt, UnifiedModule},
    in_lang_parse::parse_in_source,
};

const ORIGINAL: &str = "fn combine(a: Int, b: Int, c: Int, d: Int) -> Int {\n return a + (b + -(c + d));\n}\nfn main() -> Int { return combine(1, 2, 3, 4); }\n";

fn id(name: &str) -> Expr {
    Expr::Ident(name.into())
}
fn num(value: i64) -> Expr {
    Expr::IntLit(value)
}
fn neg(expr: Expr) -> Expr {
    Expr::Unary {
        op: "-".into(),
        expr: Box::new(expr),
    }
}
fn bin(op: &str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

struct Case {
    name: &'static str,
    text: &'static str,
    expected: Expr,
    value: i64,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "preserved",
            text: "a + (b + -(c + d))",
            expected: bin(
                "+",
                id("a"),
                bin("+", id("b"), neg(bin("+", id("c"), id("d")))),
            ),
            value: -4,
        },
        Case {
            name: "compact_and_tab",
            text: "a+\t-(b+c)",
            expected: bin("+", id("a"), neg(bin("+", id("b"), id("c")))),
            value: -4,
        },
        Case {
            name: "product",
            text: "b * -(c + d)",
            expected: bin("*", id("b"), neg(bin("+", id("c"), id("d")))),
            value: -14,
        },
        Case {
            name: "subtract_negative",
            text: "a - -b",
            expected: bin("-", id("a"), neg(id("b"))),
            value: 3,
        },
        Case {
            name: "double_negative",
            text: "--(a + b)",
            expected: neg(neg(bin("+", id("a"), id("b")))),
            value: 3,
        },
        Case {
            name: "rhs_call",
            text: "a + -identity(b)",
            expected: bin(
                "+",
                id("a"),
                neg(Expr::Call {
                    callee: Box::new(id("identity")),
                    args: vec![id("b")],
                }),
            ),
            value: -1,
        },
        Case {
            name: "left_subtraction",
            text: "20 - 6 - 3",
            expected: bin("-", bin("-", num(20), num(6)), num(3)),
            value: 11,
        },
        Case {
            name: "left_division",
            text: "24 / 3 / 2",
            expected: bin("/", bin("/", num(24), num(3)), num(2)),
            value: 4,
        },
        Case {
            name: "grouped_subtraction",
            text: "20 - (6 - 3)",
            expected: bin("-", num(20), bin("-", num(6), num(3))),
            value: 17,
        },
        Case {
            name: "minimum_literal",
            text: "a + -9223372036854775808",
            expected: bin("+", id("a"), num(i64::MIN)),
            value: i64::MIN + 1,
        },
        Case {
            name: "negated_kebab",
            text: "a + -read-count",
            expected: bin("+", id("a"), neg(id("read-count"))),
            value: -4,
        },
        Case {
            name: "kebab_name",
            text: "read-count",
            expected: id("read-count"),
            value: 5,
        },
        Case {
            name: "numeric_subtraction",
            text: "1-2",
            expected: bin("-", num(1), num(2)),
            value: -1,
        },
    ]
}

fn parsed(case: &Case) -> Result<UnifiedModule, String> {
    let source = if case.name == "preserved" {
        ORIGINAL.to_string()
    } else {
        format!(
            "fn identity(value: Int) -> Int {{ return value; }}\nfn combine(a: Int, b: Int, c: Int, d: Int) -> Int {{\n let read-count = 5;\n return {};\n}}\nfn main() -> Int {{ return combine(1, 2, 3, 4); }}\n",
            case.text
        )
    };
    parse_in_source(&source)
}

#[test]
fn unary_minus_has_the_expected_tree_and_preserves_associativity() {
    let mut failures = Vec::new();
    for case in cases() {
        let module = match parsed(&case) {
            Ok(module) => module,
            Err(error) => {
                println!("shape {} parse_error={error:?}", case.name);
                failures.push(format!("{}: {error}", case.name));
                continue;
            }
        };
        let actual = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "combine" => {
                    body.iter().find_map(|stmt| match stmt {
                        Stmt::Return(Some(expr)) => Some(expr),
                        _ => None,
                    })
                }
                _ => None,
            })
            .unwrap();
        println!("shape {} pass={}", case.name, actual == &case.expected);
        if actual != &case.expected {
            failures.push(format!(
                "{}: actual={actual:?}, expected={:?}",
                case.name, case.expected
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(all(unix, target_arch = "aarch64"))]
fn native(module: &UnifiedModule) -> i64 {
    use inauguration::{
        jit_runtime::JitRuntime,
        native_emit::{NativeLinkage, lower},
    };
    let code = lower::lower_module(module, "main", NativeLinkage::Executable).unwrap();
    assert!(code.degradations.is_empty(), "{:?}", code.degradations);
    assert!(code.external_refs.is_empty());
    let functions = code
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, code.code.len() as u32 - *offset))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load(&code.code, &functions, &code.relocations)
        .unwrap();
    unsafe { runtime.invoke("main", &[]).unwrap() }
}

#[test]
fn unary_minus_semantics_match_independent_values() {
    let mut failures = Vec::new();
    for case in cases() {
        for optimized in [false, true] {
            let mut module = match parsed(&case) {
                Ok(module) => module,
                Err(error) => {
                    println!("parse {} optimized={optimized} error={error:?}", case.name);
                    failures.push(format!("{} optimized={optimized}: {error}", case.name));
                    continue;
                }
            };
            if optimized {
                inauguration::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
            }
            let interpreted =
                inauguration::native_emit::inisa::eval_module(&module, "main").unwrap();
            println!(
                "INISA {} optimized={optimized} actual={interpreted} expected={} pass={}",
                case.name,
                case.value,
                interpreted == case.value
            );
            if interpreted != case.value {
                failures.push(format!(
                    "INISA {} optimized={optimized}: {interpreted} != {}",
                    case.name, case.value
                ));
            }
            #[cfg(all(unix, target_arch = "aarch64"))]
            {
                let actual = native(&module);
                println!(
                    "native {} optimized={optimized} actual={actual} expected={} pass={}",
                    case.name,
                    case.value,
                    actual == case.value
                );
                if actual != case.value {
                    failures.push(format!(
                        "native {} optimized={optimized}: {actual} != {}",
                        case.name, case.value
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
