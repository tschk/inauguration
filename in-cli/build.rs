fn main() {
    println!("cargo:rerun-if-changed=grammars/nim/src/parser.c");
    println!("cargo:rerun-if-changed=grammars/nim/src/tree_sitter/parser.h");
    cc::Build::new()
        .include("grammars/nim/src")
        .file("grammars/nim/src/parser.c")
        .compile("tree-sitter-nim");
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let schema = manifest_dir.join("../../shared/protocol/events.schema.json");
    if schema.exists() {
        println!("cargo:rerun-if-changed={}", schema.display());
    }
}
