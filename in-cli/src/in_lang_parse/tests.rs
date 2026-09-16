use std::fs;

use super::{
    Decl, Expr, InExternBinding, InParallelTaskFact, InSemanticBinding, LoopKind, Stmt, Typ,
    in_standard_import_bindings, parse_expr, parse_in_file, parse_in_source, parse_in_surface_info,
    split_top_level_decl_blocks,
};

#[test]
fn test_split_top_level_decl_blocks_empty() {
    let src = "";
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 0);
}

#[test]
fn test_split_top_level_decl_blocks_single() {
    let src = "fn test() {}\n";
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].0, 1);
    assert_eq!(blocks[0].1, "fn test() {}");
}

#[test]
fn test_split_top_level_decl_blocks_multiple_spacing() {
    let src = "fn a() {}\n\nfn b() {}\n";
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].0, 1);
    assert_eq!(blocks[0].1, "fn a() {}");
    assert_eq!(blocks[1].0, 3);
    assert_eq!(blocks[1].1, "fn b() {}");
}

#[test]
fn test_split_top_level_decl_blocks_nested_braces() {
    let src = "struct A {\n  fn b() {}\n}\nfn c() {}";
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].0, 1);
    assert_eq!(blocks[0].1, "struct A {\nfn b() {}\n}");
    assert_eq!(blocks[1].0, 4);
    assert_eq!(blocks[1].1, "fn c() {}");
}

#[test]
fn test_split_top_level_decl_blocks_comments() {
    let src = "// comment\nfn a() {}\n// another\n";
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].0, 2);
    assert_eq!(blocks[0].1, "fn a() {}");
}

#[test]
fn ignores_nested_fn_at_nonzero_depth() {
    let src = r#"
struct Outer {
    fn inner() -> void
}
fn main() -> void
"#;
    let blocks = split_top_level_decl_blocks(src);
    assert_eq!(blocks.len(), 2);
    assert!(blocks[1].1.contains("main"));
    let err = parse_in_source(src).expect_err("struct with fn inside");
    assert!(err.contains("fn") || err.contains("struct"));
}

#[test]
fn test_parse_in_surface_skips_closed_world_topology_blocks() {
    let src = r#"
        package subspace.demo
        import "../components/uart.in"
        system vertical-slice {
          target "thumbv8m.main-none-eabi"
          board "mps2-an521"
        }
        domain kernel { isolated false }
        instance uart0 { component uart_driver domain kernel }
        task prod_task { instance prod priority 10 }
        grant uart0_mmio { kind peripheral owner uart0 }
        port msg { type U32 from prod_task to cons_task }
        interrupt timer0 { irq 8 owner uart0 }
        startup uart0 { after }
        component uart_driver {
          target "thumbv8m.main-none-eabi"
        }
    "#;
    let info = parse_in_surface_info(src).expect("topology blocks are skipped, not unknown syntax");
    assert_eq!(info.package, Some("subspace.demo".to_string()));
    assert_eq!(info.imports, vec![r#""../components/uart.in""#.to_string()]);
}

#[test]
fn test_parse_in_surface_info_happy_path() {
    let src = "
        package my-pkg
        module my-mod
        import foo;
        use bar
        bind baz as bz
        capability network;
        parallel {
            task1()
        }
    ";
    let info = parse_in_surface_info(src).expect("should parse successfully");
    assert_eq!(info.package, Some("my-pkg".to_string()));
    assert_eq!(info.module, Some("my-mod".to_string()));
    assert_eq!(info.imports, vec!["foo".to_string()]);
    assert_eq!(info.semantic_imports, vec!["bar".to_string()]);
    assert_eq!(
        info.semantic_bindings,
        vec![InSemanticBinding {
            import: "baz".to_string(),
            alias: "bz".to_string()
        }]
    );
    assert_eq!(info.capabilities, vec!["network".to_string()]);
    assert_eq!(info.orchestration.parallel_regions, 1);
    assert_eq!(
        info.orchestration.parallel_tasks,
        vec![InParallelTaskFact {
            region: 0,
            name: "task1".to_string()
        }]
    );
}

#[test]
fn test_parse_in_surface_info_duplicate_package() {
    let src = "package pkg1\npackage pkg2";
    let err = parse_in_surface_info(src).expect_err("should fail");
    assert!(err.contains("duplicate package declaration"));
}

#[test]
fn test_parse_in_surface_info_duplicate_module() {
    let src = "module mod1\nmodule mod2";
    let err = parse_in_surface_info(src).expect_err("should fail");
    assert!(err.contains("duplicate module declaration"));
}

#[test]
fn test_parse_in_surface_info_unknown_syntax() {
    let src = "unknown_keyword foo";
    let err = parse_in_surface_info(src).expect_err("should fail");
    assert!(err.contains("unknown top-level syntax"));
}

#[test]
fn void_return_case_insensitive() {
    let m = parse_in_source("fn main() -> VOID\n").expect("ok");
    match &m.decls[0] {
        Decl::Function { ret, .. } => assert!(matches!(ret, Typ::Void)),
        _ => panic!("expected fn"),
    }
}

#[test]
fn rejects_malformed_param_without_type() {
    let err = parse_in_source("fn main(bad) -> void\n").expect_err("param");
    assert!(err.contains("name: Type") || err.contains("parameter"));
}

#[test]
fn rejects_duplicate() {
    let err = parse_in_source("fn main() -> void\nfn main() -> void\n").expect_err("dup");
    assert!(err.contains("duplicate"));
}

#[test]
fn struct_parses_inline_fields() {
    let m = parse_in_source("struct Box { Int x; String label }\nfn main() -> void\n").expect("ok");
    let st = m.decls.iter().find_map(|d| match d {
        Decl::Struct { name, fields, .. } if name == "Box" => Some(fields.clone()),
        _ => None,
    });
    let fields = st.expect("struct Box");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0], ("x".into(), Typ::Int));
    assert_eq!(fields[1], ("label".into(), Typ::String));
}

#[test]
fn struct_parses_multiline_fields() {
    let src = r#"
struct Card {
  Int rank
  String suit
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("parse");
    let fields = match &m.decls[0] {
        Decl::Struct { name, fields, .. } if name == "Card" => fields.clone(),
        _ => panic!("expected Card"),
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].0, "rank");
    assert_eq!(fields[1].0, "suit");
}

#[test]
fn struct_initializer_and_field_access_parse_in_body() {
    let module = parse_in_source(
            "struct Point { Int x; Int y }\nfn main() -> Int { let p: Point = Point { x: 2, y: 5 }; return p.y; }\n",
        )
        .expect("ok");
    let Decl::Function { body, .. } = &module.decls[1] else {
        panic!("fn");
    };
    assert!(matches!(
        &body[0],
        Stmt::Let(
            name,
            Some(Typ::Named(ty)),
            Expr::StructInit { name: init, fields }
        ) if name == "p"
            && ty == "Point"
            && init == "Point"
            && matches!(fields.as_slice(), [(x, Expr::IntLit(2)), (y, Expr::IntLit(5))] if x == "x" && y == "y")
    ));
    assert!(matches!(
        &body[1],
        Stmt::Return(Some(Expr::Field { base, name, ..}))
            if name == "y" && matches!(base.as_ref(), Expr::Ident(ident) if ident == "p")
    ));
}

#[test]
fn direct_struct_initializer_field_access_stays_one_statement() {
    let module = parse_in_source(
        "struct Point { Int x; Int y }\nfn main() -> Int { return Point { x: 2, y: 5 }.y; }\n",
    )
    .expect("ok");
    let Decl::Function { body, .. } = &module.decls[1] else {
        panic!("fn");
    };
    assert_eq!(body.len(), 1);
    assert!(matches!(
        &body[0],
        Stmt::Return(Some(Expr::Field { base, name, ..}))
            if name == "y"
                && matches!(base.as_ref(), Expr::StructInit { name: init, .. } if init == "Point")
    ));
}

#[test]
fn struct_initializer_rejects_unknown_field() {
    let err = parse_in_source(
        "struct Point { Int x; Int y }\nfn main() -> Point { return Point { x: 1, z: 2 }; }\n",
    )
    .expect_err("unknown initializer field should fail");

    assert!(
        err.contains("unknown field `Point.z`"),
        "unexpected error: {err}"
    );
}

#[test]
fn struct_initializer_rejects_missing_field() {
    let err = parse_in_source(
        "struct Point { Int x; Int y }\nfn main() -> Point { return Point { x: 1 }; }\n",
    )
    .expect_err("missing initializer field should fail");

    assert!(
        err.contains("missing field `Point.y`"),
        "unexpected error: {err}"
    );
}

#[test]
fn struct_initializer_rejects_duplicate_field() {
    let err = parse_in_source(
            "struct Point { Int x; Int y }\nfn main() -> Point { return Point { x: 1, x: 2, y: 3 }; }\n",
        )
        .expect_err("duplicate initializer field should fail");

    assert!(
        err.contains("duplicate field `Point.x`"),
        "unexpected error: {err}"
    );
}

#[test]
fn struct_skips_field_line_comments() {
    let src = r#"
struct S {
  Int a // id
  String b
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let fields = match &m.decls[0] {
        Decl::Struct { fields, .. } => fields,
        _ => panic!("struct"),
    };
    assert_eq!(fields.len(), 2);
}

#[test]
fn struct_field_type_must_be_known() {
    let err = parse_in_source("struct Bad { Unknown z }\nfn main() -> void\n").expect_err("ty");
    assert!(err.contains("unknown type") || err.contains("Bad"));
}

#[test]
fn surface_info_parses_imports_capabilities_and_externs() {
    let src = r#"
import std.fs;
capability fs.read;
extern rust fn read-file(path: String) -> String;
fn main() -> void { read-file("x"); return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(info.imports, vec!["std.fs"]);
    assert_eq!(info.capabilities, vec!["fs.read"]);
    assert_eq!(
        info.externs,
        vec![InExternBinding {
            language: "rust".into(),
            name: "read-file".into(),
            params: vec![("path".into(), Typ::String)],
            required_capabilities: Vec::new(),
            ret: Some(Typ::String),
        }]
    );
}

#[test]
fn surface_info_parses_package_and_module_facts() {
    let src = r#"
package agents.video;
module agents.video.main;
fn main() -> void { return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(info.package.as_deref(), Some("agents.video"));
    assert_eq!(info.module.as_deref(), Some("agents.video.main"));
    parse_in_source(src).expect("parse");
}

#[test]
fn parsed_module_carries_package_and_module_identity() {
    let module = parse_in_source(
        "package agents.video;\nmodule agents.video.main;\nfn main() -> void { return; }\n",
    )
    .expect("parse");
    assert_eq!(module.identity.package.as_deref(), Some("agents.video"));
    assert_eq!(module.identity.module.as_deref(), Some("agents.video.main"));
    assert_eq!(module.effective_module_id("App"), "agents.video.main");
    assert_eq!(module.effective_module_id("Explicit"), "Explicit");
}

#[test]
fn surface_info_parses_semantic_use_imports() {
    let src = r#"
package hyperchat;
use database.postgres;
fn main() -> void { return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(info.semantic_imports, vec!["database.postgres"]);
    parse_in_source(src).expect("parse");
}

#[test]
fn surface_info_parses_semantic_bindings() {
    let src = r#"
package hyperchat;
use database.postgres;
bind database.postgres as postgres;
fn main() -> void { return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(info.semantic_imports, vec!["database.postgres"]);
    assert_eq!(
        info.semantic_bindings,
        vec![InSemanticBinding {
            import: "database.postgres".into(),
            alias: "postgres".into(),
        }]
    );
    parse_in_source(src).expect("parse");
}

#[test]
fn duplicate_package_or_module_facts_are_rejected() {
    let err = parse_in_source(
        "package one;\npackage two;\nmodule one.main;\nfn main() -> void { return; }\n",
    )
    .expect_err("duplicate package fact");
    assert!(err.contains("duplicate package"), "{err}");

    let err = parse_in_source(
        "package one;\nmodule one.main;\nmodule one.extra;\nfn main() -> void { return; }\n",
    )
    .expect_err("duplicate module fact");
    assert!(err.contains("duplicate module"), "{err}");
}

#[test]
fn surface_info_parses_orchestration_facts_without_core_lowering() {
    let src = r#"
enable distributed-workers;
@gpu
distributed fn process-video(video: Video) -> void {
  return;
}
parallel {
  warm-cache();
  build-index();
}
struct Video { Int id }
fn main() -> void { return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(
        info.orchestration.enabled_extensions,
        vec!["distributed-workers"]
    );
    assert_eq!(info.orchestration.parallel_regions, 1);
    assert_eq!(
        info.orchestration.parallel_tasks,
        vec![
            InParallelTaskFact {
                region: 0,
                name: "warm-cache".into()
            },
            InParallelTaskFact {
                region: 0,
                name: "build-index".into()
            }
        ]
    );
    assert_eq!(
        info.orchestration.distributed_functions,
        vec!["process-video"]
    );
    assert_eq!(info.orchestration.annotations[0].name, "gpu");
    assert_eq!(
        info.orchestration.annotations[0].target.as_deref(),
        Some("process-video")
    );

    let module = parse_in_source(src).expect("parse");
    assert!(
        !module
            .decls
            .iter()
            .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "process-video"))
    );
}

#[test]
fn human_facing_readme_style_source_parses() {
    let src = r#"
import std.io

needs process.stdout

host-log(text: String) uses process.stdout

Message:
  text: String

main:
  print "hello from .in"
  host-log "compiler-visible effect"
"#;
    let module = parse_in_source(src).expect("parse");
    assert!(
        module
            .decls
            .iter()
            .any(|decl| { matches!(decl, Decl::Struct { name, .. } if name == "Message") })
    );
    assert!(
        module
            .decls
            .iter()
            .any(|decl| { matches!(decl, Decl::Function { name, .. } if name == "main") })
    );
    assert!(
        module
            .decls
            .iter()
            .any(|decl| { matches!(decl, Decl::Function { name, .. } if name == "print") })
    );
}

#[test]
fn malformed_orchestration_syntax_is_rejected() {
    let err = parse_in_source("parallel warm-cache();\nfn main() -> void { return; }\n")
        .expect_err("parallel shape");
    assert!(err.contains("parallel"), "{err}");

    let err =
        parse_in_source("@unknown\nfn main() -> void { return; }\n").expect_err("annotation shape");
    assert!(err.contains("unsupported annotation"), "{err}");

    let err = parse_in_source("gpu fn kernel() -> void { }\nfn main() -> void { return; }\n")
        .expect_err("unknown orchestration");
    assert!(err.contains("unknown top-level syntax"), "{err}");
}

#[test]
fn semantic_bindings_lower_as_function_decl_in_core_ir() {
    let src = r#"
package hyperchat;
use database.postgres;
bind database.postgres as postgres;
fn main() -> void { return; }
"#;
    let module = parse_in_source(src).expect("parse");
    let has_postgres_decl = module.decls.iter().any(
        |d| matches!(d, Decl::Function { name, body, .. } if name == "postgres" && body.is_empty()),
    );
    assert!(
        has_postgres_decl,
        "bind alias should produce Decl::Function with empty body"
    );
}

#[test]
fn semantic_bindings_are_callable_in_sil() {
    let src = r#"
package hyperchat;
use database.postgres;
bind database.postgres as postgres;
fn main() -> void { postgres("select 1"); return; }
"#;
    let module = parse_in_source(src).expect("parse");
    let sil = crate::lower_core::lower_to_textual_sil(module.clone(), "test");
    assert!(
        sil.contains("function_ref @postgres"),
        "bind alias should lower to function_ref\n{sil}"
    );
}

#[test]
fn extern_binding_parses_required_capabilities() {
    let src = r#"
capability fs.read;
extern rust fn read-file(path: String) -> String requires fs.read, json.parse;
fn main() -> void { read-file("x"); return; }
"#;
    let info = parse_in_surface_info(src).expect("surface");
    assert_eq!(
        info.externs[0].required_capabilities,
        vec!["fs.read", "json.parse"]
    );
}

#[test]
fn extern_binding_lowers_as_empty_function_decl() {
    let src = r#"
extern rust fn read-file(path: String) -> String;
fn main() -> void { read-file("x"); return; }
"#;
    let m = parse_in_source(src).expect("ok");
    let extern_decl = m.decls.iter().find_map(|d| match d {
        Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } if name == "read-file" => Some((params, ret, body)),
        _ => None,
    });
    let (params, ret, body) = extern_decl.expect("read-file");
    assert_eq!(params.len(), 1);
    assert!(matches!(ret, Typ::String));
    assert!(body.is_empty());
}

#[test]
fn malformed_surface_declaration_rejected() {
    let err = parse_in_source("import ;\nfn main() -> void\n").expect_err("import");
    assert!(err.contains("import path missing"), "{err}");
}

#[test]
fn malformed_capability_rejected() {
    let err = parse_in_source("capability ;\nfn main() -> void\n").expect_err("capability");
    assert!(err.contains("capability name missing"), "{err}");
}

#[test]
fn extern_body_rejected() {
    let err = parse_in_source("extern rust fn f() -> void { return; }\nfn main() -> void\n")
        .expect_err("extern body");
    assert!(err.contains("extern") && err.contains("bodies"), "{err}");
}

#[test]
fn file_import_merges_local_in_declarations() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-in-import-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("temp dir");
    let lib = dir.join("lib.in");
    let main = dir.join("main.in");
    fs::write(&lib, "fn helper() -> Int { return 1; }\n").expect("write lib");
    fs::write(
        &main,
        "import \"./lib.in\";\nfn main() -> void { helper(); return; }\n",
    )
    .expect("write main");
    let module = parse_in_file(&main).expect("parse imported file");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        module
            .decls
            .iter()
            .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "helper"))
    );
}

#[test]
fn file_import_reports_missing_local_in_file() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "inauguration-missing-import-{}-{unique}.in",
        std::process::id()
    ));
    fs::write(
        &path,
        "import \"./missing.in\";\nfn main() -> void { return; }\n",
    )
    .expect("write main");
    let err = parse_in_file(&path).expect_err("missing import");
    let _ = fs::remove_file(&path);
    assert!(err.contains("missing.in"), "{err}");
}

#[test]
fn std_import_adds_core_function_declarations() {
    let src = "import std.io;\ncapability process.stdout;\nfn main() -> void { print(\"ok\"); return; }\n";
    let module = parse_in_source(src).expect("std import");
    assert!(
        module
            .decls
            .iter()
            .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "print"))
    );
}

#[test]
fn std_http_import_adds_core_function_declaration() {
    let src = "import std.http;\ncapability network.http;\nfn main() -> String { return http-get(\"https://example.com\"); }\n";
    let module = parse_in_source(src).expect("std http import");
    let decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "http-get" => Some((params, ret)),
        _ => None,
    });
    let (params, ret) = decl.expect("http-get");
    assert_eq!(params, &vec![("url".to_string(), Typ::String)]);
    assert_eq!(ret, &Typ::String);
}

#[test]
fn std_fs_import_adds_runtime_function_declarations() {
    let src = "import std.fs;\ncapability fs.read;\ncapability fs.write;\nfn main() -> Bool { return write-file(\"/tmp/a\", \"b\"); }\n";
    let module = parse_in_source(src).expect("std fs import");
    let read_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "read-file" => Some((params, ret)),
        _ => None,
    });
    let write_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "write-file" => Some((params, ret)),
        _ => None,
    });
    let (read_params, read_ret) = read_decl.expect("read-file");
    let (write_params, write_ret) = write_decl.expect("write-file");
    assert_eq!(read_params, &vec![("path".to_string(), Typ::String)]);
    assert_eq!(read_ret, &Typ::String);
    assert_eq!(
        write_params,
        &vec![
            ("path".to_string(), Typ::String),
            ("text".to_string(), Typ::String)
        ]
    );
    assert_eq!(write_ret, &Typ::Bool);
}

#[test]
fn std_json_import_adds_core_function_declarations() {
    let src = "import std.json;\nfn main() -> String { return json-parse(\"{}\"); }\n";
    let module = parse_in_source(src).expect("std json import");
    let parse_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "json-parse" => Some((params, ret)),
        _ => None,
    });
    let stringify_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "json-stringify" => Some((params, ret)),
        _ => None,
    });
    let (parse_params, parse_ret) = parse_decl.expect("json-parse");
    let (stringify_params, stringify_ret) = stringify_decl.expect("json-stringify");
    assert_eq!(parse_params, &vec![("text".to_string(), Typ::String)]);
    assert_eq!(parse_ret, &Typ::String);
    assert_eq!(stringify_params, &vec![("text".to_string(), Typ::String)]);
    assert_eq!(stringify_ret, &Typ::String);
}

#[test]
fn std_process_import_adds_core_function_declaration() {
    let src = "import std.process;\ncapability process.spawn;\nfn main() -> String { return process-run(\"pwd\"); }\n";
    let module = parse_in_source(src).expect("std process import");
    let decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "process-run" => Some((params, ret)),
        _ => None,
    });
    let (params, ret) = decl.expect("process-run");
    assert_eq!(params, &vec![("command".to_string(), Typ::String)]);
    assert_eq!(ret, &Typ::String);
}

#[test]
fn std_cli_import_adds_core_function_declarations() {
    let src = "import std.cli;\ncapability process.args;\nfn main() -> String { return arg(0); }\n";
    let module = parse_in_source(src).expect("std cli import");
    let count_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "arg-count" => Some((params, ret)),
        _ => None,
    });
    let arg_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "arg" => Some((params, ret)),
        _ => None,
    });
    let (count_params, count_ret) = count_decl.expect("arg-count");
    let (arg_params, arg_ret) = arg_decl.expect("arg");
    assert_eq!(count_params, &Vec::<(String, Typ)>::new());
    assert_eq!(count_ret, &Typ::Int);
    assert_eq!(arg_params, &vec![("index".to_string(), Typ::Int)]);
    assert_eq!(arg_ret, &Typ::String);
}

#[test]
fn std_env_import_adds_core_function_declarations() {
    let src =
        "import std.env;\ncapability env.read;\nfn main() -> Bool { return env-has(\"HOME\"); }\n";
    let module = parse_in_source(src).expect("std env import");
    let get_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "env-get" => Some((params, ret)),
        _ => None,
    });
    let set_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "env-set" => Some((params, ret)),
        _ => None,
    });
    let has_decl = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == "env-has" => Some((params, ret)),
        _ => None,
    });
    let (get_params, get_ret) = get_decl.expect("env-get");
    let (set_params, set_ret) = set_decl.expect("env-set");
    let (has_params, has_ret) = has_decl.expect("env-has");
    assert_eq!(get_params, &vec![("name".to_string(), Typ::String)]);
    assert_eq!(get_ret, &Typ::String);
    assert_eq!(
        set_params,
        &vec![
            ("name".to_string(), Typ::String),
            ("value".to_string(), Typ::String)
        ]
    );
    assert_eq!(set_ret, &Typ::Void);
    assert_eq!(has_params, &vec![("name".to_string(), Typ::String)]);
    assert_eq!(has_ret, &Typ::Bool);
}

#[test]
fn std_env_import_declares_capability_requirements() {
    let bindings = in_standard_import_bindings("std.env");
    assert_eq!(
        bindings
            .iter()
            .map(|binding| (
                binding.name.as_str(),
                binding.required_capabilities.as_slice()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("env-get", &["env.read".to_string()][..]),
            ("env-set", &["env.write".to_string()][..]),
            ("env-has", &["env.read".to_string()][..]),
            ("env-temp-dir", &["env.read".to_string()][..]),
            ("env-current-dir", &["env.read".to_string()][..])
        ]
    );
}

#[test]
fn std_path_import_adds_core_function_declarations() {
    let src = "import std.path;\nfn main() -> String { return path-join(\"/tmp\", \"app\"); }\n";
    let module = parse_in_source(src).expect("std path import");
    let expected = [
        (
            "path-join",
            vec![
                ("base".to_string(), Typ::String),
                ("child".to_string(), Typ::String),
            ],
        ),
        ("path-dirname", vec![("path".to_string(), Typ::String)]),
        ("path-basename", vec![("path".to_string(), Typ::String)]),
        ("path-extname", vec![("path".to_string(), Typ::String)]),
        ("path-normalize", vec![("path".to_string(), Typ::String)]),
    ];
    for (expected_name, expected_params) in expected {
        let decl = module.decls.iter().find_map(|decl| match decl {
            Decl::Function {
                name, params, ret, ..
            } if name == expected_name => Some((params, ret)),
            _ => None,
        });
        let (params, ret) = decl.expect(expected_name);
        assert_eq!(params, &expected_params);
        assert_eq!(ret, &Typ::String);
    }
}

#[test]
fn fn_body_let_and_return() {
    use crate::core_ir::Expr;
    let src = r#"
fn bump() -> Int {
  let x: Int = 1;
  return x;
}
fn main() -> void { return; }
"#;
    let m = parse_in_source(src).expect("ok");
    let bump = m.decls.iter().find_map(|d| match d {
        Decl::Function { name, body, .. } if name == "bump" => Some(body.clone()),
        _ => None,
    });
    let body = bump.expect("bump");
    assert_eq!(body.len(), 2);
    assert!(
        matches!(&body[0], Stmt::Let(n, Some(Typ::Int), Expr::IntLit(1)) if n == "x"),
        "{body:?}"
    );
    assert!(
        matches!(&body[1], Stmt::Return(Some(Expr::Ident(x))) if x == "x"),
        "{body:?}"
    );
}

#[test]
fn fn_body_accepts_newline_separated_statements_without_semicolons() {
    let src = r#"
fn main() -> void {
  let seed: Int = 0
  seed = 1
  return
}
"#;
    let module = parse_in_source(src).expect("parse");
    let body = module
        .decls
        .iter()
        .find_map(|decl| match decl {
            Decl::Function { name, body, .. } if name == "main" => Some(body),
            _ => None,
        })
        .expect("main body");
    assert_eq!(body.len(), 3);
}

#[test]
fn fn_body_infers_let_without_type() {
    use crate::core_ir::Expr;
    let src = "fn f() -> void { let n = 0; return; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("f"),
    };
    assert!(matches!(&body[0], Stmt::Let(name, None, Expr::IntLit(0)) if name == "n"));
}

#[test]
fn expr_statement_parsed() {
    use crate::core_ir::Expr;
    let src = "fn g() -> void { 42; return; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "g"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("g"),
    };
    assert!(matches!(&body[0], Stmt::Expr(Expr::IntLit(42))));
}

#[test]
fn fn_body_assignment_and_call_expr() {
    use crate::core_ir::Expr;
    let src = "fn f() -> void { let n = 0; n = add(n, 1); return; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("f"),
    };
    assert!(matches!(
        &body[1],
        Stmt::Assign(name, Expr::Call { callee, args, ..})
            if name == "n"
                && matches!(callee.as_ref(), Expr::Ident(c) if c == "add")
                && args.len() == 2
    ));
}

#[test]
fn fn_body_parses_index_assignment() {
    use crate::core_ir::Expr;
    let src =
        "fn f() -> Int { let xs: [Int] = [1, 2]; xs[1] = 9; return xs[1]; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("f"),
    };
    assert!(matches!(
        &body[1],
        Stmt::IndexAssign {
            base,
            index,
            value
        } if matches!(base, Expr::Ident(name) if name == "xs")
            && matches!(index, Expr::IntLit(1))
            && matches!(value, Expr::IntLit(9))
    ));
}

#[test]
fn fn_body_parses_nested_index() {
    use crate::core_ir::Expr;
    let src = "fn f() -> Int { let xs: [Int] = [1, 2]; let ys: [Int] = [0, 1]; return xs[ys[1]]; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("f"),
    };
    assert!(matches!(
        &body[2],
        Stmt::Return(Some(Expr::Index { base, index }))
            if matches!(base.as_ref(), Expr::Ident(name) if name == "xs")
            && matches!(index.as_ref(), Expr::Index { base: inner, index: inner_idx }
                if matches!(inner.as_ref(), Expr::Ident(name) if name == "ys")
                && matches!(inner_idx.as_ref(), Expr::IntLit(1))
            )
    ));
}

#[test]
fn parse_expr_prefers_longest_comparison_operator() {
    use crate::core_ir::Expr;
    let parsed = parse_expr("n <= 1");
    match &parsed {
        Expr::Binary { op, .. } => assert_eq!(op, "<="),
        other => panic!("expected <=, got {other:?}"),
    }
}

#[test]
fn fn_body_parses_binary_expression() {
    use crate::core_ir::Expr;
    let src = "fn f() -> Int { return 1 + 2 * 3; }\nfn main() -> void\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("f"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Return(Some(Expr::Binary { op, .. })) if op == "+"
    ));
}

#[test]
fn fn_body_parses_modulo_at_multiplicative_precedence() {
    use crate::core_ir::Expr;
    let src = "fn main() -> Int { return 7 % 4; }\n";
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("main"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Return(Some(Expr::Binary { op, lhs, rhs, ..}))
            if op == "%"
                && matches!(lhs.as_ref(), Expr::IntLit(7))
                && matches!(rhs.as_ref(), Expr::IntLit(4))
    ));
}

#[test]
fn fn_body_parses_unary_and_parenthesized_expression() {
    use crate::core_ir::Expr;
    let src = r#"
fn negate(flag: Bool, value: Int) -> Int {
  if !flag == false {
    return -(value + 1);
  }
  return (value);
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "negate"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("negate"),
    };
    assert!(matches!(
        &body[0],
        Stmt::If {
            cond: Expr::Binary { lhs, op, .. },
            then_body,
            ..
        } if op == "=="
            && matches!(lhs.as_ref(), Expr::Unary { op, .. } if op == "!")
            && matches!(then_body.as_slice(), [Stmt::Return(Some(Expr::Unary { op, expr, ..}))] if op == "-" && matches!(expr.as_ref(), Expr::Binary { op, .. } if op == "+"))
    ));
    assert!(matches!(
        &body[1],
        Stmt::Return(Some(Expr::Ident(name))) if name == "value"
    ));
}

#[test]
fn fn_body_parses_logical_binary_precedence() {
    use crate::core_ir::Expr;
    let src = r#"
fn choose(a: Bool, b: Bool, n: Int) -> Int {
  if a || b && n == 1 {
    return 1;
  }
  return 0;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "choose"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("choose"),
    };
    assert!(matches!(
        &body[0],
        Stmt::If {
            cond: Expr::Binary { op, lhs, rhs, ..},
            ..
        } if op == "||"
            && matches!(lhs.as_ref(), Expr::Ident(name) if name == "a")
            && matches!(rhs.as_ref(), Expr::Binary { op, rhs, .. } if op == "&&" && matches!(rhs.as_ref(), Expr::Binary { op, .. } if op == "=="))
    ));
}

#[test]
fn fn_body_parses_if_else() {
    use crate::core_ir::Expr;
    let src = r#"
fn label(flag: Bool) -> String {
  if flag == true {
    return "yes";
  } else {
    return "no";
  }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "label"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("label"),
    };
    assert!(matches!(
        &body[0],
        Stmt::If {
            cond: Expr::Binary { op, .. },
            then_body,
            else_body
        } if op == "==" && then_body.len() == 1 && else_body.len() == 1
    ));
}

#[test]
fn fn_body_parses_else_if_as_nested_if() {
    use crate::core_ir::Expr;
    let src = r#"
fn classify(n: Int) -> Int {
  if n == 0 {
    return 0;
  } else if n == 1 {
    return 1;
  } else {
    return 2;
  }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "classify"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("classify"),
    };
    assert!(matches!(
        &body[0],
        Stmt::If {
            cond: Expr::Binary { op, .. },
            else_body,
            ..
        } if op == "==" && matches!(
            else_body.as_slice(),
            [Stmt::If {
                cond: Expr::Binary { op, .. },
                then_body,
                else_body,
            }] if op == "==" && then_body.len() == 1 && else_body.len() == 1
        )
    ));
}

#[test]
fn fn_body_parses_while_loop() {
    let src = r#"
fn spin() -> void {
  let n = 0;
  while n < 1 {
    n = n + 1;
  }
  return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "spin"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("spin"),
    };
    assert!(matches!(
        &body[1],
        Stmt::Loop {
            kind: LoopKind::While,
            ..
        }
    ));
}

#[test]
fn fn_body_parses_match_statement() {
    let src = r#"
fn choose(tag: Int) -> Int {
  let out = 0;
  match tag {
    1 {
      out = 10;
    }
    _ {
      out = 20;
    }
  }
  return out;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "choose"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("choose"),
    };
    assert!(matches!(
        &body[1],
        Stmt::Match { scrutinee, arms }
            if matches!(scrutinee, Expr::Ident(name) if name == "tag")
                &&             arms.len() == 2
                && arms[0].pattern == "1"
                && arms[1].pattern == "_"
    ));
}

#[test]
fn fn_body_parses_throw_statement() {
    let src = r#"
fn fail(msg: String) -> void {
  throw msg;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "fail"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("fail"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Throw(Expr::Ident(name)) if name == "msg"
    ));
}

#[test]
fn fn_body_parses_try_catch_statement() {
    let src = r#"
fn protect() -> void {
  try {
    let n = 0;
    n = n + 1;
  } catch e {
    n = 0;
  }
  return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "protect"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("protect"),
    };
    assert!(matches!(&body[0], Stmt::Try { body: try_body, catches }
            if try_body.len() == 2 && catches.len() == 1 && catches[0].pattern == "e"));
}

#[test]
fn fn_body_parses_try_with_multiple_catch_arms() {
    let src = r#"
fn handler() -> void {
  try {
    let n = 0;
  } catch e {
    n = 1;
  } catch _ {
    n = 2;
  }
  return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "handler"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("handler"),
    };
    assert!(matches!(&body[0], Stmt::Try { catches, .. }
            if catches.len() == 2 && catches[0].pattern == "e" && catches[1].pattern == "_"));
}

#[test]
fn fn_body_parses_closure_expression() {
    let src = r#"
fn main() -> void {
  let add = fn(a: Int, b: Int) -> Int { return a + b; };
  return;
}
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("main"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Let(name, None, Expr::Closure { params, ret, body: closure_body, .. })
            if name == "add"
                && params.len() == 2
                && matches!(ret, Typ::Int)
                && closure_body.len() == 1
    ));
}

#[test]
fn closure_without_return_type_defaults_to_void() {
    let src = r#"
fn main() -> void {
  let f = fn() { return; };
  return;
}
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("main"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Let(name, None, Expr::Closure { ret, .. })
            if name == "f" && matches!(ret, Typ::Void)
    ));
}

#[test]
fn throw_without_expression_rejected() {
    let src = "fn f() -> void { throw; return; }\nfn main() -> void\n";
    let err = parse_in_source(src).expect_err("throw without expr");
    assert!(err.contains("throw"), "{err}");
}

#[test]
fn try_without_catch_body_rejected() {
    let src = "fn f() -> void { try { return; } catch { } return; }\nfn main() -> void\n";
    let err = parse_in_source(src).expect_err("catch without pattern");
    assert!(err.contains("catch"), "{err}");
}

#[test]
fn parse_throw_expr() {
    let src = r#"
fn fail() -> void {
    throw "something went wrong";
    return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "fail"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("fail"),
    };
    assert!(matches!(
        &body[0],
        Stmt::Throw(Expr::StringLit(s)) if s == "something went wrong"
    ));
}

#[test]
fn parse_try_catch() {
    let src = r#"
fn handle() -> void {
    try {
        throw "bad";
    } catch (e) {
        return;
    }
    return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "handle"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("handle"),
    };
    assert!(matches!(&body[0], Stmt::Try { body: try_body, catches }
            if try_body.len() == 1 && catches.len() == 1 && catches[0].pattern == "e"));
}

#[test]
fn parse_try_with_multiple_stmts() {
    let src = r#"
fn protect() -> void {
    try {
        let a = 1;
        let b = 2;
        let c = a + b;
    } catch (e) {
        let a = 0;
    }
    return;
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("ok");
    let body = match m
        .decls
        .iter()
        .find(|d| matches!(d, Decl::Function { name, .. } if name == "protect"))
    {
        Some(Decl::Function { body, .. }) => body,
        _ => panic!("protect"),
    };
    assert!(matches!(&body[0], Stmt::Try { body: try_body, catches }
            if try_body.len() == 3 && catches.len() == 1));
}

#[test]
fn parse_class_with_field_and_method() {
    let src = r#"
class Dog {
    name: String
    age: Int

    fn bark() -> String {
        return "woof";
    }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("class parse");
    let class = m.decls.iter().find_map(|d| match d {
        Decl::Class {
            name,
            fields,
            methods,
            ..
        } if name == "Dog" => Some((fields.clone(), methods.clone())),
        _ => None,
    });
    let (fields, methods) = class.expect("Dog class");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0], ("name".into(), Typ::String));
    assert_eq!(fields[1], ("age".into(), Typ::Int));
    assert_eq!(methods.len(), 1);
    match &methods[0] {
        Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } => {
            assert_eq!(name, "bark");
            assert!(params.is_empty());
            assert_eq!(ret, &Typ::String);
            assert_eq!(body.len(), 1);
        }
        _ => panic!("expected function method"),
    }
}

#[test]
fn parse_class_with_extends() {
    let src = r#"
class Dog {
}
class Poodle extends Dog {
    fn bark() -> String {
        return "yap";
    }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("class extends parse");
    let ext = m.decls.iter().find_map(|d| match d {
        Decl::Class { name, extends, .. } if name == "Poodle" => Some(extends.clone()),
        _ => None,
    });
    assert_eq!(ext, Some(Some("Dog".into())));
}

#[test]
fn parse_class_with_implements() {
    let src = r#"
interface Speaker {
    fn speak() -> String
}
interface Listener {
}
class Human implements Speaker, Listener {
    fn speak() -> String {
        return "hello";
    }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("class implements parse");
    let impls = m.decls.iter().find_map(|d| match d {
        Decl::Class {
            name, implements, ..
        } if name == "Human" => Some(implements.clone()),
        _ => None,
    });
    assert_eq!(impls, Some(vec!["Speaker".into(), "Listener".into()]));
}

#[test]
fn parse_interface_with_method_sigs() {
    let src = r#"
interface Animal {
    fn speak() -> String
    fn eat(food: String) -> Int
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("interface parse");
    let sigs = m.decls.iter().find_map(|d| match d {
        Decl::Interface { name, methods, .. } if name == "Animal" => Some(methods.clone()),
        _ => None,
    });
    let methods = sigs.expect("Animal interface");
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].name, "speak");
    assert_eq!(methods[0].params, vec![]);
    assert_eq!(methods[0].ret, Typ::String);
    assert_eq!(methods[1].name, "eat");
    assert_eq!(methods[1].params, vec![("food".into(), Typ::String)]);
    assert_eq!(methods[1].ret, Typ::Int);
}

#[test]
fn parse_class_empty_body() {
    let src = r#"
class Empty {
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("empty class");
    let info = m.decls.iter().find_map(|d| match d {
        Decl::Class {
            name,
            fields,
            methods,
            ..
        } if name == "Empty" => Some((fields.clone(), methods.clone())),
        _ => None,
    });
    let (fields, methods) = info.expect("Empty class");
    assert!(fields.is_empty());
    assert!(methods.is_empty());
}

#[test]
fn parse_class_multiple_fields() {
    let src = r#"
class Point {
    x: Int
    y: Int
    z: Int
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("multi field class");
    let fields = m.decls.iter().find_map(|d| match d {
        Decl::Class { name, fields, .. } if name == "Point" => Some(fields.clone()),
        _ => None,
    });
    let fields = fields.expect("Point class");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0], ("x".into(), Typ::Int));
    assert_eq!(fields[1], ("y".into(), Typ::Int));
    assert_eq!(fields[2], ("z".into(), Typ::Int));
}

#[test]
fn class_with_extends_and_implements() {
    let src = r#"
class BaseWidget {
}
interface Drawable {
    fn draw() -> void
}
interface Clickable {
}
class MyWidget extends BaseWidget implements Drawable, Clickable {
    fn draw() -> void {
        return;
    }
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("extends+implements parse");
    let info = m.decls.iter().find_map(|d| match d {
        Decl::Class {
            name,
            extends,
            implements,
            ..
        } if name == "MyWidget" => Some((extends.clone(), implements.clone())),
        _ => None,
    });
    let (extends, implements) = info.expect("MyWidget class");
    assert_eq!(extends, Some("BaseWidget".into()));
    assert_eq!(
        implements,
        vec!["Drawable".to_string(), "Clickable".to_string()]
    );
}

#[test]
fn class_extends_unknown_parent_is_rejected() {
    let src = r#"
class Poodle extends Dog {
}
fn main() -> void
"#;
    let err = parse_in_source(src).expect_err("unknown parent");
    assert!(err.contains("extends unknown class `Dog`"), "{err}");
}

#[test]
fn class_implements_unknown_interface_is_rejected() {
    let src = r#"
class Human implements Speaker {
}
fn main() -> void
"#;
    let err = parse_in_source(src).expect_err("unknown interface");
    assert!(
        err.contains("implements unknown interface `Speaker`"),
        "{err}"
    );
}

#[test]
fn class_missing_interface_method_is_rejected() {
    let src = r#"
interface Speaker {
    fn speak() -> String
}
class Human implements Speaker {
}
fn main() -> void
"#;
    let err = parse_in_source(src).expect_err("missing interface method");
    assert!(err.contains("does not implement `Speaker.speak`"), "{err}");
}

#[test]
fn parse_class_struct_init_accepts_class_name() {
    let src = r#"
class Dog {
    name: String
}
fn main() -> String {
    let d = Dog { name: "Rex" };
    return d.name;
}
"#;
    let m = parse_in_source(src).expect("class init");
    assert!(
        m.decls
            .iter()
            .any(|d| matches!(d, Decl::Class { name, .. } if name == "Dog"))
    );
}

#[test]
fn class_name_duplicate_with_struct_is_rejected() {
    let src = r#"
class Dog {
    name: String
}
struct Dog { Int x }
fn main() -> void
"#;
    let err = parse_in_source(src).expect_err("class+struct dup");
    assert!(err.contains("duplicate"), "{err}");
}

#[test]
fn interface_accepts_empty_body() {
    let src = r#"
interface Marker {
}
fn main() -> void
"#;
    let m = parse_in_source(src).expect("empty interface");
    assert!(
        m.decls
            .iter()
            .any(|d| matches!(d, Decl::Interface { name, .. } if name == "Marker"))
    );
}

#[test]
fn parse_struct_pattern_shorthand_and_literal() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("Point { x, y: 0 }").expect("parse struct pattern");
    assert_eq!(
        pat,
        MatchPattern::StructPat {
            name: "Point".into(),
            fields: vec![
                ("x".into(), MatchPattern::IdentPat("x".into())),
                ("y".into(), MatchPattern::IntPat(0)),
            ],
        }
    );
}

#[test]
fn parse_struct_pattern_wild_field() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("Point { x: _, y }").expect("parse struct with wild field");
    assert_eq!(
        pat,
        MatchPattern::StructPat {
            name: "Point".into(),
            fields: vec![
                ("x".into(), MatchPattern::WildPat),
                ("y".into(), MatchPattern::IdentPat("y".into())),
            ],
        }
    );
}

#[test]
fn parse_tuple_pattern() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("(1, 2, 3)").expect("parse tuple pattern");
    assert_eq!(
        pat,
        MatchPattern::TuplePat(vec![
            MatchPattern::IntPat(1),
            MatchPattern::IntPat(2),
            MatchPattern::IntPat(3),
        ])
    );
}

#[test]
fn parse_tuple_pattern_with_wild() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("(x, _)").expect("parse tuple wild pattern");
    assert_eq!(
        pat,
        MatchPattern::TuplePat(vec![
            MatchPattern::IdentPat("x".into()),
            MatchPattern::WildPat,
        ])
    );
}

#[test]
fn parse_array_pattern() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("[a, b, ..]").expect("parse array pattern");
    assert_eq!(
        pat,
        MatchPattern::ArrayPat(vec![
            MatchPattern::IdentPat("a".into()),
            MatchPattern::IdentPat("b".into()),
            MatchPattern::RestPat,
        ])
    );
}

#[test]
fn parse_array_pattern_literals() {
    use crate::core_ir::MatchPattern;
    let pat = MatchPattern::parse("[1, 2, 3]").expect("parse array literal pattern");
    assert_eq!(
        pat,
        MatchPattern::ArrayPat(vec![
            MatchPattern::IntPat(1),
            MatchPattern::IntPat(2),
            MatchPattern::IntPat(3),
        ])
    );
}

#[test]
fn parse_match_pattern_literals() {
    use crate::core_ir::MatchPattern;
    assert_eq!(MatchPattern::parse("42").unwrap(), MatchPattern::IntPat(42));
    assert_eq!(
        MatchPattern::parse("\"hello\"").unwrap(),
        MatchPattern::StringPat("hello".into())
    );
    assert_eq!(
        MatchPattern::parse("true").unwrap(),
        MatchPattern::BoolPat(true)
    );
    assert_eq!(
        MatchPattern::parse("false").unwrap(),
        MatchPattern::BoolPat(false)
    );
    assert_eq!(MatchPattern::parse("_").unwrap(), MatchPattern::WildPat);
    assert_eq!(MatchPattern::parse("else").unwrap(), MatchPattern::WildPat);
    assert_eq!(MatchPattern::parse("..").unwrap(), MatchPattern::RestPat);
    assert_eq!(
        MatchPattern::parse("my-var").unwrap(),
        MatchPattern::IdentPat("my-var".into())
    );
}

#[test]
fn parse_component_declaration() {
    let src = r#"
component TestComp {
  target "x86_64"
  deterministic true
  checkpoint full

  import dep: DepInterface
  export api: PubInterface
  capability log: DebugConsole(write)
}

interface DepInterface {
  fn helper(x: Int) -> String
}

interface PubInterface {
  fn run() -> Int
}

fn main() -> void {}
"#;
    let module = parse_in_source(src).expect("component should parse");

    let comp = module.decls.iter().find_map(|d| match d {
        Decl::Component { name, .. } if name == "TestComp" => Some(d),
        _ => None,
    });
    assert!(comp.is_some(), "expected TestComp component");

    if let Decl::Component {
        target,
        deterministic,
        checkpoint,
        imports,
        exports,
        capabilities,
        ..
    } = comp.unwrap()
    {
        assert_eq!(target, "x86_64");
        assert!(deterministic);
        assert_eq!(checkpoint, "full");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "dep");
        assert_eq!(imports[0].interface, "DepInterface");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name, "api");
        assert_eq!(exports[0].interface, "PubInterface");
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].name, "log");
        assert_eq!(capabilities[0].capability_type, "DebugConsole");
        assert_eq!(capabilities[0].args, vec!["write"]);
    } else {
        panic!("expected Component variant");
    }
}

#[test]
fn parse_capability_declaration() {
    // Test capability with multiple args
    let src = r#"
component MultiCap {
  target "x86_64"
  deterministic false
  checkpoint none

  capability mem: PhysicalMemory(discover, map, protect)
  capability caps: CapabilityTable(create, mint)
}

fn main() -> void {}
"#;
    let module = parse_in_source(src).expect("multi-cap component should parse");

    let comp = module.decls.iter().find_map(|d| match d {
        Decl::Component { name, .. } if name == "MultiCap" => Some(d),
        _ => None,
    });
    assert!(comp.is_some(), "expected MultiCap component");

    if let Decl::Component { capabilities, .. } = comp.unwrap() {
        assert_eq!(capabilities.len(), 2);

        assert_eq!(capabilities[0].name, "mem");
        assert_eq!(capabilities[0].capability_type, "PhysicalMemory");
        assert_eq!(capabilities[0].args, vec!["discover", "map", "protect"]);

        assert_eq!(capabilities[1].name, "caps");
        assert_eq!(capabilities[1].capability_type, "CapabilityTable");
        assert_eq!(capabilities[1].args, vec!["create", "mint"]);
    } else {
        panic!("expected Component variant");
    }

    // Test rejection of unknown component field
    let bad_src = r#"
component BadField {
  target "x86_64"
  deterministic true
  checkpoint none

  unknown-field foo
}

fn main() -> void {}
"#;
    let err = parse_in_source(bad_src).expect_err("unknown field should fail");
    assert!(
        err.contains("unknown component field"),
        "unexpected error: {err}"
    );
}
