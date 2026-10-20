cd in-cli
cargo build --release --bin in --features extended
./target/release/in build --path src/main.rs --module-id InCli --verbose 2>&1 | grep "jit-lowering-failed"
