#![cfg(all(target_arch = "aarch64", target_os = "macos"))]

use inauguration::{
    compiler::mir_lower::lower_boot_image,
    core_ir::Decl,
    in_lang_parse::parse_in_source,
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, NativeObjectRequest, emit_native_object, lower},
};

const TWO_GLOBALS: &str = r#"
struct Pair {
  Int left
  Int right
}
var first: Pair = 0;
var second: Pair = 0;
fn bump() -> Int {
  first.left = first.left + 4;
  second.right = second.right + first.left;
  return second.right;
}
fn main() -> Int {
  first.left = 3;
  first.right = 99;
  second.left = 40;
  second.right = 1;
  return bump() + first.right + second.left;
}
"#;

#[test]
fn jit_struct_globals_are_distinct_writable_and_shared_across_calls() {
    let module = parse_in_source(TWO_GLOBALS).expect("parse two globals");
    let lowered =
        lower::lower_module(&module, "main", NativeLinkage::Executable).expect("lower two globals");
    assert!(
        lowered.degradations.is_empty(),
        "{:?}",
        lowered.degradations
    );
    assert_eq!(lowered.data.len(), 32);
    assert!(lowered.global_relocs.len() >= 8);
    let functions = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, lowered.code.len() as u32 - *offset))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load_with_globals(
            &lowered.code,
            &functions,
            &lowered.relocations,
            &lowered.data,
            &lowered.global_relocs,
        )
        .expect("load global data");
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(147));
    assert_eq!(unsafe { runtime.invoke("bump", &[]) }, Some(19));
}

#[test]
fn non_jit_artifacts_reject_writable_struct_globals() {
    let module = parse_in_source(TWO_GLOBALS).expect("parse two globals");
    let error = lower::lower_module(&module, "main", NativeLinkage::StaticLib)
        .expect_err("static library has no writable-data loader");
    assert!(error.contains("JIT writable-data loader"), "{error}");
    let error = lower_boot_image(&module, "main", Some("aarch64-unknown-none"))
        .expect_err("boot image cannot discard the global data section");
    assert!(error.contains("writable AArch64 globals"), "{error}");
    let path = std::env::temp_dir().join(format!(
        "inauguration-global-reject-{}.bin",
        std::process::id()
    ));
    let error = lower::compile_native_artifact_with_report(
        &module,
        "App",
        "main",
        NativeLinkage::Executable,
        &path,
    )
    .expect_err("native artifact cannot use the JIT-only writable-data loader");
    assert!(error.contains("JIT writable-data loader"), "{error}");
    assert!(!path.exists(), "unsupported native artifact wrote output");
    let jit = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("failed native lowering must reset native-mode state");
    assert_eq!(jit.data.len(), 32);
}

#[test]
fn unsupported_global_initializer_is_rejected_when_read() {
    let module = parse_in_source(
        r#"
struct Counter { Int count }
var counter: Counter = 42;
fn main() -> Int { return counter.count; }
"#,
    )
    .expect("parse initializer fixture");
    let error = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect_err("unsupported initializer must not silently read as zero");
    assert!(
        error.contains("unknown field `count` on struct global `counter`"),
        "{error}"
    );
}

#[test]
fn aarch64_object_paths_reject_mutable_globals() {
    let module = parse_in_source(TWO_GLOBALS).expect("parse two globals");
    for (target_triple, linkage) in [
        ("aarch64-unknown-none", NativeLinkage::StaticLib),
        ("aarch64-unknown-linux-gnu", NativeLinkage::StaticLib),
        ("aarch64-unknown-linux-gnu", NativeLinkage::Executable),
        ("aarch64-apple-darwin", NativeLinkage::StaticLib),
    ] {
        let request = NativeObjectRequest {
            target_triple,
            linkage,
            entry: "main",
            exit_code: 0,
            module: &module,
            module_id: "App",
            base: None,
        };
        assert!(
            emit_native_object(&request).is_none(),
            "{target_triple} {linkage:?} accepted missing global data"
        );
    }
}

#[test]
fn unsupported_scalar_global_read_fails_instead_of_returning_zero() {
    let module = parse_in_source("var count: Int = 42;\nfn main() -> Int { return count; }\n")
        .expect("parse scalar global");
    let error = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect_err("scalar global must not silently lower as zero");
    assert!(error.contains("direct access to global `count`"), "{error}");
}

#[test]
fn local_shadow_blocks_global_field_lookup() {
    let module = parse_in_source(
        r#"
struct Pair { Int left }
var first: Pair = 0;
fn main() -> Int {
  let first: [Int] = [7];
  return first.left;
}
"#,
    )
    .expect("parse shadowing fixture");
    let error = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect_err("array local must not resolve to global struct field");
    assert!(error.contains("not a struct local"), "{error}");
}

#[test]
fn scalar_parameter_shadow_blocks_global_field_reads_and_writes() {
    for (body, expected_error) in [
        (
            "return first.left;",
            "field access base `first` is a scalar parameter",
        ),
        (
            "first.left = 5; return first;",
            "field assignment base `first` is a scalar parameter",
        ),
    ] {
        let source = format!(
            "struct Pair {{ Int left }}\nvar first: Pair = 0;\nfn probe(first: Int) -> Int {{ {body} }}\nfn main() -> Int {{ return probe(3); }}\n"
        );
        let module = parse_in_source(&source).expect("parse scalar parameter shadow");
        let error = lower::lower_module(&module, "probe", NativeLinkage::Executable)
            .expect_err("parameter field must not resolve to same-named global");
        assert!(error.contains(expected_error), "{error}");
    }
}

#[test]
fn unsupported_mutable_global_cannot_enter_native_or_boot_image() {
    let module = parse_in_source(
        "struct Counter { Int count }\nvar counter: Counter = 42;\nfn main() -> Int { return 0; }\n",
    )
    .expect("parse unsupported initializer");
    let native = lower::lower_module(&module, "main", NativeLinkage::StaticLib)
        .expect_err("native artifact cannot discard even unused mutable global");
    assert!(native.contains("JIT writable-data loader"), "{native}");
    let boot = lower_boot_image(&module, "main", Some("aarch64-unknown-none"))
        .expect_err("boot image cannot discard unallocated mutable global");
    assert!(boot.contains("writable AArch64 globals"), "{boot}");
}
#[test]
fn immutable_global_field_assignment_is_rejected() {
    let mut module = parse_in_source(
        r#"
struct Counter { Int count }
var counter: Counter = 0;
fn main() -> Int { counter.count = 5; return counter.count; }
"#,
    )
    .expect("parse immutable global fixture");
    let global = module
        .decls
        .iter_mut()
        .find(|decl| matches!(decl, Decl::Global { name, .. } if name == "counter"))
        .expect("parsed global");
    let Decl::Global { mutable, .. } = global else {
        unreachable!()
    };
    *mutable = false;
    let error = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect_err("immutable global field write must not compile");
    assert!(
        error.contains("cannot assign to immutable global"),
        "{error}"
    );
}
