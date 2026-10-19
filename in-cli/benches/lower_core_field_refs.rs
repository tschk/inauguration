use criterion::{Criterion, criterion_group, criterion_main};
use inauguration::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ, UnifiedModule, Visibility};
use inauguration::lower_core::desugar_module;
use std::hint::black_box;

fn bench_desugar_class_fields(c: &mut Criterion) {
    let mut fields = Vec::new();
    let mut body = Vec::new();
    for i in 0..1000 {
        fields.push((format!("field_{}", i), Typ::Int));
        body.push(Stmt::Expr(Expr::Ident(format!("field_{}", i))));
    }

    let module = UnifiedModule {
        identity: CoreModuleIdentity::default(),
        decls: vec![Decl::Class {
            name: "MyClass".to_string(),
            type_params: vec![],
            fields,
            methods: vec![Decl::Function {
                name: "do_something".to_string(),
                type_params: vec![],
                params: vec![],
                ret: Typ::Void,
                body,
            }],
            visibility: Visibility::Pub,
            extends: None,
            implements: vec![],
        }],
    };

    c.bench_function("desugar_class_fields", |b| {
        b.iter(|| {
            let mut mod_clone = module.clone();
            desugar_module(black_box(&mut mod_clone));
        })
    });
}

criterion_group!(benches, bench_desugar_class_fields);
criterion_main!(benches);
