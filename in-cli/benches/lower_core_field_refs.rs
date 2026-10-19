use criterion::{Criterion, criterion_group, criterion_main};
use inauguration::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ, UnifiedModule};
use inauguration::lower_core::desugar_module;

fn bench_desugar_module_fields(c: &mut Criterion) {
    let mut fields = Vec::new();
    let mut stmts = Vec::new();

    // Create a large number of fields
    for i in 0..1000 {
        fields.push((format!("field_{}", i), Typ::Int));

        // The method body will reference these fields
        stmts.push(Stmt::Expr(Expr::Ident(format!("field_{}", i))));
    }

    let module = UnifiedModule {
        identity: CoreModuleIdentity::default(),
        decls: vec![Decl::Class {
            name: "MyClass".to_string(),
            type_params: vec![],
            fields,
            methods: vec![Decl::Function {
                name: "my_method".to_string(),
                type_params: vec![],
                params: vec![],
                ret: Typ::Void,
                body: stmts,
            }],
            extends: None,
            implements: vec![],
            visibility: inauguration::core_ir::Visibility::Pub,
        }],
    };

    c.bench_function("desugar_module_fields", |b| {
        b.iter(|| {
            let mut mod_clone = module.clone();
            desugar_module(&mut mod_clone);
        })
    });
}

criterion_group!(benches, bench_desugar_module_fields);
criterion_main!(benches);
