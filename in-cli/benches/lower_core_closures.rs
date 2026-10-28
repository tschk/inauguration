use criterion::{Criterion, criterion_group, criterion_main};
use inauguration::core_ir::*;
use inauguration::lower_core::lower_to_textual_sil;
use std::hint::black_box;

fn generate_module_with_closures() -> UnifiedModule {
    let mut stmts = Vec::new();
    for i in 0..100 {
        stmts.push(Stmt::Let(
            format!("var_{}", i),
            Some(Typ::Int),
            Expr::IntLit(i as i64),
        ));
    }

    for i in 0..50 {
        // Create a closure that captures the previously defined variables
        let mut closure_body = Vec::new();
        // capture multiple vars
        let cap_expr = Expr::Binary {
            op: "+".to_string(),
            lhs: Box::new(Expr::Ident(format!("var_{}", i))),
            rhs: Box::new(Expr::Ident(format!("var_{}", i + 1))),
        };
        closure_body.push(Stmt::Return(Some(cap_expr)));

        let closure = Expr::Closure {
            params: vec![],
            ret: Typ::Int,
            body: closure_body,
            captures: vec![],
        };

        stmts.push(Stmt::Let(format!("closure_{}", i), None, closure));
    }

    let func = Decl::Function {
        name: "main".to_string(),
        params: vec![],
        ret: Typ::Void,
        body: stmts,
        type_params: vec![],
    };

    UnifiedModule {
        identity: CoreModuleIdentity {
            package: None,
            module: Some("test".to_string()),
        },
        decls: vec![func],
    }
}

fn bench_desugar_closures(c: &mut Criterion) {
    let module = generate_module_with_closures();

    c.bench_function("lower_to_textual_sil_closures", |b| {
        b.iter(|| {
            lower_to_textual_sil(black_box(module.clone()), "test");
        })
    });
}

criterion_group!(benches, bench_desugar_closures);
criterion_main!(benches);
