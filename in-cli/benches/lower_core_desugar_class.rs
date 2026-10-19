use criterion::{Criterion, criterion_group, criterion_main};
use inauguration::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ, UnifiedModule, Visibility};
use inauguration::lower_core::desugar_module;
use std::hint::black_box;

fn bench_desugar_class_declarations(c: &mut Criterion) {
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    for i in 0..100 {
        fields.push((format!("field_{}", i), Typ::Int));
        let mut body = Vec::new();
        for j in 0..10 {
            body.push(Stmt::Expr(Expr::Ident(format!("field_{}", j))));
            body.push(Stmt::Assign(format!("field_{}", j), Expr::IntLit(42)));
        }
        methods.push(Decl::Function {
            name: format!("method_{}", i),
            type_params: vec![],
            params: vec![],
            ret: Typ::Void,
            body,
        });
    }
    let mut decls = Vec::new();
    for k in 0..10 {
        decls.push(Decl::Class {
            name: format!("MyClass_{}", k),
            fields: fields.clone(),
            methods: methods.clone(),
            type_params: vec![],
            extends: None,
            implements: vec![],
            visibility: Visibility::Pub,
        });
    }
    let module = UnifiedModule {
        identity: CoreModuleIdentity::default(),
        decls,
    };
    c.bench_function("desugar_class_declarations", |b| {
        b.iter(|| {
            let mut mod_clone = module.clone();
            desugar_module(black_box(&mut mod_clone));
        })
    });
}
criterion_group!(benches, bench_desugar_class_declarations);
criterion_main!(benches);
