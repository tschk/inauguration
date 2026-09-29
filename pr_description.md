🎯 **What:** Replaced the `#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]` workaround with conditional compilation (`#[cfg(feature = "parse-extended")]`) for extended parser modules in `in-cli/src/compiler/tree_front/mod.rs`.

💡 **Why:** When the `parse-extended` feature was disabled, several language modules were being unconditionally compiled, resulting in legitimate dead code since their extraction functions were never called. Suppressing these warnings with `allow(dead_code)` hides the issue and compiles unnecessary code. Conditionally compiling these modules entirely removes the dead code, adheres to Rust idioms, and slightly improves compile times when the feature is off.

✅ **Verification:**
1. Ran `cargo clippy --all-targets --locked -- -D warnings` (without feature) and verified that the previously unused modules are no longer compiled and no warnings are emitted.
2. Ran `cargo clippy --all-targets --features parse-extended --locked -- -D warnings` and verified that the modules compile cleanly when the feature is enabled.
3. Ran `cargo test` in `in-cli` to ensure no functionality or tests were broken.

✨ **Result:** The codebase is cleaner, idiomatic conditional compilation is used, and the dead code allowance is removed without introducing new warnings.
