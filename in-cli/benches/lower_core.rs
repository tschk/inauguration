use criterion::{Criterion, criterion_group, criterion_main};
use inauguration::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ, UnifiedModule};
use inauguration::lower_core::desugar_module;
use std::hint::black_box;

fn bench_desugar_module(c: &mut Criterion) {
    let mut stmts = Vec::new();
    for i in 0..1000 {
        stmts.push(Stmt::Let(
            format!("var_{}", i),
            None,
            Expr::StringLit("abc".to_string()),
        ));
        stmts.push(Stmt::Expr(Expr::Closure {
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Expr(Expr::Ident(format!("var_{}", i)))],
            captures: vec![],
        }));
    }

    let module = UnifiedModule {
        identity: CoreModuleIdentity::default(),
        decls: vec![Decl::Function {
            name: "main".to_string(),
            type_params: vec![],
            params: vec![],
            ret: Typ::Void,
            body: stmts,
        }],
    };

    c.bench_function("desugar_module_closures", |b| {
        b.iter(|| {
            let mut mod_clone = module.clone();
            desugar_module(black_box(&mut mod_clone));
        })
    });
}

criterion_group!(benches, bench_desugar_module);
criterion_main!(benches);
