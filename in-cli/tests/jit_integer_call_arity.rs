//! Direct JIT calls must preserve every integer argument used by a loaded function.
#![cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]

use inauguration::{in_lang_parse::parse_in_source, jit_runtime::JitRuntime};

const SOURCE: &str = r#"
fn two(a: Int, b: Int) -> Int { return a - b; }
fn four(a: Int, b: Int, c: Int, d: Int) -> Int { return d; }
fn five(a: Int, b: Int, c: Int, d: Int, e: Int) -> Int { return e; }
fn six(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int) -> Int {
  return a + b + c + d + e + f;
}
fn main() -> Int { return 0; }
"#;

fn loaded_runtime() -> JitRuntime {
    let module = parse_in_source(SOURCE).expect("parse integer functions");
    let mut runtime = JitRuntime::new();

    #[cfg(target_arch = "aarch64")]
    {
        use inauguration::native_emit::{NativeLinkage, lower};
        let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
            .expect("lower integer functions");
        assert!(
            lowered.degradations.is_empty(),
            "{:?}",
            lowered.degradations
        );
        let functions = lowered
            .function_offsets
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, lowered.code.len() as u32 - *offset))
            .collect::<Vec<_>>();
        runtime
            .load(&lowered.code, &functions, &lowered.relocations)
            .expect("load integer functions");
    }

    #[cfg(target_arch = "x86_64")]
    {
        use inauguration::native_emit::x86_64_lower;
        let lowered = x86_64_lower::lower_module(&module, "main").expect("lower integer functions");
        assert!(lowered.extern_calls.is_empty(), "unresolved extern calls");
        let functions = lowered
            .exports
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, lowered.code.len() as u32 - *offset))
            .collect::<Vec<_>>();
        let relocations = lowered
            .relocations
            .iter()
            .map(|offset| (*offset, lowered.codegen_base))
            .collect::<Vec<_>>();
        runtime
            .load_with_globals(
                &lowered.code,
                &functions,
                &relocations,
                &lowered.data,
                &lowered.global_relocs,
            )
            .expect("load integer functions");
    }

    runtime
}

#[test]
fn four_to_six_integer_arguments_are_passed_without_truncation() {
    let runtime = loaded_runtime();
    unsafe {
        assert_eq!(
            runtime.invoke("two", &[1_790_000_000_000, 1_789_999_999_999]),
            Some(1)
        );
        assert_eq!(runtime.invoke("two", &[-4, 5]), Some(-9));
        assert_eq!(runtime.invoke("four", &[1, 2, 3, 1010]), Some(1010));
        assert_eq!(runtime.invoke("four", &[4, 5, 6, -1011]), Some(-1011));
        assert_eq!(
            runtime.invoke("five", &[1, 2, 3, 4, 1_790_000_000_000]),
            Some(1_790_000_000_000)
        );
        assert_eq!(runtime.invoke("five", &[1, 2, 3, 4, -1012]), Some(-1012));

        for position in 0..6 {
            let mut args = [0; 6];
            args[position] = 1001 + position as i64;
            assert_eq!(
                runtime.invoke("six", &args),
                Some(args[position]),
                "argument position {position}"
            );
        }
        assert_eq!(
            runtime.invoke("six", &[1, -2, 3, -4, 5, 1_790_000_000_000]),
            Some(1_790_000_000_003)
        );
    }
}

#[test]
fn unsupported_arity_and_unknown_name_do_not_return_success_zero() {
    let runtime = loaded_runtime();
    unsafe {
        assert_eq!(runtime.invoke("six", &[0; 7]), None);
        assert_eq!(runtime.invoke("absent", &[]), None);
    }
}
