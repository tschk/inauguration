use super::*;
use crate::core_ir::{Decl, Typ, UnifiedModule};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use super::jit::resolve_jit_entry;
use super::util::artifact_stem;

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "inauguration-owned-compile-{}-{}-{name}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn default_request(
    path: PathBuf,
    target: CompileTarget,
    entry: Option<&str>,
    out: Option<PathBuf>,
) -> OwnedCompileRequest {
    OwnedCompileRequest {
        path,
        module_id: "App".to_string(),
        parser: ParserCli::Auto,
        target,
        entry: entry.map(str::to_string),
        out,
        linkage: NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    }
}

#[test]
fn base_report_populates_fields_correctly() {
    let request = default_request(
        PathBuf::from("test.in"),
        CompileTarget::Jit,
        Some("main"),
        None,
    );
    let started = std::time::Instant::now();
    let jobs = 4;
    let report = base_report(&request, jobs, started);
    assert_eq!(report.schema_version, 1);
    assert!(report.owned);
    assert_eq!(report.path, "test.in");
    assert_eq!(report.module_id, "App");
    assert_eq!(report.target, "jit");
    assert_eq!(report.entry.as_deref(), Some("main"));
    assert_eq!(report.linkage, "executable");
    assert_eq!(report.frontend_level, "unsupported");
    assert_eq!(report.semantic_level, "failed");
    assert_eq!(report.backend_level, "owned-native-subset");
    assert_eq!(report.runtime_level, "inrt-jit");
    assert_eq!(report.jobs, 4);
    assert!(!report.success);

    let request_native =
        default_request(PathBuf::from("test2.in"), CompileTarget::Native, None, None);
    let report_native = base_report(&request_native, 1, started);
    assert_eq!(report_native.backend_level, "contract-only");
    assert_eq!(report_native.runtime_level, "none");
}

#[test]
fn unknown_statements_refuse_before_execution_across_profiles() {
    let statement = crate::core_ir::UNSUPPORTED_DEFINED_BODY;
    for (extension, text, marker) in [
        (
            "java",
            "class Probe { public static int main() { int x = 42; synchronized (\"lock\") { x = 7; } return x; } }",
            statement,
        ),
        (
            "py",
            "def main() -> int:\n    assert 0\n    return 42\n",
            statement,
        ),
        (
            "lua",
            "function main() local value = {42}; return end",
            statement,
        ),
        (
            "lua",
            "function main() local value = helper(1, function() end, 2); return end",
            statement,
        ),
        (
            "lua",
            "function main() local value = 1LL; return end",
            "#unsupported-number-literal",
        ),
        (
            "lua",
            "function main() local value = 0b10; return end",
            "#unsupported-number-literal",
        ),
        (
            "lua",
            "function main() print(0x2LL); return end",
            "#unsupported-number-literal",
        ),
        (
            "lua",
            "function main() global value = print(\"side\"); return end",
            statement,
        ),
    ] {
        let source = temp_path(&format!("unknown-statement.{extension}"));
        fs::write(&source, text).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(!report.success, "{extension} {profile:?}: {report:?}");
            assert!(!report.execution_started, "{report:?}");
            assert!(report.eval_result.is_none(), "{report:?}");
            assert!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(marker)),
                "{report:?}"
            );
        }
        fs::remove_file(source).unwrap();
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_dynamic_string_concat_preserves_payloads_and_operand_order() {
    let original = "var trace:Int=0;\nvar word:String=\"λ\0\";\nfn left()->String{trace=trace*10+1;return word;}\nfn right()->String{trace=trace*10+2;return \"雪\";}\nfn stringentry()->String{return (left()+right())+\"!\";}\nfn main()->Int{let result=stringentry();if result==\"λ\0雪!\" && trace==12{return 42;}return 0;}";
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("dynamic-string-concat.in");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("stringentry"),
                None,
            );
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {text}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Str("λ\0雪!".into())));
            request.entry = Some("main".into());
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {text}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_typed_string_globals_preserve_layout_values_and_mutability() {
    for (original, expected) in [
        (
            "const first:String=\"left\";\nvar sentinel:Int=42;\nconst second:String=\"right\";\nfn stringentry()->String{return second;}\nfn main()->Int{if first==\"left\" && second==\"right\" && sentinel==42{return 42;}return 0;}",
            "right",
        ),
        (
            "const empty:String=\"\";\nconst unicode:String=\"你好🦀\";\nconst multiline:String=\"first\n  //literal\n\n尾\";\nfn stringentry()->String{return multiline;}\nfn main()->Int{if empty==\"\" && unicode==\"你好🦀\" && multiline==\"first\\n  //literal\\n\\n尾\"{return 42;}return 0;}",
            "first\n  //literal\n\n尾",
        ),
        (
            "var text:String=\"initial\";\nconst other:String=\"other\";\nconst untouched:String=\"constant\";\nfn mutate()->String{if text!=\"initial\"{return \"wrong initial value\";}text=\"changed\";if text!=\"changed\"{return \"wrong replacement\";}text=other;return text;}\nfn stringentry()->String{return mutate();}\nfn main()->Int{let result=mutate();if result==\"other\" && other==\"other\" && untouched==\"constant\"{return 42;}return 0;}",
            "other",
        ),
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original)
            .unwrap_or_else(|error| panic!("{original}: {error}"));
        for (form, text) in [("original", original), ("canonical", canonical.as_str())] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let source = temp_path("typed-string-global.in");
                fs::write(&source, text).unwrap();
                let mut request = default_request(
                    source.clone(),
                    CompileTarget::Jit,
                    Some("stringentry"),
                    None,
                );
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{form} {profile:?} {text}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Str(expected.into())),
                    "{form} {profile:?} {text}"
                );
                request.entry = Some("main".into());
                let report = compile_owned(&request);
                assert!(report.success, "{form} {profile:?} {text}: {report:?}");
                assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let binary = temp_path("typed-string-global-native");
                    request.target = CompileTarget::Native;
                    request.out = Some(binary.clone());
                    let report = compile_owned(&request);
                    assert!(report.success, "{form} {profile:?} {text}: {report:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(42),
                        "{form} {profile:?} {text}"
                    );
                    fs::remove_file(binary).unwrap();
                }
                fs::remove_file(source).unwrap();
            }
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_typed_string_globals_retain_initializer_and_immutability_refusals() {
    for (text, code, reason) in [
        (
            "var text:String=make();\nfn make()->String{return \"runtime\";}\nfn main()->Int{return 42;}",
            None,
            "initializer",
        ),
        (
            "const text:String=42;\nfn main()->Int{return 42;}",
            Some("verify-type-mismatch"),
            "initializer expected String",
        ),
        (
            "const text:String=\"fixed\";\nfn main()->Int{text=\"changed\";return 42;}",
            Some("verify-immutable-global"),
            "cannot assign to constant global",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("typed-string-global-refusal.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(!report.success, "{profile:?} {text}: {report:?}");
            assert!(report.eval_result.is_none(), "{report:?}");
            if let Some(code) = code {
                assert_eq!(report.reason_code.as_deref(), Some(code), "{report:?}");
            }
            assert!(
                report.reason.as_deref().unwrap_or("").contains(reason),
                "{report:?}"
            );
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_conditional_headers_preserve_quoted_braces_and_expression_effects() {
    for original in [
        "fn main()->Int {if \"{\"==\"{\" {return 42;} return 0;}",
        "fn main()->Int {var count:Int=0;while \"}\"==\"}\" && count<1 {count+=1;} return count+41;}",
        "fn main()->Int {let value=(if \"{\"==\"{\" {42} else {0});return value;}",
        "fn main()->Int {if \"{\"!=\"{\" {return 0;} else if \"}\"==\"}\" {return 42;} else {return 0;}}",
        "fn main()->Int {let value=(if false {0} else if \"{\"==\"{\" {42} else {0});return value;}",
        "fn main()->Int {var calls:Int=0;if ({calls+=1;calls==1}) {return calls+41;} return 0;}",
        "fn main()->Int {var checks:Int=0;var turns:Int=0;while ({checks+=1;turns<1}) {turns+=1;} return checks+turns+39;}",
        "struct Flag {Bool ready}\nfn main()->Int {if (Flag {ready:true}).ready {return 42;} return 0;}",
        "struct Flag {Bool ready}\nfn main()->Int {while (Flag {ready:true}).ready {return 42;} return 0;}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original)
            .unwrap_or_else(|error| panic!("{original}: {error}"));
        for (form, text) in [("original", original), ("canonical", canonical.as_str())] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let source = temp_path("conditional-header.in");
                fs::write(&source, text).unwrap();
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{form} {profile:?} {text}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{form} {profile:?} {text}"
                );
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let binary = temp_path("conditional-header-native");
                    request.target = CompileTarget::Native;
                    request.out = Some(binary.clone());
                    let report = compile_owned(&request);
                    assert!(report.success, "{form} {profile:?} {text}: {report:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(42),
                        "{form} {profile:?} {text}"
                    );
                    fs::remove_file(binary).unwrap();
                }
                fs::remove_file(source).unwrap();
            }
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn unit_aggregate_layouts_refuse_until_zero_width_storage_is_supported() {
    for text in [
        "fn main()->i64{let items=[()];match items[0]{()=>42}}",
        "struct Pair{unit:(),answer:i64}fn main()->i64{let value=Pair{unit:(),answer:42};value.answer}",
        "fn main()->i64{let value=((),42);value.1}",
        "fn main()->i64{let items=vec![()];if items.len()==1{42}else{0}}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("unit-layout-refused.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(!report.success, "{text} {profile:?}: {report:?}");
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn inline_constants_refuse_runtime_dependencies_and_invalid_unused_values() {
    for text in [
        "fn main()->i64{let runtime=42;let value=const{runtime};value}",
        "fn runtime()->i64{42}fn main()->i64{let value=const{runtime()};value}",
        "static VALUE:i64=42;fn main()->i64{let value=const{VALUE};value}",
        "fn runtime()->i64{0}fn main()->i64{let value=const{if true{42}else{runtime()}};value}",
        "fn main()->i64{let value=const{let invalid:i64=false;42};value}",
        "struct Pair{x:i64}fn main()->i64{let value=const{let invalid=Pair{x:false};42};value}",
        "fn main()->i64{let value=const{9223372036854775807+1};value}",
        "fn main()->i64{let value=const{return 42;};value}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("inline-const-refused.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(!report.success, "{text} {profile:?}: {report:?}");
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_inline_constants_execute_only_folded_values() {
    for text in [
        "fn main()->i64{let value=const {let base=20;base+22};value}",
        "fn main()->i64{let value=const{let mut x=20;let y={x+=1;x};x+y};value}",
        "fn main()->i64{let value=const {let mut sum=0;let mut i=0;while i<7{sum+=i;i+=1;}sum*2};value}",
        "const BASE:i64=20;fn main()->i64{let value=const{BASE+22};value}",
        "fn main()->i64{let value=const{if true{42}else{0}};value}",
        "fn main()->i64{let value=const{let a=[20,22];a[0]+a[1]};value}",
        "struct Pair{x:i64,y:i64}fn main()->i64{let value=const{Pair{x:20,y:22}};value.x+value.y}",
        "fn main()->i64{let value=const{(const{20})+22};value}",
        "fn main()->i64{let value=const{let yes=true;yes && true};if value{42}else{0}}",
        "fn main()->i64{let value=const{\"answer\"};if value==\"answer\"{42}else{0}}",
        "fn main()->i64{let value=const{let mut a=[20,0];a[1]=22;a};value[0]+value[1]}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("inline-const.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("inline-const-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_unit_values_preserve_types_effects_parameters_and_returns() {
    for text in [
        "fn main()->i64{let x=match (){()=>42};x}",
        "fn main()->i64{match (){()=>42}}",
        "fn main()->i64{let mut x=0;match (){()=>{x=42;}}x}",
        "static mut CALLS:i64=0;fn unit(){unsafe{CALLS+=1;}}fn main()->i64{match unit(){()=>unsafe{CALLS+41}}}",
        "fn unit()->(){()}fn main()->i64{let x=match unit(){()=>42};x}",
        "fn main()->i64{let u:()=if true{()}else{()};let x=match u{()=>42};x}",
        "fn take(unit:())->i64{let alias=unit;match alias{()=>42}}fn main()->i64{take(())}",
        "fn take(a:i64,unit:(),b:i64)->i64{match unit{()=>a+b}}fn main()->i64{take(20,(),22)}",
        "static mut CALLS:i64=0;fn touch()->(){unsafe{CALLS+=1;}return ();}fn main()->i64{let value=touch();match value{()=>unsafe{CALLS+41}}}",
        "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn relay(){return touch();}fn main()->i64{relay();unsafe{CALLS+41}}",
        "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn take(a:i64,unit:(),b:i64)->i64{match unit{()=>a+b}}fn main()->i64{take(20,touch(),unsafe{CALLS+21})}",
        "fn main()->i64{let x=match (){u if u==()=>42,_=>0};x}",
        "fn main()->i64{if ()==(){42}else{0}}",
        "fn main()->i64{let x=const{()};match x{()=>42}}",
        "fn main()->i64{let x=const{let y=42;};match x{()=>42}}",
        "fn choose(flag:bool)->(){if flag{return ();}()}fn main()->i64{let x=choose(true);match x{()=>42}}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("unit-value.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("unit-value-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_match_values_preserve_selected_effects_bindings_and_control_flow() {
    for text in [
        "fn main()->i64{let tag=1;let value=match tag{0=>20,1=>42,_=>0};value}",
        "fn main()->i64{let x=match 1{n if {return 42;}=>0,_=>0};x}",
        "fn main()->i64{let x=match true{true=>42,false=>0};x}",
        "static mut CALLS:i64=0;fn source()->i64{unsafe{CALLS+=1;}2}fn main()->i64{let x=match source(){0=>0,n if n==2=>41,_=>0};unsafe{x+CALLS}}",
        "fn main()->i64{let mut x=20;let y=match 1{0=>{x=0;0},_=>{x+=1;x}};x+y}",
        "fn main()->i64{let x=20;let y=match 22{x=>x};x+y}",
        "fn main()->i64{let x=match 0{0=>return 42,_=>0};x}",
        "fn main()->i64{let x:i64=match true{true=>return 42,false=>return 0};x}",
        "fn main()->i64{let mut v=match 1{0=>vec![0],_=>vec![20]};v.push(22);v[0]+v[1]}",
        "fn take(prefix:i64,v:Vec<i64>)->i64{prefix+v[0]+v[1]}fn main()->i64{take(1,match 1{0=>vec![0],_=>vec![20,21]})}",
        "struct Pair{x:i64,y:i64}fn main()->i64{let p=match false{true=>Pair{x:0,y:0},false=>Pair{x:20,y:22}};p.x+p.y}",
        "fn main()->i64{let p=match 1{0=>(0,0),_=>(20,22)};p.0+p.1}",
        "fn main()->i64{let x=match (20,22){(a,b) if a==20=>a+b,_=>0};x}",
        "fn main()->i64{let x=match [20,22]{[a,b] if a==20=>a+b,_=>0};x}",
        "fn main()->i64{let x=if let (20,value)=(20,42){value}else{0};x}",
        "fn main()->i64{let mut sum=0;for i in 0..3{let x=match i{1=>continue,_=>i};sum+=x;}sum+40}",
        "fn main()->i64{let mut sum=0;while sum<1{let x=match sum{0=>{sum=42;break;},_=>0};sum+=x;}sum}",
        "static mut CALLS:i64=0;fn main()->i64{let x=match 1{n if {unsafe{CALLS+=1;}false}=>0,n if n==1=>41,_=>0};unsafe{x+CALLS}}",
        "fn main()->i64{let mut values=[20,22];let x=match values{[a,b] if {values[0]=0;true}=>a+b,_=>0};x}",
        "fn main()->i64{let x=match 1{0=>0,_=>match true{true=>42,false=>0}};x}",
        "fn main()->i64{let x=match false{true=>0.0,false=>42.0};x as i64}",
        "fn main()->i64{let x=match 1{0=>\"no\",_=>\"yes\"};if x==\"yes\"{42}else{0}}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("match-value.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("match-value-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_conditional_and_block_values_preserve_scope_effects_and_control_flow() {
    for text in [
        "static mut CALLS:i64=0;fn take(unit:())->i64{unsafe{CALLS+41}}fn main()->i64{take({unsafe{CALLS+=1;}})}",
        "fn main()->i64{let x=if {return 42;}{0}else{1};x}",
        "fn main()->i64{let x=if true{{return 42;}}else{0};x}",
        "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn main()->i64{let unit=touch();unsafe{CALLS+41}}",
        "static mut CALLS:i64=0;fn main()->i64{let mut n=0;for i in 0..3{unsafe{CALLS+=1;if CALLS>3{return 0;}}let x=if i<2{continue;}else{42};n=x;}n}",
        "fn take(prefix:i64,v:Vec<i64>)->i64{prefix+v[0]+v[1]} fn main()->i64{take(1,if false{vec![0]}else{vec![20,21]})}",
        "struct Pair{x:i64,y:i64} fn take(prefix:i64,p:Pair)->i64{prefix+p.x+p.y} fn main()->i64{take(1,if true{Pair{x:20,y:21}}else{Pair{x:0,y:0}})}",
        "fn values()->Vec<i64>{let v=if true{return Vec::with_capacity(10);}else{vec![0]};v} fn main()->i64{let mut v=values();v.push(42);if v.capacity()>=10{v[0]}else{0}}",
        "fn main()->i64{let choose=|flag:bool| if flag{let value=42;value}else{0};choose(true)}",
        "fn main()->i64{let mut v=if true{vec![20]}else{vec![0]};v.push(22);v[0]+v[1]}",
        "fn main()->i64{let v:Vec<i64>=if false{vec![0]}else{vec![20,22]};v[0]+v[1]}",
        "fn main()->i64{let mut x=20;let y={x+=1;x};x+y}",
        "fn main()->i64{let x=20;let y={let x=22;x};x+y}",
        "fn main()->i64{let x=if false{0}else if true{42}else{1};x}",
        "fn take(x:i64,y:i64)->i64{x+y} fn main()->i64{let mut x=20;take({x+=1;x},{x+=1;x})-1}",
        "static mut CALLS:i64=0;fn count()->bool{unsafe{CALLS+=1;}false} fn main()->i64{let x=if count(){unsafe{CALLS+=100;}0}else{41};unsafe{x+CALLS}}",
        "fn main()->i64{let x=if true{return 42;}else{0};x}",
        "fn main()->i64{let x:i64=if true{return 42;}else{return 0;};x}",
        "fn take(v:Vec<i64>)->i64{v[0]+v[1]} fn main()->i64{take(if false{vec![0]}else{vec![20,22]})}",
        "struct Pair{x:i64,y:i64} fn main()->i64{let p=if false{Pair{x:0,y:0}}else{let x=20;Pair{x,y:22}};p.x+p.y}",
        "fn main()->i64{let mut i=0;let mut n=0;while i<3{i+=1;let x=if i<3{continue;}else{42};n=x;}n}",
        "fn main()->i64{let mut x=0;while true{let y=if true{x=42;break;}else{0};x=y;}x}",
        "fn main()->i64{let values={let x=20;[x,22]};values[0]+values[1]}",
        "fn main()->i64{let mut v:Vec<i64>=if false{Vec::with_capacity(1)}else{Vec::with_capacity(10)};v.push(42);if v.capacity()>=10{v[0]}else{0}}",
        "fn main()->i64{let y={let x=20;x+22};y}",
        "fn main()->i64{let value=if false{0.0}else{42.0};value as i64}",
        "fn main()->i64{let value=if false{false}else{true};if value{42}else{0}}",
        "fn main()->i64{let text=if false{\"wrong\"}else{let name=\"answer\";name};if text==\"answer\"{42}else{0}}",
        "fn main()->i64{let pair=if false{(0,0)}else{let x=20;(x,22)};pair.0+pair.1}",
        "fn main()->i64{(if true{[20,22]}else{[0,0]})[1]+20}",
        "fn main()->i64{let ignored={return 42;};0}",
        "fn main()->i64{let ignored={return 42;1}*0;0}",
        "fn main()->i64{let mut x=0;let y={x=42;7}*0;x+y}",
        "fn main()->i64{let ignored={return 42;true}&&false;0}",
        "struct Pair{x:i64,y:i64} impl Pair{fn sum(&self)->i64{self.x+self.y}} fn main()->i64{{let p=Pair{x:20,y:22};p}.sum()}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("conditional-block.rs");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("conditional-block-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_dependency_modules_expand_before_verification_and_optimization() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let crate_name = format!("in_dependency_probe_{}_{}", std::process::id(), nonce);
    let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor");
    let dependency = vendor.join(&crate_name);
    let sources = dependency.join("src");
    fs::create_dir_all(&sources).unwrap();
    for (name, value) in [("left", 20), ("right", 22)] {
        fs::write(sources.join(format!("{name}.rs")), format!(
            "static VALUE:i64={value};struct Entry{{value:i64}}fn helper(value:i64)->i64{{if value>0{{return value;}}0}}fn make()->Entry{{Entry{{value:VALUE}}}}pub fn answer()->i64{{helper(make().value)}}"
        )).unwrap();
    }
    for extension in ["rs", "in"] {
        let source = temp_path(&format!("dependency-before-verification.{extension}"));
        let ret = if extension == "rs" { "i64" } else { "Int" };
        fs::write(&source, format!(
        "fn main()->{ret}{{let first={crate_name}::left::answer;return invoke(first)+{crate_name}::right::answer();}}"
    )).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {report:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("dependency-before-verification-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                if profile == crate::emit_profile::EmitProfile::Default {
                    let dependency_source = sources.join("right.rs");
                    let original = fs::read_to_string(&dependency_source).unwrap();
                    let changed = original.replace("VALUE:i64=22", "VALUE:i64=24");
                    assert_ne!(changed, original);
                    fs::write(&dependency_source, &changed).unwrap();
                    let updated = compile_owned(&request);
                    assert!(updated.success && !updated.cache_hit, "{updated:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(44)
                    );
                    let cached = compile_owned(&request);
                    assert!(cached.success && cached.cache_hit, "{cached:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(44)
                    );
                    fs::write(&dependency_source, &original).unwrap();
                    let restored = compile_owned(&request);
                    assert!(restored.success && !restored.cache_hit, "{restored:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(42)
                    );
                    fs::write(&dependency_source, &changed).unwrap();
                    request.profile = crate::emit_profile::EmitProfile::Lean;
                    let alternate = compile_owned(&request);
                    assert!(alternate.success, "{alternate:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(44)
                    );
                    fs::write(&dependency_source, original).unwrap();
                    request.profile = crate::emit_profile::EmitProfile::Default;
                    let restored = compile_owned(&request);
                    assert!(restored.success && !restored.cache_hit, "{restored:?}");
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(42)
                    );
                }
                fs::remove_file(binary).unwrap();
            }
        }
        fs::remove_file(source).unwrap();
    }
    fs::remove_dir_all(dependency).unwrap();
    let _ = fs::remove_dir(vendor);
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_function_addresses_preserve_targets_and_indirect_arguments() {
    for text in [
        "fn answer()->Int{return 42;}\nfn main()->Int{let pointer:Int=answer;return invoke(pointer);}",
        "fn next(value:Int)->Int{return value+1;}\nfn main()->Int{let pointer:Int=next;return invoke1(pointer,41);}",
        "fn add(a:Int,b:Int)->Int{return a+b;}\nfn main()->Int{let pointer:Int=add;return invoke2(pointer,20,22);}",
        "fn add(a:Int,b:Int)->Int{return a+b;}\nfn pointer()->Int{return add;}\nfn twenty()->Int{return 20;}\nfn twenty_two()->Int{return 22;}\nfn main()->Int{return invoke2(pointer(),twenty(),twenty_two());}",
        "fn next(value:Int)->Int{return value+1;}\nfn main()->Int{let pointers:[Int]=[next];return invoke1(pointers[0],41);}",
        "fn target(value:Int)->Int{return value+1;}\nfn main()->Int{let pointer:Int=target;if pointer!=0{return 42;}return 0;}",
        "fn fail(value:Int)->Int{throw value;return 0;}\nfn bridge()->Int{let pointer:Int=fail;return invoke1(pointer,7);}\nfn main()->Int{try {bridge();return 0;} catch 7 {return 42;} return 0;}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("function-address.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("function-address-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn icore_symbol_names_resolve_declared_parameters_exactly() {
    for (extension, text) in [
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"take\", \"params\": [{\"name\": \"bad value\", \"type\": \"Int\"}], \"return\": \"Int\", \"body\": [{\"kind\": \"if\", \"cond\": {\"kind\": \"binary\", \"op\": \"==\", \"lhs\": {\"kind\": \"ident\", \"name\": \"bad value\"}, \"rhs\": 42}, \"then_body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"ident\", \"name\": \"bad value\"}}], \"else_body\": [{\"kind\": \"return\", \"value\": 0}]}]}, {\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"call\", \"callee\": \"take\", \"args\": [42]}}]}]}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"take\", \"params\": [{\"name\": \"bad..value\", \"type\": \"Int\"}], \"return\": \"Int\", \"body\": [{\"kind\": \"if\", \"cond\": {\"kind\": \"binary\", \"op\": \"==\", \"lhs\": {\"kind\": \"ident\", \"name\": \"bad..value\"}, \"rhs\": 42}, \"then_body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"ident\", \"name\": \"bad..value\"}}], \"else_body\": [{\"kind\": \"return\", \"value\": 0}]}]}, {\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"call\", \"callee\": \"take\", \"args\": [42]}}]}]}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"take\", \"params\": [{\"name\": \"данные\", \"type\": \"Int\"}], \"return\": \"Int\", \"body\": [{\"kind\": \"if\", \"cond\": {\"kind\": \"binary\", \"op\": \"==\", \"lhs\": {\"kind\": \"ident\", \"name\": \"данные\"}, \"rhs\": 42}, \"then_body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"ident\", \"name\": \"данные\"}}], \"else_body\": [{\"kind\": \"return\", \"value\": 0}]}]}, {\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"call\", \"callee\": \"take\", \"args\": [42]}}]}]}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"take\", \"params\": [{\"name\": \"正常\", \"type\": \"Int\"}], \"return\": \"Int\", \"body\": [{\"kind\": \"if\", \"cond\": {\"kind\": \"binary\", \"op\": \"==\", \"lhs\": {\"kind\": \"ident\", \"name\": \"正常\"}, \"rhs\": 42}, \"then_body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"ident\", \"name\": \"正常\"}}], \"else_body\": [{\"kind\": \"return\", \"value\": 0}]}]}, {\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"value\": {\"kind\": \"call\", \"callee\": \"take\", \"args\": [42]}}]}]}",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path(&format!("symbol-name.{extension}"));
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("shared-symbol-name-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn shared_unit_fronts_preserve_void_values_and_matches() {
    for (extension, text) in [
        (
            "in",
            "fn unit()->void {return ();}\nfn main()->Int {let value=unit();let answer=(match (value) {() -> {42}});return answer;}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"expr\": {\"kind\": \"match-value\", \"scrutinee\": {\"kind\": \"unit\"}, \"arms\": [{\"pattern\": \"()\", \"value\": 42}]}}]}]}",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path(&format!("unit-value.{extension}"));
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("shared-unit-value-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn shared_match_value_fronts_execute_and_preserve_errors() {
    for (extension, text) in [
        (
            "in",
            "fn main()->Int {let value=(match (20) {value if value==20 -> {let extra=22;value+extra} _ -> {0}});return value;}",
        ),
        (
            "in",
            "fn maybe()->Int {let value=(match (true) {true -> {throw 7;0} false -> {0}});return value;}\nfn main()->Int {try {maybe();return 0;} catch 7 {return 42;} return 0;}",
        ),
        (
            "in",
            "fn main()->Int {var sum:Int=0;for i in 0..3 {let value=(match (i) {1 -> {continue;} _ -> {i}});sum+=value;}return sum+40;}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"expr\": {\"kind\": \"match-value\", \"scrutinee\": 20, \"arms\": [{\"pattern\": \"value\", \"guard\": {\"kind\": \"binary\", \"op\": \"==\", \"lhs\": {\"kind\": \"ident\", \"name\": \"value\"}, \"rhs\": 20}, \"value\": {\"kind\": \"binary\", \"op\": \"+\", \"lhs\": {\"kind\": \"ident\", \"name\": \"value\"}, \"rhs\": 22}}, {\"pattern\": \"_\", \"value\": 0}]}}]}]}",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path(&format!("match-value.{extension}"));
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("shared-match-value-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn shared_conditional_block_fronts_execute_and_preserve_errors() {
    for (extension, text) in [
        (
            "in",
            "fn main()->Int { let base=20; let chosen=(if (false) {0} else {let local=22;local}); return base+chosen; }",
        ),
        (
            "in",
            "fn maybe()->Int {let x=({throw 7;0});return x;}\nfn main()->Int {try {maybe();return 0;} catch 7 {return 42;} return 0;}",
        ),
        (
            "in",
            "fn main()->Int {var sum:Int=0;for i in 0..3 {let value=({if i==1 {continue;} i});sum+=value;} return sum+40;}",
        ),
        (
            "icore",
            "{\"icoreVersion\": 2, \"decls\": [{\"kind\": \"function\", \"name\": \"main\", \"params\": [], \"return\": \"Int\", \"body\": [{\"kind\": \"return\", \"expr\": {\"kind\": \"conditional\", \"cond\": true, \"then\": {\"kind\": \"block\", \"body\": [{\"kind\": \"let\", \"name\": \"base\", \"type\": \"Int\", \"value\": 20}], \"result\": {\"kind\": \"binary\", \"op\": \"+\", \"lhs\": {\"kind\": \"ident\", \"name\": \"base\"}, \"rhs\": 22}}, \"else\": 0}}]}]}",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path(&format!("conditional-block.{extension}"));
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("shared-conditional-block-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
fn owned_compile_rejects_primitive_field_read_and_write() {
    for source in [
        "fn main() -> i64 { let mut value: i64 = 0; value.missing = 42; return value; }",
        "fn main() -> i64 { let value: i64 = 42; return value.missing; }",
        "fn value() -> f64 { return 42.0; } fn main() -> i64 { value().missing = 0; return 42; }",
        "struct Point { x: i64 } fn main() -> i64 { let mut point: Point = Point { x: 0 }; point.x.missing = 42; return point.x; }",
    ] {
        let path = temp_path("primitive-field.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{report:?}: {source}");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("verify-invalid-field-base"),
            "{report:?}: {source}"
        );
        assert!(report.eval_result.is_none());
    }
}

#[test]
fn owned_compile_rejects_unknown_field_write_before_execution() {
    let path = temp_path("unknown-field.rs");
    fs::write(&path, "struct Point { x: i64 } fn main() -> i64 { let mut point: Point = Point { x: 0 }; point.missing = 42; return 42; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(!report.success, "{report:?}");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("verify-unknown-struct-field")
    );
    assert!(report.eval_result.is_none());
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn rust_jit_executes_explicit_scalar_vector_constructors() {
    for text in [
        "fn main()->i64{let mut items=vec![20];items.extend(vec![22]);items[0]+items[1]}",
        "mod Vec{pub fn with_capacity(value:i64)->i64{value}} fn main()->i64{Vec::with_capacity(42)}",
        "struct Reader{value:i64} impl Reader{fn take(&self,items:Vec<i64>)->i64{if items.capacity()>=10{self.value}else{0}}} fn main()->i64{let reader=Reader{value:42};reader.take(Vec::with_capacity(10))}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::new()};bag.items=Vec::with_capacity(10);if bag.items.capacity()>=10{42}else{0}}",
        "fn main()->i64{let items:Vec<i64>=Vec::with_capacity(10).clone();if items.capacity()==0 && items.len()==0{42}else{0}}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::with_capacity(10);let before=items.len();items.push(42);if before==0 && items.capacity()>=10{items[0]}else{0}}",
        "fn main()->i64{let mut items=Vec::with_capacity(2);items.push(20);items.push(22);if items.capacity()>=2{items[0]+items[1]}else{0}}",
        "fn main()->i64{let mut items=std::vec::Vec::<f64>::with_capacity(3);items.push(42.0);items[0] as i64}",
        "fn main()->i64{let mut items=alloc::vec::Vec::<_>::with_capacity(3);items.push(true);if items[0]{42}else{0}}",
        "fn source()->Vec<i64>{Vec::with_capacity(10)} fn main()->i64{let mut items=source();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
        "fn take(items:Vec<i64>)->i64{if items.len()==0 && items.capacity()>=10{42}else{0}} fn main()->i64{take(Vec::with_capacity(10))}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::with_capacity(10)};bag.items.push(42);if bag.items.capacity()>=10{bag.items[0]}else{0}}",
        "fn main()->i64{let make=||->Vec<i64>{Vec::with_capacity(10)};let mut items=make();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
        "static mut CALLS:i64=0;fn count()->i64{unsafe{CALLS+=1;}10} fn main()->i64{let mut items=Vec::<i64>::with_capacity(count());items.push(41);unsafe{items[0]+CALLS}}",
        "fn main()->i64{let mut items=Vec::<i64>::with_capacity(0);if items.capacity()!=0{return 0;}items.push(20);items.push(22);items[0]+items[1]}",
        "fn main()->i64{let mut items=Vec::new();items.push(42);if items.capacity()>items.len(){42}else{0}}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::new();let before=items.capacity();items.extend(vec![1,2,3,4,5,6,7,8,9,10]);if before==0 && items.capacity()>=items.len(){42}else{0}}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let bag=Bag{items:vec![20,22]};if bag.items.capacity()>=2{42}else{0}}",
        "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![42]} fn main()->i64{let capacity=source().capacity();unsafe{if capacity>=1 && CALLS==1{42}else{0}}}",
        "fn main()->i64{let items=vec![42];if items.clone().capacity()>=1{42}else{0}}",
        "fn capacity(flag:bool)->i64{0} fn main()->i64{let items=vec![42];if items.capacity()>=1{42}else{0}}",
        "fn capacity(items:Vec<i64>)->i64{42} fn main()->i64{let items=vec![1];items.capacity()}",
        "fn main()->i64{let mut items=vec![20,22];items.extend(vec![]);items[0]+items[1]}",
        "fn main()->i64{let mut items=vec![20.0,22.0];items.extend(vec![]);(items[0]+items[1]) as i64}",
        "fn main()->i64{let mut items=vec![true];items.extend([]);if items[0]{42}else{0}}",
        "fn main()->i64{let mut items=vec![20];let source=[22];items.extend(source);items[0]+items[1]}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(vec![1,2,3,4,5,6,7,8,9,10]);items.len()+items[9]+22}",
        "fn main()->i64{let mut items=vec![20,22];let empty:Vec<i64>=vec![];items.extend(empty);items[0]+items[1]}",
        "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![20,21]} fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(source());unsafe{items[0]+items[1]+CALLS}}",
        "fn main()->i64{let mut items=Vec::new();items.extend(vec![20,22]);items[0]+items[1]}",
        "fn extend(items:Vec<bool>,source:Vec<i64>)->i64{42} fn main()->i64{let items=Vec::new();items.extend(vec![1])}",
        "fn source()->Vec<i64>{vec![20,22]} fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(source());items[0]+items[1]}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:vec![20]};let other=vec![22];bag.items.extend(other);bag.items[0]+bag.items[1]}",
        "fn main()->i64{let mut items=vec![20,1];let source=items.clone();items.extend(source);items[0]+items[2]+items[1]+items[3]}",
        "fn main()->i64{let mut items=vec![true];items.extend(vec![false]);if items[0] && !items[1]{42}else{0}}",
        "fn main()->i64{let mut items=vec![20.0];items.extend(vec![22.0]);(items[0]+items[1]) as i64}",
        "fn main()->i64{let mut items=Vec::<i64>::new();items.push(42);items[0]}",
        "fn main()->i64{let mut items=Vec::<_>::new();items.push(42);items[0]}",
        "fn values()->Vec<i64>{Vec::<_>::new()} fn main()->i64{let mut items=values();items.push(42);items[0]}",
        "fn main()->i64{let mut items=std::vec::Vec::<_>::new();items.push(true);if items[0]{42}else{0}}",
        "fn main()->i64{let mut items=alloc::vec::Vec::<_>::new();items.push(42.0);items[0] as i64}",
        "fn main()->i64{let mut items=std::vec::Vec::new();items.push(42);items[0]}",
        "fn main()->i64{let mut items=alloc::vec::Vec::new();items.push(20);items.push(22);items[0]+items[1]}",
        "fn values()->Vec<i64>{std::vec::Vec::<i64>::new()} fn main()->i64{let mut items=values();items.push(42);items[0]}",
        "fn main()->i64{let mut items=alloc::vec::Vec::<f64>::new();items.push(42.0);items[0] as i64}",
        "fn main()->i64{let mut items=Vec::<bool>::new().clone();items.push(true);if items[0]{42}else{0}}",
        "fn take(items:Vec<i64>)->i64{items.len()+42} fn main()->i64{take(Vec::<i64>::new())}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::<i64>::new()};bag.items.push(42);bag.items[0]}",
        "fn main()->i64{let make=|| Vec::<i64>::new();let mut items=make();items.push(42);items[0]}",
    ] {
        let path = temp_path("explicit-vector.rs");
        fs::write(&path, text).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}: {report:?}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn rust_jit_preserves_vector_return_storage_after_optimization() {
    let path = temp_path("vector-return-storage.rs");
    fs::write(&path, "fn values()->Vec<i64>{vec![20,22]} fn main()->i64{let mut items=values();items.push(0);items[0]+items[1]}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{profile:?}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(42)),
            "{profile:?}: {report:?}"
        );
    }
    fs::remove_file(path).unwrap();
}

#[test]
#[cfg(all(unix, target_arch = "aarch64"))]
fn rust_jit_executes_known_aggregate_parameters() {
    for source in [
        "struct Bag { items: Vec<i64> } impl Bag { fn replace(&mut self) -> i64 { self.items = vec![20, 22]; return self.items[0] + self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; return Bag::replace(&mut bag); }",
        "struct Bag { items: Vec<i64> } struct Wrapper { bag: Bag } impl Wrapper { fn replace(&mut self) -> i64 { self.bag.items = vec![20, 22]; return self.bag.items[0] + self.bag.items[1]; } } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { bag: Bag { items: vec![0] } }; return Wrapper::replace(&mut wrapper); }",
        "struct Bag { items: Vec<i64> } impl Bag { fn replace(&mut self, other: Bag) -> i64 { self.items = other.items; return self.items[0] + self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; let other: Bag = Bag { items: vec![20, 22] }; return Bag::replace(&mut bag, other); }",
        "struct Inner { value: i64 } struct Wrapper { left: Inner, right: Inner } impl Wrapper { fn answer(&mut self) -> i64 { self.left.value = 20; return self.left.value + self.right.value; } } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { left: Inner { value: 0 }, right: Inner { value: 22 } }; return Wrapper::answer(&mut wrapper); }",
        "struct Empty {} impl Empty { fn answer(&self, value: i64) -> i64 { return value; } } fn main() -> i64 { let empty: Empty = Empty {}; return Empty::answer(&empty, 42); }",
        "struct Pair { x: i64, y: f64 } fn answer(value: Pair) -> i64 { return value.x + value.y as i64; } fn main() -> i64 { return answer(Pair { x: 20, y: 22.0 }); }",
        "struct Pair { x: i64, y: i64 } fn answer(value: Pair, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64) -> i64 { return value.x + value.y + a + b + c + d + e + f + g; } fn main() -> i64 { return answer(Pair { x: 20, y: 15 }, 1, 1, 1, 1, 1, 1, 1); }",
        "struct Bag { items: Vec<i64> } impl Bag { fn answer(&mut self, other: Bag, a: i64, b: i64, c: i64, d: i64, e: i64) -> i64 { self.items = other.items; return self.items[0] + self.items[1] + a + b + c + d + e; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; let other: Bag = Bag { items: vec![20, 17] }; return Bag::answer(&mut bag, other, 1, 1, 1, 1, 1); }",
    ] {
        let path = temp_path("aggregate-parameters.rs");
        fs::write(&path, source).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}: {source}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {report:?}: {source}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(all(unix, target_arch = "aarch64"))]
fn jit_executes_typed_saturating_float_casts() {
    for source in [
        "fn main() -> i64 { let items: Vec<f64> = vec![20.0, 22.0]; return (items[0] + items[1]) as i64; }",
        "struct Point { x: f64 } fn main() -> i64 { let point = Point { x: 42.9 }; return point.x as i64; }",
        "fn value() -> f64 { return 1000.5; } fn main() -> i64 { if value() as i8 == 127 { return 42; } return 0; }",
        "fn value() -> f64 { return -42.9; } fn main() -> i64 { return (value() as u8) as i64 + 42; }",
        "fn value() -> f64 { return 9223372036854775808.0; } fn main() -> i64 { let value: i64 = (value() as u64) as i64; if value < 0 { return 42; } return 0; }",
        "fn value() -> f64 { return 0.0 / 0.0; } fn main() -> i64 { return (value() as i64) + 42; }",
        "const LIMIT: i8 = 1000.0 as i8; fn main() -> i64 { return LIMIT as i64 - 85; }",
    ] {
        let path = temp_path("float-cast.rs");
        fs::write(&path, source).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}: {source}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {report:?}: {source}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(all(unix, target_arch = "aarch64"))]
fn jit_compiles_contextual_vector_literals_through_validation() {
    for source in [
        "fn main() -> i64 { let values: Vec<i64> = vec![20, 22]; let mut sum = 0; for value in values { sum += value; } return sum; }",
        "struct Bag { items: Vec<i64> } fn main() -> i64 { let bag = Bag { items: vec![20, 22] }; return bag.items[0] + bag.items[1]; }",
        "fn values() -> Vec<i64> { return vec![20, 22]; } fn main() -> i64 { let items: Vec<i64> = values(); let mut sum = 0; for value in items { sum += value; } return sum; }",
        "fn accept(items: Vec<i64>) -> i64 { return items[0] + items[1]; } fn main() -> i64 { return accept(vec![20, 22]); }",
        "fn accept(items: Vec<i64>) -> i64 { return items[0] + items[1]; } fn main() -> i64 { return accept(vec![accept(vec![20, 22]), 0]); }",
        "struct Bag { items: Vec<i64> } fn main() -> i64 { let values: Vec<i64> = vec![20, 22]; let bag = Bag { items: values }; return bag.items[0] + bag.items[1]; }",
        "fn main() -> i64 { let mut items: Vec<i64> = vec![]; items = vec![20, 22]; return items[0] + items[1]; }",
        "fn values() -> Vec<i64> { return vec![]; } fn main() -> i64 { let items: Vec<i64> = values(); let mut sum = 42; for value in items { sum += value; } return sum; }",
        "fn main() -> i64 { let mut items: Vec<i64> = vec![0, 22]; items[0] = 20; return items[0] + items[1]; }",
        "fn main() -> f64 { let items: Vec<f64> = vec![20.0, 22.0]; return items[0] + items[1]; }",
        "fn accept(items: Vec<i64>) -> i64 { items[0] = 20; return items[0] + items[1]; } fn main() -> i64 { return accept(vec![0, 22]); }",
        "fn accept(items: Vec<f64>) -> f64 { items[0] = 20.0; return items[0] + items[1]; } fn main() -> f64 { return accept(vec![0.0, 22.0]); }",
    ] {
        let path = temp_path("vector-literal.rs");
        fs::write(&path, source).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}: {source}");
            assert_eq!(
                report.eval_result,
                Some(if source.contains("fn main() -> f64") {
                    EvalValue::Float(42.0)
                } else {
                    EvalValue::Int(42)
                }),
                "{profile:?}: {report:?}: {source}"
            );
        }
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn native_target_reports_host_status() {
    let source_path = temp_path("native.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("main"),
        None,
    ));

    if native_backend::native_subset_host_available() {
        assert!(!report.success);
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--out"))
        );
    } else {
        assert!(!report.success);
        assert!(
            matches!(
                report.reason_code.as_deref(),
                Some(native_backend::NATIVE_BACKEND_NOT_IMPLEMENTED)
                    | Some("native-lowering-failed")
            ),
            "unexpected reason_code: {:?}",
            report.reason_code
        );
    }

    fs::remove_file(source_path).unwrap();
}

#[test]
fn jit_executes_stdlib_process_run_with_string_result() {
    for (imports, name) in [
        ("", "process-run"),
        ("", "process_run"),
        (
            "import std.process;\ncapability process.spawn;\n",
            "process-run",
        ),
        (
            "import std.process;\ncapability process.spawn;\n",
            "process_run",
        ),
    ] {
        let source_path = temp_path("stdlib-process-run.in");
        fs::write(
            &source_path,
            format!("{imports}fn main() -> String {{ return {name}(\"printf hello\"); }}\n"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source_path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source_path).unwrap();
        if !super::jit::host_supports_jit() {
            assert!(!report.success, "{report:?}");
            assert!(
                report
                    .error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("jit-host-unsupported:")),
                "{report:?}"
            );
            continue;
        }
        assert!(report.success, "{imports}/{name}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Str("hello".into())));
    }
}

#[test]
fn native_compile_rejects_missing_explicit_entry() {
    let source_path = temp_path("native-missing-entry.in");
    let out_path = temp_path("native-missing-entry.out");
    fs::write(&source_path, "fn main() -> Int { return 42; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    ));

    assert!(!report.success, "{:?}", report);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("verify-missing-entry-symbol")
    );

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_file(out_path);
}

#[test]
fn native_staticlib_emits_x86_64_linux_object_file() {
    let source_path = temp_path("x86-object.in");
    let out_path = temp_path("x86-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    assert_eq!(
        report.artifact_path.as_deref(),
        Some(out_path.to_str().unwrap())
    );
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 62);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn dual_emit_writes_runtime_and_harden_artifacts() {
    let source_path = temp_path("dual-emit.in");
    let runtime_out = temp_path("dual-runtime.o");
    let harden_out = temp_path("dual-harden.o");
    let sample = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/compile/antidecomp_sample.in");
    let source = fs::read_to_string(&sample).unwrap_or_else(|_| {
        concat!(
            "fn mix(a: Int, b: Int) -> Int {\n",
            "  let s: Int = a + b;\n",
            "  let t: Int = s * 3;\n",
            "  return t - a;\n",
            "}\n",
            "fn gate(n: Int) -> Int {\n",
            "  if n > 0 {\n",
            "    return mix(n, 2);\n",
            "  } else {\n",
            "    return mix(1, n);\n",
            "  }\n",
            "}\n",
            "fn main() -> Int {\n",
            "  return gate(7);\n",
            "}\n",
        )
        .to_string()
    });
    fs::write(&source_path, source).unwrap();

    let mut runtime_req = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(runtime_out.clone()),
    );
    runtime_req.linkage = NativeLinkage::StaticLib;
    runtime_req.target_triple = Some("x86_64-unknown-none".to_string());
    runtime_req.profile = crate::emit_profile::EmitProfile::Default;

    let mut harden_req = runtime_req.clone();
    harden_req.out = Some(harden_out.clone());
    harden_req.profile = crate::emit_profile::EmitProfile::Harden;

    let runtime_report = compile_owned(&runtime_req);
    let harden_report = compile_owned(&harden_req);

    if !runtime_report.success || !harden_report.success {
        let msg = format!("{runtime_report:?} / {harden_report:?}");
        if msg.contains("not implemented")
            || msg.contains("NATIVE_BACKEND")
            || msg.contains("native-lowering-failed")
        {
            eprintln!("skip dual_emit_writes_runtime_and_harden_artifacts: {msg}");
            let _ = fs::remove_file(&source_path);
            let _ = fs::remove_file(&runtime_out);
            let _ = fs::remove_file(&harden_out);
            return;
        }
        panic!("dual-emit owned compile failed: {msg}");
    }

    assert!(runtime_out.is_file(), "runtime artifact missing");
    assert!(harden_out.is_file(), "harden artifact missing");
    let runtime_bytes = fs::read(&runtime_out).expect("read runtime");
    let harden_bytes = fs::read(&harden_out).expect("read harden");
    assert_ne!(
        runtime_bytes, harden_bytes,
        "runtime and harden artifacts must differ"
    );
    for name in [b"\0mix\0".as_slice(), b"\0gate\0".as_slice()] {
        for artifact in [&runtime_bytes, &harden_bytes] {
            assert!(
                artifact.windows(name.len()).any(|symbol| symbol == name),
                "both library profiles must preserve exported helper names"
            );
        }
    }
    assert_eq!(
        &harden_bytes[..4.min(harden_bytes.len())],
        b"\x7fELF",
        "harden respects the requested ELF object target"
    );

    assert_eq!(
        u16::from_le_bytes(harden_bytes[16..18].try_into().unwrap()),
        1,
        "harden must preserve static-library linkage"
    );
    assert_eq!(harden_report.runtime_level, "freestanding-none");

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&runtime_out).unwrap();
    fs::remove_file(&harden_out).unwrap();
    let _ = fs::remove_file(runtime_out.with_extension("abi.json"));
    let _ = fs::remove_file(harden_out.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_aarch64_linux_object_file() {
    let source_path = temp_path("aarch64-object.in");
    let out_path = temp_path("aarch64-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("aarch64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-static");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 183);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_arm32_linux_object_file() {
    let source_path = temp_path("arm32-object.in");
    let out_path = temp_path("arm32-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("armv7-unknown-linux-gnueabihf".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 1);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 40);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_aarch64_macho_archive() {
    let source_path = temp_path("macho-staticlib.in");
    let out_path = temp_path("macho-staticlib.a");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-static");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("archive bytes");
    assert_eq!(&bytes[..8], b"!<arch>\n");
    assert!(bytes.windows(7).any(|window| window == b"_answer"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_executable_emits_x86_64_linux_elf_file() {
    let source_path = temp_path("x86-executable.in");
    let out_path = temp_path("x86-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 62);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&out_path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_aarch64_linux_elf_file() {
    let source_path = temp_path("aarch64-linux-executable.in");
    let out_path = temp_path("aarch64-linux-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-aarch64-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 2);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 183);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_arm32_linux_elf_file() {
    let source_path = temp_path("arm32-linux-executable.in");
    let out_path = temp_path("arm32-linux-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("armv7-unknown-linux-gnueabihf".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-arm32");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-armv7-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 1);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 40);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_windows_pe_exe_file() {
    let source_path = temp_path("windows-executable.in");
    let out_path = temp_path("windows-executable.exe");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-pc-windows-msvc".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64");
    assert_eq!(report.runtime_level, "windows-exitprocess");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-windows-exe-subset")
    );
    let bytes = fs::read(&out_path).expect("exe bytes");
    assert_eq!(&bytes[0..2], b"MZ");
    let pe_off = u32::from_le_bytes(bytes[0x3C..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe_off..pe_off + 4], b"PE\0\0");
    assert_eq!(
        u16::from_le_bytes(bytes[pe_off + 4..pe_off + 6].try_into().unwrap()),
        0x8664
    );
    assert!(bytes.windows(12).any(|window| window == b"KERNEL32.dll"));
    assert!(bytes.windows(11).any(|window| window == b"ExitProcess"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_aarch64_darwin_app_bundle() {
    let source_path = temp_path("darwin-app.in");
    let out_path = temp_path("Answer.app");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-app");
    assert_eq!(report.runtime_level, "macos-app-bundle");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-aarch64-darwin-app-subset")
    );
    let executable = out_path
        .join("Contents/MacOS")
        .join(artifact_stem(&out_path, "App"));
    assert!(executable.exists());
    assert!(out_path.join("Contents/Info.plist").exists());

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_dir_all(&out_path);
}

#[test]
fn native_executable_emits_linux_appdir() {
    let source_path = temp_path("linux-appdir.in");
    let out_path = temp_path("Answer.AppDir");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64-appdir");
    assert_eq!(report.runtime_level, "linux-appdir");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-linux-appdir-subset")
    );
    assert!(out_path.join("AppRun").exists());
    assert!(out_path.join("answer.desktop").exists());

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_dir_all(&out_path);
}

#[test]
fn native_executable_appimage_fails_closed() {
    let source_path = temp_path("linux-appimage.in");
    let out_path = temp_path("Answer.AppImage");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-package-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn explicit_unsupported_native_target_fails_closed() {
    let source_path = temp_path("unsupported-target.in");
    let out_path = temp_path("unsupported-target.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("riscv64gc-unknown-none-elf".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-target-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn explicit_aarch64_darwin_executable_target_fails_closed() {
    let source_path = temp_path("unsupported-macho-executable.in");
    let out_path = temp_path("unsupported-macho-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-target-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn native_staticlib_emits_wasm32_module() {
    let source_path = temp_path("wasm-object.in");
    let out_path = temp_path("wasm-object.wasm");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("wasm32-unknown-unknown".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("wasm bytes");
    assert_eq!(&bytes[0..4], b"\0asm");
    assert!(bytes.windows(6).any(|window| window == b"answer"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_answer_entry_compiles_on_aarch64_host() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("answer.in");
    let out_path = temp_path("answer.bin");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    ));
    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset");
    assert_eq!(report.runtime_level, "inrt-native");
    assert_eq!(report.eval_exit_code, Some(42));
    assert!(out_path.exists());

    fs::remove_file(source_path).unwrap();
    fs::remove_file(out_path).unwrap();
}

#[test]
fn native_polyglot_answer_entries_compile_on_aarch64_host() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let cases = [
        (
            "sample.js",
            "function answer() {\n  return 42;\n}\n\nfunction main() {}\n",
        ),
        (
            "sample.ts",
            "function answer(): number {\n  return 42;\n}\n\nfunction main(): void {}\n",
        ),
        (
            "sample.py",
            "def answer() -> int:\n    return 42\n\ndef main() -> None:\n    pass\n",
        ),
        (
            "sample.rb",
            "def answer\n  return 42\nend\n\ndef main\nend\n",
        ),
        (
            "sample.zig",
            "fn answer() i32 {\n    return 42;\n}\n\npub fn main() void {}\n",
        ),
        #[cfg(feature = "parse-extended")]
        (
            "sample.php",
            "<?php\n\nfunction answer(): int {\n    return 42;\n}\n\nfunction main(): void {\n}\n",
        ),
        (
            "Sample.java",
            "class Sample {\n  static int answer() {\n    return 42;\n  }\n\n  public static void main(String[] args) {}\n}\n",
        ),
    ];
    for (name, source) in cases {
        let source_path = temp_path(name);
        fs::write(&source_path, source).unwrap();
        let out_path = temp_path(&format!(
            "polyglot-{}.bin",
            source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("sample")
        ));
        let report = compile_owned(&default_request(
            source_path.clone(),
            CompileTarget::Native,
            Some("answer"),
            Some(out_path.clone()),
        ));
        assert!(report.success, "{}: {report:?}", source_path.display());
        assert_eq!(report.eval_exit_code, Some(42), "{}", source_path.display());
        assert!(out_path.exists(), "{}", source_path.display());
        fs::remove_file(source_path).unwrap();
        fs::remove_file(out_path).unwrap();
    }
}

#[test]
fn report_has_empty_external_invocations() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("external.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.external_invocations.is_empty());
    assert!(report.success);

    fs::remove_file(source_path).unwrap();
}

#[test]
fn bare_source_without_entry_keeps_main_through_optimization() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("bare-main.in");
    fs::write(
        &source_path,
        "fn helper() -> Int { return 3; }\n\nfn main() -> Int { return helper(); }\n",
    )
    .unwrap();

    // No entry is named, so the optimizer must not fall back to a kernel entry
    // name and delete `main` along with everything it calls. A stripped module
    // fails lowering with "module has no functions" and returns no result.
    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{report:?}");
    assert!(
        report.typed_function_count >= 1,
        "optimizer emptied the module: {report:?}"
    );
    assert_eq!(report.eval_exit_code, Some(3), "{report:?}");
    fs::remove_file(source_path).unwrap();
}

/// A Float entry's value comes back in the integer result register as its `f64`
/// bit pattern, so the JIT can report it; the exit status still cannot carry it
/// and stays 0.
#[test]
fn float_entry_reports_its_value_without_an_exit_status() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("float-main.in");
    fs::write(&source_path, "fn main() -> Float { return 2.5 + 3.5; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_exit_code, Some(0), "{report:?}");
    assert_eq!(
        report.eval_result,
        Some(crate::owned_compile::EvalValue::Float(6.0)),
        "{report:?}"
    );
    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_to_json_roundtrip_fields() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("json.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    let json = report_to_json(&report).unwrap();
    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"owned\": true"));
    assert!(json.contains("\"external_invocations\": []"));

    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_to_json_serialization_format() {
    let started = std::time::Instant::now();
    let request = OwnedCompileRequest {
        path: std::path::PathBuf::from("/fake/path"),
        module_id: "test_module".to_string(),
        parser: crate::parser_registry::ParserCli::In,
        target: CompileTarget::Native,
        entry: None,
        out: None,
        linkage: crate::native_emit::NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    };

    let mut report = crate::owned_compile::report::base_report(&request, 1, started);
    report.timing_micros = 12345;
    report.timing_waves_us = Some(vec![12345]);

    let json = crate::owned_compile::report_to_json(&report).expect("Failed to serialize report");

    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"owned\": true"));
    assert!(json.contains("\"path\": \"/fake/path\""));
    assert!(json.contains("\"module_id\": \"test_module\""));
    assert!(json.contains("\"target\": \"native\""));
    assert!(json.contains("\"linkage\": \"executable\""));
    assert!(json.contains("\"frontend_level\": \"unsupported\""));
    assert!(json.contains("\"semantic_level\": \"failed\""));
    assert!(json.contains("\"backend_level\": \"contract-only\""));
    assert!(json.contains("\"runtime_level\": \"none\""));
    assert!(json.contains("\"success\": false"));
    assert!(json.contains("\"timing_micros\": 12345"));
}

#[test]
fn report_carries_core_identity_metadata() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("identity.in");
    fs::write(
        &source_path,
        "package agents.video;\nmodule agents.video.main;\nfn main() -> Int { return 7; }\n",
    )
    .unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{:?}", report);
    let identity = report.module_identity.as_ref().expect("module identity");
    assert_eq!(identity.package.as_deref(), Some("agents.video"));
    assert_eq!(identity.module.as_deref(), Some("agents.video.main"));
    assert_eq!(identity.requested_module_id, "App");
    assert_eq!(identity.effective_module_id, "agents.video.main");

    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_defaults_identity_metadata_without_source_identity() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("default-identity.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{:?}", report);
    let identity = report.module_identity.as_ref().expect("module identity");
    assert_eq!(identity.package, None);
    assert_eq!(identity.module, None);
    assert_eq!(identity.requested_module_id, "App");
    assert_eq!(identity.effective_module_id, "App");

    fs::remove_file(source_path).unwrap();
}

#[test]
fn jit_execution_does_not_reuse_report_cache() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("cache.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();
    let first = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    assert!(!first.cache_hit);
    let second = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    assert!(!second.cache_hit);
    assert_eq!(first.success, second.success);
    fs::remove_file(source_path).unwrap();
}

#[test]
fn resolve_jit_entry_exact_match() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "main");
}

#[test]
fn resolve_jit_entry_namespaced() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "package.main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "package.main");
}

#[test]
fn resolve_jit_entry_suffix_dot() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "foo.bar.main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "foo.bar.main");
}

#[test]
fn resolve_jit_entry_no_match_falls_through() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "other".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    // Falls through to first non-closure function for library mode
    assert_eq!(resolve_jit_entry(&module, "main"), "other");
}

#[test]
fn resolve_jit_entry_prefers_exact_over_namespaced() {
    let module = UnifiedModule::new(vec![
        Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![],
            type_params: vec![],
        },
        Decl::Function {
            name: "package.main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![],
            type_params: vec![],
        },
    ]);
    assert_eq!(resolve_jit_entry(&module, "main"), "main");
}

#[test]
fn resolve_jit_entry_non_dot_suffix_not_matched() {
    // "also_main" ends with "main" but char before is '_', not '.'
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "also_main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    // Falls through to first non-closure function for library mode
    assert_eq!(resolve_jit_entry(&module, "main"), "also_main");
}

#[test]
fn compilation_restores_the_callers_emission_profile() {
    use crate::emit_profile::EmitProfile;
    use crate::native_emit::antidecomp;
    let module = crate::in_lang_parse::parse_in_source(
        "fn add(a: Int, b: Int) -> Int { return a + b; }\nfn main() -> Int { return add(5, 37); }",
    )
    .unwrap();
    let previous = antidecomp::current_profile();
    antidecomp::set_profile(EmitProfile::Lean);
    let request = default_request(
        PathBuf::from("profile.in"),
        CompileTarget::Native,
        Some("main"),
        None,
    );
    // Missing output fails without executing the entry.
    assert!(super::native::compile_native(&module, "App", &request).is_err());
    let actual = antidecomp::current_profile();
    antidecomp::set_profile(previous);
    assert_eq!(actual, EmitProfile::Lean);
    if super::jit::host_supports_jit() {
        for entry in ["main", "absent"] {
            antidecomp::set_profile(EmitProfile::Lean);
            let request = default_request(
                PathBuf::from("profile.in"),
                CompileTarget::Jit,
                Some(entry),
                None,
            );
            let target_module = if entry == "main" {
                module.clone()
            } else {
                UnifiedModule::new(Vec::new())
            };
            let result = super::jit::compile_jit(&target_module, "App", &request);
            let actual = antidecomp::current_profile();
            antidecomp::set_profile(previous);
            assert_eq!(result.is_ok(), entry == "main");
            if entry == "main" {
                assert_eq!(result.unwrap().eval_exit_code, Some(42));
            }
            assert_eq!(actual, EmitProfile::Lean, "{entry}");
        }
    }
}

#[test]
fn cross_target_exit_stubs_reject_observable_entry_bodies() {
    let module = crate::in_lang_parse::parse_in_source(
        "fn main() -> Int { print(\"runtime-only\"); return 42; }",
    )
    .unwrap();
    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "armv7-unknown-linux-gnueabihf",
        "x86_64-pc-windows-msvc",
    ] {
        let out = temp_path("side-effect-native");
        let mut request = default_request(
            PathBuf::from("effects.in"),
            CompileTarget::Native,
            Some("main"),
            Some(out.clone()),
        );
        request.target_triple = Some(target.into());
        let result = super::native::compile_native(&module, "App", &request);
        assert!(
            matches!(result, Err(ref error) if error.contains("pure scalar exit entry")),
            "{target}"
        );
        assert!(!out.exists(), "{target} wrote an incomplete artifact");
    }
}

#[test]
fn resolve_jit_entry_empty_name_uses_library_fallback() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "answer".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, ""), "answer");
    assert_eq!(resolve_jit_entry(&UnifiedModule::new(vec![]), ""), "");
}

#[test]
fn native_backends_reject_aggregate_catches() {
    for pattern in ["[value]", "(value, other)"] {
        let mut module = crate::in_lang_parse::parse_in_source(
            "fn main() -> Int { try { throw 42; } catch error { return 1; } return 2; }",
        )
        .unwrap();
        if let Decl::Function { body, .. } = &mut module.decls[0]
            && let crate::core_ir::Stmt::Try { catches, .. } = &mut body[0]
        {
            catches[0].pattern = pattern.into();
        }
        for result in [
            crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                .map(|_| ()),
            crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
        ] {
            let error = result.expect_err("unsupported catch must not run unconditionally");
            assert!(error.contains("unsupported catch pattern"), "{error}");
        }
    }
}

#[test]
fn native_matches_reject_unsupported_and_incompatible_patterns() {
    for body in [
        "match 5 { [value] { return 1; } _ { return 2; } } return 3;",
        "match false { 0 { return 1; } _ { return 2; } } return 3;",
        "match 5 { 5 { return 1; } _ { return 2; } } return 3;",
    ] {
        let mut module =
            crate::in_lang_parse::parse_in_source(&format!("fn main() -> Int {{ {body} }}"))
                .unwrap();
        if body.starts_with("match 5 { 5") {
            module.decls.push(crate::core_ir::Decl::Struct {
                name: "Point".into(),
                fields: vec![("x".into(), crate::core_ir::Typ::Int)],
                type_params: vec![],
            });
            if let crate::core_ir::Decl::Function { body, .. } = &mut module.decls[0]
                && let crate::core_ir::Stmt::Match { arms, .. } = &mut body[0]
            {
                arms[0].pattern = "Point {}".into();
            } else {
                panic!("expected match fixture");
            }
        }
        for optimized in [false, true] {
            if optimized {
                crate::core_opt::optimize_with_profile(
                    &mut module.decls,
                    Some("main"),
                    crate::emit_profile::EmitProfile::Default,
                );
            }
            for result in [
                crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                    .map(|_| ()),
                crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(
                    error.contains("unsupported match pattern")
                        || error.contains("compatible scrutinee")
                        || error.contains("is not an array")
                        || error.contains("unsupported array-len argument"),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn aarch64_aggregate_returns_refuse_register_overflow() {
    let parsed = crate::compiler::rust_front::parse_rust_source("fn wide()->(i64,i64,i64,i64,i64,i64,i64,i64,i64) {(1,2,3,4,5,6,7,8,9)} fn main()->i64 {wide().0}").unwrap();
    let mut module =
        crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
    crate::lower_core::desugar_module(&mut module);
    assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
    let error = crate::native_emit::lower::lower_module(&module, "wide", NativeLinkage::Executable)
        .err()
        .unwrap();
    assert!(error.contains("exceeds eight scalar fields"), "{error}");
}

#[test]
fn native_apis_refuse_nonvoid_fallthrough_across_profiles() {
    for source in [
        "fn main()->Int{42;}",
        "fn helper(flag:Bool)->Int{if flag{return 42;}}fn main()->Int{return helper(false);}",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let _profile = crate::native_emit::antidecomp::ProfileGuard::new(profile);
            for result in [
                crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                    .map(|_| ()),
                crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
                crate::native_emit::thumb_lower::lower_module(&module, "main").map(|_| ()),
                crate::native_emit::c_backend::emit_c_module(&module).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(
                    error.contains("missing-return"),
                    "{source}: {profile:?}: {error}"
                );
            }
        }
    }
    for source in [
        "fn main()->Int{return 42;}",
        "fn main()->Int{while true{continue;}}",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
            .unwrap();
        crate::native_emit::x86_64_lower::lower_module(&module, "main").unwrap();
    }
}

#[test]
fn native_backends_refuse_unresolved_private_compiler_operations() {
    for source in [
        "fn main()->i64 {let pair=(20,22);let borrowed=&pair;let(a,b)=borrowed;*a+*b}",
        "fn main()->i64 {let pair=(20,22);let borrowed=&pair;borrowed.0}",
        "fn answer(value:i64)->i64 {42} fn main()->i64 {let value=1;value.answer()}",
    ] {
        let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        for result in [
            crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                .map(|_| ()),
            crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(
                error.contains("unresolved private compiler operation"),
                "{source}: {error}"
            );
        }
    }
}

#[test]
fn flat_struct_array_patterns_use_typed_record_slots() {
    let parsed = crate::compiler::rust_front::parse_rust_source("struct Point {x:i64,y:i64} fn main()->i64 { let values=[Point{x:20,y:22}]; match values {[Point{x:20,y:22}]=>42,_=>0} }").unwrap();
    let mut module =
        crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
    crate::lower_core::desugar_module(&mut module);
    assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
    crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable).unwrap();
    crate::native_emit::x86_64_lower::lower_module(&module, "main").unwrap();
}

#[test]
fn nested_struct_arrays_refuse_unsupported_layouts() {
    let module = crate::in_lang_parse::parse_in_source("struct Point{Int x;Int y} struct Nested{Point point} fn main()->Int{let values:[Nested]=[Nested{point:Point{x:20,y:22}}];return 42;}").unwrap();
    for result in [
        crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
            .map(|_| ()),
        crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
    ] {
        let error = result.unwrap_err();
        assert!(
            error.contains("unsupported flat record field `Nested.point`"),
            "{error}"
        );
    }
}

#[test]
fn inisa_refuses_matches_without_selection_semantics() {
    for source in [
        "fn main()->Int {match 1 {1 {return 42;} _ {return 0;}} return 0;}",
        "fn main()->Int {let values=[1]; match values {[1] {return 42;} _ {return 0;}} return 0;}",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        assert!(
            crate::native_emit::inisa::compile_program(&module, "main")
                .err()
                .unwrap()
                .contains("match execution is unsupported")
        );
    }
}

#[test]
fn c_patterns_refuse_multiple_rests() {
    let module = crate::in_lang_parse::parse_in_source("fn main()->Int {let values=[1,2]; match values {[1,..,..] {return 1;} _ {return 2;}} return 0;}").unwrap();
    assert!(
        crate::native_emit::c_backend::emit_c_module(&module)
            .unwrap_err()
            .contains("unsupported match pattern")
    );
}

#[test]
fn native_string_patterns_reject_non_string_scrutinees() {
    let module = crate::in_lang_parse::parse_in_source(
        "fn main() -> Int { match 123 { \"text\" { return 1; } _ { return 2; } } return 3; }",
    )
    .unwrap();
    for result in [
        crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
            .map(|_| ()),
        crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
    ] {
        assert!(
            result
                .unwrap_err()
                .contains("string pattern requires a String scrutinee")
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn substring_builtin_copies_bytes_and_checks_ranges() {
    for (text, start, count, expected) in [
        ("abcdefghijk", 1, 9, "bcdefghij"),
        ("éabc", 2, 2, "ab"),
        ("abc", 3, 0, ""),
        ("abc", 0, 3, "abc"),
        ("abc", -1, 1, ""),
        ("abc", 0, -1, ""),
        ("abc", 4, 0, ""),
        ("abc", 2, 2, ""),
        ("abc", i64::MAX, 1, ""),
        ("abc", 1, i64::MAX, ""),
    ] {
        let source = temp_path("substring-range.in");
        fs::write(
            &source,
            format!(
                "fn main() -> String {{ return __inrt_str_substr({text:?}, {start}, {count}); }}"
            ),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(report.success, "{text:?}/{start}/{count}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Str(expected.into())),
            "{text:?}/{start}/{count}"
        );
    }
    let null_source = temp_path("substring-null.in");
    fs::write(
        &null_source,
        "fn main() -> String { return __inrt_str_substr(0, 0, 0); }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        null_source.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(null_source).unwrap();
    assert!(!report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("verify-type-mismatch"));
    let source = temp_path("substring-native.in");
    let binary = temp_path("substring-native");
    fs::write(&source, "fn main() -> Int { let value = __inrt_str_substr(\"abcdefghijk\", 1, 9); if __inrt_str_eq(value, \"bcdefghij\") { return 42; } return 1; }").unwrap();
    let report = compile_owned(&default_request(
        source.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(binary.clone()),
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(
        std::process::Command::new(&binary).status().unwrap().code(),
        Some(42)
    );
    fs::remove_file(source).unwrap();
    fs::remove_file(binary).unwrap();
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vectors_construct_grow_and_preserve_fields() {
    for text in [
        "fn main()->i64 {let mut items=Vec::new();items.push(20);items.push(22);items[0]+items[1]}",
        "fn main()->i64 {let items=vec![20,22];items[0]+items[1]}",
        "fn main()->i64 {let mut items=Vec::new();let mut index=0;while index<10 {items.push(index);index+=1;}items.len()+items[8]+items[9]+15}",
        "struct Bag {items:Vec<i64>} struct Outer {id:i64,bag:Bag} impl Outer {fn fill(&mut self) {let mut index=0;while index<10 {self.bag.items.push(index);index+=1;}}} fn main()->i64 {let mut outer=Outer{id:15,bag:Bag{items:Vec::new()}};outer.fill();outer.bag.items.len()+outer.bag.items[8]+outer.bag.items[9]+outer.id}",
        "fn main()->i64 {let mut items=Vec::new();items.push(20.0);items.push(22.0);(items[0]+items[1]) as i64}",
        "fn main()->i64 {let mut items=Vec::new();items.push(true);items.push(false);if items[0] && !items[1] {42} else {0}}",
    ] {
        let source = temp_path("vector-native.rs");
        let binary = temp_path("vector-native");
        fs::write(&source, text).unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(binary.clone()),
        ));
        assert!(report.success, "{text}: {report:?}");
        assert_eq!(
            std::process::Command::new(&binary).status().unwrap().code(),
            Some(42),
            "{text}"
        );
        fs::remove_file(source).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vector_runtime_refuses_invalid_headers_and_failed_allocations() {
    for text in [
        "fn main()->i64 {__inrt_vector_push(0,42);0}",
        "fn main()->i64 {__inrt_vector_push(1,42);0}",
        "fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.len=1;items.push(42);0}",
        "fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.cap=4;items.push(42);0}",
        "fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.ptr=8;items.cap=576460752303423488;items.push(42);0}",
        "fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.ptr=8;items.len=288230376151711744;items.cap=288230376151711744;items.push(42);0}",
    ] {
        let source = temp_path("vector-invalid-native.rs");
        let binary = temp_path("vector-invalid-native");
        fs::write(&source, text).unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(binary.clone()),
        ));
        assert!(report.success, "{text}: {report:?}");
        let output = std::process::Command::new(&binary).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(crate::inrt::INRT_TRAP_EXIT_CODE as i32),
            "{text}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("invalid vector header or allocation failure"),
            "{output:?}"
        );
        fs::remove_file(source).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vector_clones_preserve_independent_storage_and_returns() {
    for text in [
        "fn main()->i64{let v=vec![42];v.clone().clear();v[0]}",
        "struct Bag{items:Vec<i64>} impl Bag{fn reset(&mut self){self.items.clear();self.items.push(42);}} fn main()->i64{let mut bag=Bag{items:vec![1,2]};bag.reset();bag.items[0]}",
        "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![1]} fn main()->i64{source().clear();unsafe{CALLS+41}}",
        "fn main()->i64{let mut v=Vec::<i64>::with_capacity(10);v.push(1);let cap=v.capacity();v.clear();if v.len()!=0 || v.capacity()!=cap{return 0;}v.push(42);v[0]}",
        "fn main()->i64{let mut v=Vec::<i64>::new();v.clear();v.clear();v.push(42);v[0]}",
        "struct Bag{items:Vec<i64>} struct Outer{bag:Bag} fn main()->i64{let mut o=Outer{bag:Bag{items:vec![1,2]}};o.bag.items.clear();o.bag.items.push(42);o.bag.items[0]}",
        "fn main()->i64{let mut v=vec![1.0];v.clear();v.push(42.0);v[0] as i64}",
        "fn main()->i64{let mut v=vec![false];v.clear();v.push(true);if v.len()==1 && v[0]{42}else{0}}",
        "fn main()->i64{let mut v=Vec::<i64>::new();let mut i=0;while i<10{v.push(i);i+=1;}v.clear();v.extend([20,22]);v[0]+v[1]}",
        "fn clear(v:Vec<i64>)->i64{42} fn main()->i64{let v=vec![1];v.clear()}",
        "fn clear(flag:bool)->i64{0} fn main()->i64{let mut v=vec![1];v.clear();v.push(42);v[0]}",
        "struct Bag{answer:i64} impl Bag{fn clear(&self)->i64{self.answer}} fn clear(flag:bool)->i64{0} fn main()->i64{let bag=Bag{answer:42};bag.clear()}",
        "fn main()->i64{let mut items=vec![20];items.extend(vec![22]);items[0]+items[1]}",
        "mod Vec{pub fn with_capacity(value:i64)->i64{value}} fn main()->i64{Vec::with_capacity(42)}",
        "struct Reader{value:i64} impl Reader{fn take(&self,items:Vec<i64>)->i64{if items.capacity()>=10{self.value}else{0}}} fn main()->i64{let reader=Reader{value:42};reader.take(Vec::with_capacity(10))}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::new()};bag.items=Vec::with_capacity(10);if bag.items.capacity()>=10{42}else{0}}",
        "fn main()->i64{let items:Vec<i64>=Vec::with_capacity(10).clone();if items.capacity()==0 && items.len()==0{42}else{0}}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::with_capacity(10);let before=items.len();items.push(42);if before==0 && items.capacity()>=10{items[0]}else{0}}",
        "fn main()->i64{let mut items=Vec::with_capacity(2);items.push(20);items.push(22);if items.capacity()>=2{items[0]+items[1]}else{0}}",
        "fn main()->i64{let mut items=std::vec::Vec::<f64>::with_capacity(3);items.push(42.0);items[0] as i64}",
        "fn main()->i64{let mut items=alloc::vec::Vec::<_>::with_capacity(3);items.push(true);if items[0]{42}else{0}}",
        "fn source()->Vec<i64>{Vec::with_capacity(10)} fn main()->i64{let mut items=source();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
        "fn take(items:Vec<i64>)->i64{if items.len()==0 && items.capacity()>=10{42}else{0}} fn main()->i64{take(Vec::with_capacity(10))}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::with_capacity(10)};bag.items.push(42);if bag.items.capacity()>=10{bag.items[0]}else{0}}",
        "fn main()->i64{let make=||->Vec<i64>{Vec::with_capacity(10)};let mut items=make();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
        "static mut CALLS:i64=0;fn count()->i64{unsafe{CALLS+=1;}10} fn main()->i64{let mut items=Vec::<i64>::with_capacity(count());items.push(41);unsafe{items[0]+CALLS}}",
        "fn main()->i64{let mut items=Vec::<i64>::with_capacity(0);if items.capacity()!=0{return 0;}items.push(20);items.push(22);items[0]+items[1]}",
        "fn main()->i64{let mut items=Vec::new();items.push(42);if items.capacity()>items.len(){42}else{0}}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::new();let before=items.capacity();items.extend(vec![1,2,3,4,5,6,7,8,9,10]);if before==0 && items.capacity()>=items.len(){42}else{0}}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let bag=Bag{items:vec![20,22]};if bag.items.capacity()>=2{42}else{0}}",
        "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![42]} fn main()->i64{let capacity=source().capacity();unsafe{if capacity>=1 && CALLS==1{42}else{0}}}",
        "fn main()->i64{let items=vec![42];if items.clone().capacity()>=1{42}else{0}}",
        "fn capacity(flag:bool)->i64{0} fn main()->i64{let items=vec![42];if items.capacity()>=1{42}else{0}}",
        "fn capacity(items:Vec<i64>)->i64{42} fn main()->i64{let items=vec![1];items.capacity()}",
        "fn main()->i64{let mut items=vec![20,22];items.extend(vec![]);items[0]+items[1]}",
        "fn main()->i64{let mut items=vec![20.0,22.0];items.extend(vec![]);(items[0]+items[1]) as i64}",
        "fn main()->i64{let mut items=vec![true];items.extend([]);if items[0]{42}else{0}}",
        "fn main()->i64{let mut items=vec![20];let source=[22];items.extend(source);items[0]+items[1]}",
        "fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(vec![1,2,3,4,5,6,7,8,9,10]);items.len()+items[9]+22}",
        "fn main()->i64{let mut items=vec![20,22];let empty:Vec<i64>=vec![];items.extend(empty);items[0]+items[1]}",
        "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![20,21]} fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(source());unsafe{items[0]+items[1]+CALLS}}",
        "fn main()->i64{let mut items=Vec::new();items.extend(vec![20,22]);items[0]+items[1]}",
        "fn extend(items:Vec<bool>,source:Vec<i64>)->i64{42} fn main()->i64{let items=Vec::new();items.extend(vec![1])}",
        "fn source()->Vec<i64>{vec![20,22]} fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(source());items[0]+items[1]}",
        "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:vec![20]};let other=vec![22];bag.items.extend(other);bag.items[0]+bag.items[1]}",
        "fn main()->i64{let mut items=vec![20,1];let source=items.clone();items.extend(source);items[0]+items[2]+items[1]+items[3]}",
        "fn main()->i64{let mut items=vec![true];items.extend(vec![false]);if items[0] && !items[1]{42}else{0}}",
        "fn main()->i64{let mut items=vec![20.0];items.extend(vec![22.0]);(items[0]+items[1]) as i64}",
        "fn main()->i64{let mut items=Vec::<i64>::new();items.push(42);items[0]}",
        "fn main()->i64{let mut items=Vec::<_>::new();items.push(42);items[0]}",
        "fn values()->Vec<i64>{Vec::<_>::new()} fn main()->i64{let mut items=values();items.push(42);items[0]}",
        "fn main()->i64{let mut items=std::vec::Vec::<_>::new();items.push(true);if items[0]{42}else{0}}",
        "fn main()->i64{let mut items=alloc::vec::Vec::<_>::new();items.push(42.0);items[0] as i64}",
        "fn main()->i64{let mut items=std::vec::Vec::new();items.push(42);items[0]}",
        "fn main()->i64{let mut items=alloc::vec::Vec::new();items.push(20);items.push(22);items[0]+items[1]}",
        "fn values()->Vec<i64>{std::vec::Vec::<i64>::new()} fn main()->i64{let mut items=values();items.push(42);items[0]}",
        "fn values()->Vec<i64>{vec![20,22]} fn main()->i64{let mut items=values();items.push(0);items[0]+items[1]}",
        "fn main()->i64 {let mut items=vec![20,22];let copied=items.clone();items[0]=0;copied[0]+copied[1]}",
        "fn main()->i64 {let items=vec![42];items.clone()[0]}",
        "fn main()->i64 {let items:Vec<i64>=Vec::new();let mut copied=items.clone();copied.push(42);items.len()+copied[0]}",
        "fn take(prefix:i64,items:Vec<i64>)->i64 {prefix+items[0]+items[1]} fn main()->i64 {let items=vec![20,21];take(1,items.clone())}",
        "struct Bag {items:Vec<i64>} struct Outer {bag:Bag} fn main()->i64 {let mut outer=Outer{bag:Bag{items:vec![20,22]}};let copied=outer.bag.items.clone();outer.bag.items[0]=0;copied[0]+copied[1]}",
        "static mut CALLS:i64=0;fn values()->Vec<i64> {unsafe{CALLS+=1;}vec![20,21]} fn main()->i64 {let copied=values().clone();unsafe{copied[0]+copied[1]+CALLS}}",
        "fn main()->i64 {let items=vec![20.0,22.0];let mut copied=items.clone();copied[0]=0.0;(items[0]+copied[1]) as i64}",
        "fn main()->i64 {let items=vec![true,false];let mut copied=items.clone();copied[0]=false;if items[0] && !copied[0] && !copied[1] {42} else {0}}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("vector-clone-native.rs");
            let binary = temp_path("vector-clone-native");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Native,
                Some("main"),
                Some(binary.clone()),
            );
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{text} {profile:?}: {report:?}");
            assert_eq!(
                std::process::Command::new(&binary).status().unwrap().code(),
                Some(42),
                "{text} {profile:?}"
            );
            fs::remove_file(source).unwrap();
            fs::remove_file(binary).unwrap();
        }
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vector_allocation_refuses_overflow_and_failed_mapping() {
    for capacity in ["-1", "576460752303423488", "1125899906842624"] {
        let source = temp_path("vector-allocation-invalid.rs");
        let binary = temp_path("vector-allocation-invalid");
        fs::write(
            &source,
            format!(
                "fn main()->i64{{let items=Vec::<i64>::with_capacity({capacity});items.len()}}"
            ),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(binary.clone()),
        ));
        assert!(report.success, "{capacity}: {report:?}");
        let output = std::process::Command::new(&binary).output().unwrap();
        assert_eq!(output.status.code(), Some(70), "{capacity}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("invalid vector header or allocation failure")
        );
        fs::remove_file(source).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vector_extend_refuses_invalid_descriptors() {
    for text in [
        "fn main()->i64{__inrt_vector_extend(0,0,0,0);0}",
        "fn main()->i64{__inrt_vector_extend(1,0,0,0);0}",
        "fn main()->i64{__inrt_vector_extend(8,0,1,1);0}",
        "fn main()->i64{__inrt_vector_extend(8,1,1,1);0}",
        "fn main()->i64{__inrt_vector_extend(8,8,2,1);0}",
        "fn main()->i64{__inrt_vector_extend(8,0,0,1152921504606846976);0}",
    ] {
        let source = temp_path("vector-extend-invalid.rs");
        let binary = temp_path("vector-extend-invalid");
        fs::write(&source, text).unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(binary.clone()),
        ));
        assert!(report.success, "{text}: {report:?}");
        let output = std::process::Command::new(&binary).output().unwrap();
        assert_eq!(output.status.code(), Some(70), "{text}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid vector header"));
        fs::remove_file(source).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_vector_clone_refuses_invalid_descriptors_and_failed_allocations() {
    for text in [
        "fn main()->i64 {__inrt_vector_clone(0,1,0);0}",
        "fn main()->i64 {__inrt_vector_clone(1,1,1);0}",
        "fn main()->i64 {__inrt_vector_clone(0,0,1);0}",
        "fn main()->i64 {__inrt_vector_clone(8,0,1152921504606846976);0}",
        "fn main()->i64 {__inrt_vector_clone(8,576460752303423488,576460752303423488);0}",
    ] {
        let source = temp_path("vector-clone-invalid-native.rs");
        let binary = temp_path("vector-clone-invalid-native");
        fs::write(&source, text).unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(binary.clone()),
        ));
        assert!(report.success, "{text}: {report:?}");
        let output = std::process::Command::new(&binary).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(crate::inrt::INRT_TRAP_EXIT_CODE as i32),
            "{text}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("invalid vector header or allocation failure"),
            "{output:?}"
        );
        fs::remove_file(source).unwrap();
        fs::remove_file(binary).unwrap();
    }
}

#[test]
fn owned_jit_uses_runtime_builtin_result_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (typ, expression, expected) in [
        ("String", "\"\"", EvalValue::Str(String::new())),
        ("String", "\"é🙂\"", EvalValue::Str("é🙂".into())),
        (
            "String",
            "path_join(\"/tmp\", \"file\")",
            EvalValue::Str("/tmp/file".into()),
        ),
        (
            "String",
            "__inrt_str_concat(\"a\", \"b\")",
            EvalValue::Str("ab".into()),
        ),
        (
            "String",
            "__inrt_str_substr(\"abcd\", 1, 2)",
            EvalValue::Str("bc".into()),
        ),
        ("Bool", "__inrt_str_eq(\"a\", \"a\")", EvalValue::Bool(true)),
        (
            "Bool",
            "__inrt_str_contains(\"abcd\", \"bc\")",
            EvalValue::Bool(true),
        ),
    ] {
        let source = temp_path("builtin-result.in");
        fs::write(
            &source,
            format!("fn main() -> {typ} {{ return {expression}; }}"),
        )
        .unwrap();
        for out in [None, Some(temp_path("builtin-result.jit"))] {
            let report = compile_owned(&default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("main"),
                out,
            ));
            assert!(report.success, "{expression}: {report:?}");
            assert_eq!(report.eval_result, Some(expected.clone()), "{expression}");
        }
        fs::remove_file(source).unwrap();
    }
}

#[test]
fn owned_jit_preserves_caught_scalar_payload_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (typ, value, expected) in [
        ("Int", "7", EvalValue::Int(7)),
        ("Bool", "true", EvalValue::Bool(true)),
        ("Float", "3.75", EvalValue::Float(3.75)),
        ("String", "\"value\"", EvalValue::Str("value".into())),
    ] {
        let source = temp_path("catch-payload.in");
        fs::write(
            &source,
            format!(
                "fn main() -> {typ} {{ try {{ throw {value}; }} catch error {{ return error; }} }}"
            ),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(report.success, "{typ}: {report:?}");
        assert_eq!(report.eval_result, Some(expected), "{typ}");
    }
}

#[test]
fn owned_jit_rejects_constant_writes_in_exception_bodies() {
    for body in [
        "try { count = 7; } catch error { return 9; } return count;",
        "try { throw 42; } catch error { count = 7; } return count;",
    ] {
        let source = temp_path("try-constant.in");
        fs::write(
            &source,
            format!("const count: Int = 0;\nfn main() -> Int {{ {body} }}"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(!report.success, "{body}: {report:?}");
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cannot assign to constant global")),
            "{body}: {report:?}"
        );
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_jit_preserves_error_type_through_calls_and_rethrows() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for body in [
            "try { fail(); } catch 0 { return 1; } catch false { return 2; } return 3;",
            "try { try { fail(); } catch value { let alias = value; throw alias; } } catch 0 { return 1; } catch false { return 2; } return 3;",
            "try { throw 0 == 1; } catch 0 { return 1; } catch false { return 2; } return 3;",
        ] {
            let source = temp_path("typed-error.in");
            fs::write(
                &source,
                format!(
                    "fn fail() -> Int {{ throw false; return 0; }}\nfn main() -> Int {{ {body} }}"
                ),
            )
            .unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {body}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(2)),
                "{profile:?}: {body}"
            );
        }
    }
}

#[test]
fn owned_jit_selects_catches_and_reports_unmatched_errors() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (catches, handled) in [
            ("catch 41 { return 1; } catch 42 { return 2; }", true),
            ("catch 41 { return 1; }", false),
        ] {
            let source = temp_path("catch-dispatch.in");
            fs::write(&source, format!("fn fail() -> Int {{ throw 42; return 0; }}\nfn main() -> Int {{ try {{ fail(); }} {catches} return 3; }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert_eq!(report.success, handled, "{profile:?}: {report:?}");
            if handled {
                assert_eq!(report.eval_result, Some(EvalValue::Int(2)));
            } else {
                assert_eq!(
                    report.error.as_deref(),
                    Some("jit-uncaught-error: entry `main` threw 42")
                );
                assert_eq!(report.eval_result, None);
            }
        }
    }
}

#[test]
fn native_backends_reject_dynamic_call_targets() {
    use crate::core_ir::{Expr, Stmt};
    for callee in [
        Expr::Field {
            base: Box::new(Expr::IntLit(0)),
            name: "method".into(),
        },
        Expr::Call {
            callee: Box::new(Expr::Ident("make".into())),
            args: vec![],
        },
        Expr::IntLit(42),
    ] {
        let mut module =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 0; }").unwrap();
        if let Decl::Function { body, .. } = &mut module.decls[0] {
            *body = vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(callee),
                args: vec![],
            }))];
        }
        for result in [
            crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                .map(|_| ()),
            crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
        ] {
            let error = result.expect_err("dynamic call cannot silently return zero");
            assert!(error.contains("dynamic call target unsupported"), "{error}");
        }
        if native_backend::native_subset_host_available() {
            let request = default_request(
                PathBuf::from("dynamic-call.in"),
                CompileTarget::Jit,
                Some("main"),
                None,
            );
            let error = super::jit::compile_jit(&module, "App", &request)
                .err()
                .expect("dynamic call must fail");
            assert!(error.starts_with("jit-lowering-failed:"), "{error}");
            assert!(error.contains("dynamic call target unsupported"), "{error}");
        }
    }
}

#[test]
fn owned_jit_reports_uncaught_errors_before_decoding_results() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (typ, tail) in [
            ("Int", "return 7;"),
            ("Bool", "return true;"),
            ("Float", "return 1.5;"),
            ("String", "return \"value\";"),
            ("void", "return;"),
        ] {
            let source = temp_path("uncaught-error.in");
            fs::write(&source, format!("fn fail() -> Int {{ throw 42; return 0; }}\nfn main() -> {typ} {{ fail(); {tail} }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(!report.success, "{profile:?}/{typ}: {report:?}");
            assert_eq!(
                report.error.as_deref(),
                Some("jit-uncaught-error: entry `main` threw 42"),
                "{profile:?}/{typ}: {report:?}"
            );
            assert_eq!(report.eval_result, None);
            assert_eq!(report.eval_exit_code, None);
        }
    }
}

#[test]
fn owned_jit_stops_effects_after_throwing_calls() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for main in [
            "try { throw 42; count = 7; } catch error { return count; } return -1;",
            "try { fail(); count = 8; } catch error { return count; } return -1;",
        ] {
            let source = temp_path("throw-flow.in");
            fs::write(&source, format!("var count: Int = 0;\nfn fail() -> Int {{ throw 42; count = 7; return 0; }}\nfn main() -> Int {{ {main} }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {main}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(0)),
                "{profile:?}: {main}"
            );
        }
    }
}

#[test]
fn owned_jit_preserves_shadowed_bindings() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (text, expected) in [
            (
                "var count: Int = 0;\nfn read() -> Int { return count; }\nfn main() -> Int { count = 42; let count = 7; return read(); }",
                42,
            ),
            (
                "fn shadow(x: Int) -> Int { let x = x + 1; return x; }\nfn main() -> Int { return shadow(41); }",
                42,
            ),
            (
                "fn main() -> Int { let x = 1; let before = x; let x = 7; return before + x; }",
                8,
            ),
            (
                "fn main() -> Int { var x = 1; if true { let x = 7; x = x + 1; } return x; }",
                1,
            ),
        ] {
            let source = temp_path("shadowed-bindings.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {text}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{profile:?}: {text}"
            );
        }
    }
}

#[test]
fn inlining_preserves_argument_call_evaluation() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (callee, main, expected) in [
            (
                "fn twice(x: Int) -> Int { return x + x; }",
                "return twice(next());",
                2,
            ),
            (
                "fn twice(x: Int) -> Int { return x + x; }",
                "let result = twice(next()); return result;",
                2,
            ),
            (
                "fn unused(x: Int) -> Int { return 7; }",
                "let result = unused(next()); return count;",
                1,
            ),
            (
                "fn unused(x: Int) -> Void { return; }",
                "unused(next()); return count;",
                1,
            ),
            (
                "fn combine(x: Int) -> Int { return next() + x; }",
                "return combine(count);",
                1,
            ),
            (
                "fn combine() -> Int { let snapshot = count; return next() + snapshot; }",
                "return combine();",
                1,
            ),
            (
                "fn addtwice(x: Int) -> Void { count = count + x; count = count + x; }",
                "count = 1; addtwice(count); return count;",
                3,
            ),
            (
                "fn helper() -> Void { let temp = 7; count = temp; }",
                "let temp = 42; helper(); return count + temp;",
                49,
            ),
            (
                "fn helper() -> Void { return; count = 2; }",
                "count = 1; helper(); return count;",
                1,
            ),
            (
                "var shared: Int = 42;\nfn read() -> Int { return shared; }",
                "let shared = 7; return read();",
                42,
            ),
            (
                "var shared: Int = 42;\nfn read() -> Int { let copy = shared; return copy; }",
                "let shared = 7; return read();",
                42,
            ),
        ] {
            let source = temp_path("inline-argument-effects.in");
            fs::write(&source, format!("var count: Int = 0;\nfn next() -> Int {{ count = count + 1; return count; }}\n{callee}\nfn main() -> Int {{ {main} }}\n")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {main}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{profile:?}: {main}: {report:?}"
            );
        }
    }
}

#[test]
fn harden_jit_preserves_typed_float_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = temp_path("harden-float.in");
    fs::write(
        &source,
        "var x: Float = 1.25;\nfn main() -> Float { x = 1.25; return x + 2.5; }\n",
    )
    .unwrap();
    let mut request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
    request.profile = crate::emit_profile::EmitProfile::Harden;
    let report = compile_owned(&request);
    fs::remove_file(source).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(
        report.eval_result,
        Some(EvalValue::Float(3.75)),
        "{report:?}"
    );
    assert_eq!(report.runtime_level, "inrt-jit");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn harden_native_preserves_host_target_and_executes_at_runtime() {
    let source = temp_path("harden-native.in");
    let out = temp_path("harden-native");
    fs::write(&source, "fn main() -> Int { return 42; }\n").unwrap();
    let mut request = default_request(
        source.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(out.clone()),
    );
    request.profile = crate::emit_profile::EmitProfile::Harden;
    let report = compile_owned(&request);
    fs::remove_file(source).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.runtime_level, "inrt-native");
    assert_eq!(&fs::read(&out).unwrap()[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    let status = std::process::Command::new(&out).status().unwrap();
    fs::remove_file(out).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn owned_rust_jit_executes_primitive_entries() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for (source, expected) in [
        ("fn main() -> i64 { return 42; }", EvalValue::Int(42)),
        (
            "fn main() -> f64 { return 1.25 + 2.5; }",
            EvalValue::Float(3.75),
        ),
        ("fn main() -> bool { return true; }", EvalValue::Bool(true)),
        (
            "fn main() -> i64 { let mut sum = 0; for mut item in 1..4 { sum = sum + item; item = 99; } return sum; }",
            EvalValue::Int(6),
        ),
        (
            "struct Point { x: i64 } fn unused(point: Point) -> Point { return point; } fn main() -> i64 { return 42; }",
            EvalValue::Int(42),
        ),
    ] {
        let path = temp_path("rust-jit.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("jit-executed"),
            "{report:?}"
        );
        assert_eq!(report.eval_result, Some(expected));
    }
}

#[test]
fn owned_rust_jit_retains_compile_only_for_uninvocable_entries() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for source in [
        "struct Point { x: i64 } static VALUES: [i64;2]=[20,22]; fn main() -> Point { return Point { x: 42 }; }",
        "struct Point { x: i64 } fn main() -> Point { return Point { x: 42 }; }",
        "fn main(value: i64) -> i64 { return value; }",
        "fn helper() -> i64 { return 42; }",
    ] {
        let path = temp_path("rust-abi.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            None,
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert_eq!(report.eval_exit_code, None);
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_integer_inference_executes_backward_unsigned_constraints() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("rust-integer-inference-unsigned.rs");
    fs::write(&path, "fn next(at:usize)->usize{at+1}fn main()->i64{let opening=1;let delimiter=opening+1;let mut at=delimiter;at=next(at);if at==3usize{42}else{7}}")
        .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{profile:?}: {report:?}");
        assert!(report.execution_started, "{profile:?}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn owned_rust_integer_inference_keeps_valid_selfhost_compile_only() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("rust-integer-inference-selfhost.rs");
    fs::write(
        &path,
        include_str!("../../tests/fixtures/rust_integer_inference_selfhost.rs"),
    )
    .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, None, None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{profile:?}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert!(!report.execution_started);
        assert_eq!(report.eval_exit_code, None);
        assert_eq!(report.eval_result, None);
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn owned_rust_compile_only_reports_unsupported_global_data_without_execution() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-static-ir-only.rs");
    fs::write(&path, "struct Bag { value: i64 } static VALUE: i64=make(); fn make() -> i64 { return 42; } fn main() -> Bag { return Bag { value: 42 }; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    fs::remove_file(&path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-ir-only"));
    assert_eq!(report.backend_level, "owned-core-ir-only");
    assert_eq!(report.runtime_level, "not-executed");
    assert_eq!(report.eval_result, None);
    assert_eq!(report.eval_exit_code, None);
    assert_eq!(report.artifact_path, None);
    assert!(
        report
            .reason
            .unwrap()
            .contains("unsupported native initializer for global `VALUE`")
    );
    let path = temp_path("rust-static-executable.rs");
    fs::write(
        &path,
        "static VALUE: i64=make(); fn make() -> i64 { return 42; } fn main() -> i64 { return 42; }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    fs::remove_file(&path).unwrap();
    assert!(!report.success, "{report:?}");
    assert_eq!(report.eval_result, None);
}

#[test]
fn owned_rust_compile_only_keeps_unsupported_type_contracts_without_executing() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let unsupported = "struct Row;trait Other{type Output;}impl Other for Row{type Output=i64;}trait Factory{type Output;fn make()->Self::Output;}impl Factory for Row{type Output=<Self as Other>::Output;fn make()->Self::Output{42}}fn main()->i64{<Row as Factory>::make()}";
    let invalid = "struct Row{x:i64}impl std::ops::Neg for Row{type Output=Self::Output;fn neg(self)->Self::Output{77}}fn main()->i64{-Row{x:42}}";
    let unused_invalid = format!(
        "{unsupported} struct Bad;trait Broken{{type Output;fn work()->i64;}}impl Broken for Bad{{type Output=Self::Output;fn work()->i64{{let unused:Self::Output;42}}}}"
    );
    let mixed = unused_invalid.replace(
        "fn main()->i64{<Row as Factory>::make()}",
        "fn main()->i64{<Row as Factory>::make()+<Bad as Broken>::work()}",
    );
    for (name, source, entry, expected) in [
        ("unsupported", unsupported, None, true),
        ("explicit-entry", unsupported, Some("main"), false),
        ("invalid", invalid, None, false),
        ("unused-invalid", unused_invalid.as_str(), None, false),
        ("mixed", mixed.as_str(), None, false),
    ] {
        let path = temp_path(&format!("rust-type-proof-ir-only-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, entry, None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert_eq!(report.success, expected, "{name} {profile:?}: {report:?}");
            assert!(!report.execution_started, "{report:?}");
            assert_eq!(report.eval_result, None);
            assert_eq!(report.eval_exit_code, None);
            assert_eq!(report.artifact_path, None);
            if expected {
                assert_eq!(report.reason_code.as_deref(), Some("jit-ir-only"));
                assert_eq!(report.backend_level, "owned-core-ir-only");
                assert_eq!(report.runtime_level, "not-executed");
            } else if name == "unused-invalid" {
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("rust-invalid-associated-type")
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn owned_rust_compile_only_preserves_unsupported_pattern_shadowing() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn helper(len:u64)->i64{let Ok(len)=usize::try_from(len)else{return 7;};let end:usize=len;end as i64}fn main()->i64{helper(42u64)}",
        "fn helper(len:u64,align:u64)->i64{let(Ok(align),Ok(len))=(usize::try_from(align.max(1)),usize::try_from(len))else{return 7;};let end:usize=len+align;end as i64}fn main()->i64{helper(41u64,1u64)}",
    ] {
        let path = temp_path("rust-pattern-shadow-ir-only.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, None, None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{source} {profile:?}: {report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("jit-ir-only"));
            assert_eq!(report.backend_level, "owned-core-ir-only");
            assert_eq!(report.runtime_level, "not-executed");
            assert!(!report.execution_started);
            assert_eq!(report.eval_result, None);
            assert_eq!(report.eval_exit_code, None);
            assert_eq!(report.artifact_path, None);
            assert_eq!(report.executable_path, None);
            assert!(report.external_invocations.is_empty());
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn owned_rust_jit_verifies_before_execution() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-unresolved.rs");
    fs::write(&path, "fn main() -> i64 { return missing_value; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(&path).unwrap();
    assert!(!report.success, "{report:?}");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unresolved identifier")),
        "{report:?}"
    );
    assert_eq!(report.eval_exit_code, None);
}

#[test]
fn owned_rust_jit_inclusive_ranges_include_the_endpoint() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let mut sum = 0; for i in 1..=3 { sum = sum + i; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 7..=3 { count = count + 1; } return count; }",
            0,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 9223372036854775807..=9223372036854775807 { count = count + 1; } return count; }",
            1,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 9223372036854775806..=9223372036854775807 { count = count + 1; continue; } return count; }",
            2,
        ),
        (
            "fn main() -> i64 { let mut end = 3; let mut sum = 0; for mut i in 1..=end { sum = sum + i; end = 0; i = 99; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 1..=2 { for i in 1..=2 { count = count + 1; continue; } } return count; }",
            4,
        ),
    ] {
        let path = temp_path("rust-inclusive.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_jit_loop_temporaries_preserve_user_names() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let __for_items_0 = 100; let mut sum = 0; for item in [1, 2, 3] { sum = sum + item; } return __for_items_0 + sum; }",
            106,
        ),
        (
            "fn main() -> i64 { let __for_0 = 100; let __for_end_0 = 200; let mut sum = 0; for item in 1..3 { sum = sum + item; } return __for_0 + __for_end_0 + sum; }",
            303,
        ),
        (
            "fn sum(__for_0: i64) -> i64 { let mut result = 0; for item in 1..=3 { result = result + item; } return __for_0 + result; } fn main() -> i64 { return sum(100); }",
            106,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for mut __for_0 in 1..=3 { sum = sum + __for_0; __for_0 = 99; } return sum; }",
            6,
        ),
        (
            "fn __for_0() -> i64 { return 100; } fn main() -> i64 { let mut sum = 0; for item in 1..3 { sum = sum + item; } return __for_0() + sum; }",
            103,
        ),
    ] {
        let path = temp_path("rust-loop-names.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_jit_complement_preserves_operand_type() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> bool { let len = true; return !len; }",
            EvalValue::Bool(false),
        ),
        ("fn main() -> i64 { return !1_i64; }", EvalValue::Int(-2)),
        (
            "fn main() -> i64 { let value = 3; return !value; }",
            EvalValue::Int(-4),
        ),
        (
            "fn invert(value: i64) -> i64 { return !value; } fn main() -> i64 { return invert(3); }",
            EvalValue::Int(-4),
        ),
        (
            "fn number(value: i64) -> i64 { if value == 0 { return 1; } else { return 2; } } fn main() -> i64 { return !number(0); }",
            EvalValue::Int(-2),
        ),
        ("fn main() -> i64 { return !(1 + 2); }", EvalValue::Int(-4)),
        ("fn main() -> i64 { return !!1; }", EvalValue::Int(1)),
        (
            "fn main() -> bool { return !true; }",
            EvalValue::Bool(false),
        ),
        (
            "fn invert(value: bool) -> bool { return !value; } fn main() -> bool { return invert(false); }",
            EvalValue::Bool(true),
        ),
        (
            "fn main() -> bool { return !(1 < 2); }",
            EvalValue::Bool(false),
        ),
        (
            "fn main() -> i64 { let values = [1, 2]; return !values[0]; }",
            EvalValue::Int(-2),
        ),
    ] {
        let path = temp_path("rust-complement.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(expected), "{source}");
    }
}

#[test]
fn owned_rust_jit_complement_refuses_nonintegral_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> f64 { return !1.25; }",
        "fn main() -> String { return !\"text\"; }",
    ] {
        let path = temp_path("rust-invalid-complement.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{report:?}");
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("complement operand")),
            "{report:?}"
        );
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_in_jit_bitwise_complement_handles_negative_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/types/bitwise-complement.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_jit_preserves_conditional_returns_and_tail_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn answer(value: bool) -> i64 { if value { return 41; } return 42; } fn main() -> i64 { return answer(true) + answer(false); }",
        "fn answer(value: bool) -> i64 { if value { return 41; } else { let ignored = 99; } return 42; } fn main() -> i64 { return answer(true) + answer(false); }",
        "fn answer(value: bool) -> i64 { if value { 41 } else { 42 } } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { return if value { 41 } else { 42 }; } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { match value { true => 41, false => 42 } } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { return match value { true => 41, false => 42 }; } fn main() -> i64 { answer(true) + answer(false) }",
    ] {
        let path = temp_path("rust-return-flow.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Int(83)), "{source}");
    }
    for source in [
        "fn main() -> i64 { { 99; } return 42; }",
        "fn main() -> i64 { { let value = 42; value } }",
        "fn main() -> i64 { match true { true => { let ignored = 1; }, false => { let ignored = 2; } }; return 42; }",
        "fn main() -> i64 { if false { return 1; } else if false { return 2; } else { let ignored = 3; } return 42; }",
    ] {
        let path = temp_path("rust-statement-flow.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_rust_jit_refuses_missing_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { 42; }",
        "fn answer(value: i64) -> i64 { match value { 1 => 41, 2 => 42 } } fn main() -> i64 { answer(3) }",
    ] {
        let path = temp_path("rust-missing-value.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_jit_unit_tails_execute_in_the_enclosing_function() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() { for item in 0..3 { let ignored = item; } }",
        "fn main() { while false { } }",
        "fn main() { loop { break; } }",
        "fn main() { let mut value = 0; value = 1 }",
        "fn main() { let mut value = 0; value += 1 }",
    ] {
        let path = temp_path("rust-unit-tail.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, None);
        assert_eq!(report.eval_exit_code, Some(0));
    }
}

#[test]
fn owned_rust_jit_tail_branches_preserve_primitive_result_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn answer(value: bool) -> f64 { if value { 1.25 } else { 2.75 } } fn main() -> f64 { answer(true) + answer(false) }",
            EvalValue::Float(4.0),
        ),
        (
            "fn answer(value: bool) -> bool { if value { false } else { true } } fn main() -> bool { answer(false) }",
            EvalValue::Bool(true),
        ),
        (
            "fn answer(value: bool) -> &'static str { if value { \"left\" } else { \"right\" } } fn main() -> &'static str { answer(false) }",
            EvalValue::Str("right".into()),
        ),
    ] {
        let path = temp_path("rust-tail-types.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(expected));
    }
}

#[test]
fn owned_rust_jit_match_guards_select_the_first_successful_arm() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { match true { true if false => 1, _ => 42 } }",
        "fn accepted(value: i64) -> bool { if value == 3 { return true; } else { return false; } } fn main() -> i64 { match 3 { value if value < 0 => 1, value if accepted(value) => value + 39, _ => 2 } }",
        "fn main() -> i64 { let value = 42; match 3 { value if false => 1, _ => value } }",
        "fn main() -> i64 { let __match_value_0 = 40; let __match_done_0 = 2; match 3 { value if value > 0 => __match_value_0 + __match_done_0, _ => 0 } }",
    ] {
        let path = temp_path("rust-match-guards.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_guarded_matches_share_lowering_across_frontends() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for fixture in ["match-guard-effects.in", "match-guard.icore"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../conformance/control-flow")
            .join(fixture);
        let report = compile_owned(&default_request(
            path,
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        assert!(report.success, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{fixture}");
    }
}

#[test]
fn owned_match_guards_require_boolean_conditions() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (suffix, source) in [
        (
            "rs",
            "fn bad() -> bool { return 1; } fn main() -> i64 { match true { true if bad() => 1, _ => 42 } }",
        ),
        ("rs", "fn main() -> i64 { if 1 { 42 } else { 0 } }"),
        (
            "rs",
            "fn main() -> i64 { match 3 { value if 1 => 1, _ => 42 } }",
        ),
        (
            "in",
            "fn main() -> Int { match 3 { value if 1 { return 1; } _ { return 42; } } }",
        ),
    ] {
        let path = temp_path(&format!("invalid-guard.{suffix}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn raw_native_guard_lowering_preserves_effect_order() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = include_str!("../../../conformance/control-flow/match-guard-effects.in");
    let module = crate::in_lang_parse::parse_in_source(source).unwrap();
    let request = default_request(
        PathBuf::from("direct-guard.in"),
        CompileTarget::Jit,
        Some("main"),
        None,
    );
    let result = super::jit::compile_jit(&module, "GuardEffects", &request).unwrap();
    assert_eq!(result.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_user_range_is_not_replaced_by_numeric_iteration() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-user-range.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> [i64; 2] { return [10, 20]; } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
    assert_eq!(report.eval_result, Some(EvalValue::Int(30)));
}

#[test]
fn owned_rust_range_syntax_does_not_call_a_user_range_function() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-range-syntax.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> i64 { return 99; } fn main() -> i64 { let mut sum = 0; for item in 1..4 { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(6)));
}

#[test]
fn owned_rust_noninlined_range_call_retains_its_array_abi() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-range-array-abi.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> [i64; 2] { if start == 1 { return [10, 20]; } else { return [30, 40]; } } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
    assert!(report.execution_started);
    assert_eq!(report.eval_result, Some(EvalValue::Int(30)));
    assert_eq!(report.eval_exit_code, Some(30));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn raw_x86_user_range_iterator_executes_the_function_result() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = "fn range(start: i64, end: i64) -> [i64; 2] { if start == 1 { return [10, 20]; } else { return [30, 40]; } } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }";
    let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
    let request = default_request(
        PathBuf::from("direct-range.in"),
        CompileTarget::Jit,
        Some("main"),
        None,
    );
    let result = super::jit::compile_jit(&module, "RangeCall", &request).unwrap();
    assert_eq!(result.eval_result, Some(EvalValue::Int(30)));
}

#[test]
fn owned_rust_open_ranges_iterate_from_their_start() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { for item in 1.. { return item; } return 2; }",
            1,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for mut item in 1.. { sum = sum + item; if item == 3 { break; } item = 99; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for item in 1.. { if item < 3 { continue; } sum = sum + item; if item == 4 { break; } } return sum; }",
            7,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for item in 9223372036854775807.. { count = count + 1; if count == 2 { return item; } } return 0; }",
            i64::MIN,
        ),
    ] {
        let path = temp_path("rust-open-range.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_in_open_ranges_evaluate_the_start_once() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/control-flow/range-open-effects.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(8)));
}

#[test]
fn owned_range_iterators_do_not_invent_missing_bounds() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (suffix, source) in [
        (
            "rs",
            "fn main() -> i64 { for item in ..3 { return item; } return 42; }",
        ),
        (
            "rs",
            "fn main() -> i64 { for item in ..=3 { return item; } return 42; }",
        ),
        (
            "rs",
            "fn main() -> i64 { for item in .. { return item; } return 42; }",
        ),
        (
            "in",
            "fn main() -> Int { for item in ..3 { return item; } return 42; }",
        ),
        (
            "in",
            "fn main() -> Int { for item in 1..= { return item; } return 42; }",
        ),
    ] {
        let path = temp_path(&format!("invalid-range.{suffix}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_array_repeats_preserve_count() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let mut sum = 0; for value in [7; 3] { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let values = [7; 3]; let mut sum = 0; for value in values { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [7; 0] { sum = sum + value; } return sum; }",
            0,
        ),
        (
            "const COUNT: usize = 1 + 2; fn main() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [true; 3] { if value { sum = sum + 1; } } return sum; }",
            3,
        ),
        ("const N: i64 = 42; fn main() -> i64 { N }", 42),
        (
            "fn main() -> i64 { let __array_repeat_0 = 100; let mut sum = 0; for value in [7; 3] { sum = sum + value; } return sum + __array_repeat_0; }",
            121,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [\"x\"; 3] { if value == \"x\" { sum = sum + 1; } } return sum; }",
            3,
        ),
        (
            "const COUNT: usize = 3; fn main() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } let COUNT = 0; return sum + COUNT; }",
            21,
        ),
    ] {
        let path = temp_path("rust-array-repeat.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_array_repeats_preserve_effect_order() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { var sum: Int = 0; for value in array-repeat(step(), 3) { sum += value; } return sum + calls * 10; }",
            13,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { var sum: Int = 0; for value in array-repeat(step(), 0) { sum += value; } return calls; }",
            1,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { if false && array-len(array-repeat(step(), 3)) > 0 { return 99; } return calls; }",
            0,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { while array-repeat(step(), 1)[0] < 3 { continue; } return calls; }",
            3,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn combine(a: Int, b: Int, c: Int) -> Int { return a + b * 10 + c * 100; } fn main() -> Int { return combine(step(), array-len(array-repeat(step(), 3)), step()) + calls * 1000; }",
            3331,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; throw 5; } fn main() -> Int { try { let values: [Int] = array-repeat(step(), 0); return 9; } catch e { return calls + 41; } }",
            42,
        ),
        (
            "fn main() -> Int { var count: Int = 0; for item in [\"..\", \"x\"] { count += 1; } return count; }",
            2,
        ),
        (
            "var calls: Int = 0; fn index() -> Int { calls += 1; return 0; } fn value() -> Int { calls += 1; return calls; } fn main() -> Int { let values: [Int] = [0]; index-assign-value-first(values, index(), value()); return values[0] * 10 + calls; }",
            12,
        ),
        (
            "var calls: Int = 0; fn index() -> Int { calls += 1; return 0; } fn value() -> Int { throw 5; } fn main() -> Int { let values: [Int] = [0]; try { index-assign-value-first(values, index(), value()); return 9; } catch e { return calls + 42; } }",
            42,
        ),
    ] {
        let path = temp_path("array-repeat-effects.in");
        fs::write(
            &path,
            source.replace("; fn", ";\nfn").replace("} fn", "}\nfn"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_array_repeat_counts_require_supported_constants() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let count = 3; let mut sum = 0; for value in [7; count] { sum = sum + value; } return sum; }",
        "const COUNT: usize = 3; fn main() -> i64 { let COUNT = 1; let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; }",
        "fn main() -> i64 { let mut sum = 0; for value in [7; -1] { sum = sum + value; } return sum; }",
        "fn main() -> i64 { let mut sum = 0; for value in [7; 4097] { sum = sum + value; } return sum; }",
    ] {
        let path = temp_path("unsupported-repeat-count.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_class_array_repeats_use_field_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/classes/class-array-repeat.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_scalar_struct_parameters_preserve_layout_and_stack_arguments() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Pair {\n Int a\n Int b\n}\nfn score(pair: Pair, z: Int) -> Int { var sum: Int = 0; for i in 0..2 { sum += pair.a + pair.b; } return sum + z; }\nfn main() -> Int { let pair = Pair { a: 3, b: 4 }; return score(pair, 28); }",
        "struct Pair {\n Int a\n Int b\n}\nfn score(pair: Pair, z: Int) -> Int { var sum: Int = 0; for i in 0..2 { sum += pair.a + pair.b; } return sum + z; }\nfn main() -> Int { return score(Pair { a: 3, b: 4 }, 28); }",
        "struct Pair {\n Int a\n Int b\n}\nfn score(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, pair: Pair) -> Int { var sum: Int = 0; for i in 0..1 { sum += a + b + c + d + e + f + pair.a + pair.b; } return sum; }\nfn main() -> Int { let pair = Pair { a: 10, b: 11 }; return score(1, 2, 3, 4, 5, 6, pair); }",
    ] {
        let path = temp_path("scalar-struct-arguments.in");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_rust_compile_time_constants_copy_values_at_use() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "const TEXT: &str = \"constant\"; fn main() -> &str { TEXT }",
            EvalValue::Str("constant".into()),
        ),
        (
            "const VALUES: [i64; 2] = [7, 14]; fn main() -> i64 { let mut first = VALUES; first[0] = 100; let second = VALUES; first[1] + second[0] + second[1] + 7 }",
            EvalValue::Int(42),
        ),
        (
            "mod first { pub const COUNT: usize = 3; pub fn calc() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; } } mod second { pub const COUNT: usize = 2; pub fn calc() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; } } fn main() -> i64 { first::calc() + second::calc() + 7 }",
            EvalValue::Int(42),
        ),
    ] {
        let path = temp_path("compile-time-constants.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(expected), "{source}");
    }
}

#[test]
fn owned_large_repeat_respects_native_frame_limits() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("large-repeat.rs");
    fs::write(&path, "fn main() -> i64 { let mut sum = 0; for value in [7; 600] { sum = sum + value; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    if cfg!(target_arch = "aarch64") {
        assert!(!report.success, "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("stack frame")),
            "{report:?}"
        );
        assert_eq!(report.eval_result, None);
    } else {
        assert!(report.success, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(4200)));
    }
}

#[test]
fn owned_rust_indexed_compound_assignments_store_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, initial, rhs, expected) in [
        ("+", 7, 21, 28),
        ("-", 7, 2, 5),
        ("*", 7, 3, 21),
        ("/", 21, 3, 7),
        ("%", 21, 4, 1),
        ("&", 7, 3, 3),
        ("|", 4, 3, 7),
        ("^", 7, 3, 4),
        ("<<", 3, 2, 12),
        (">>", 12, 2, 3),
    ] {
        let path = temp_path("indexed-compound-store.rs");
        let source = format!(
            "fn main() -> i64 {{ let mut values = [{initial}, 14]; values[0] {op}= {rhs}; values[0] + values[1] }}"
        );
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected + 14)),
            "{source}"
        );
    }
}

#[test]
fn owned_indexed_compound_stores_preserve_value_first_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "var calls: Int = 0;\nfn index() -> Int { calls += 1; return 0; }\nfn value() -> Int { calls += 1; return calls; }\nfn main() -> Int { let values: [Int] = [10]; index-modify-value-first(values, index(), value(), \"+\"); return values[0] * 10 + calls; }",
            112,
        ),
        (
            "var calls: Int = 0;\nfn index() -> Int { calls += 1; return 0; }\nfn value() -> Int { throw 5; }\nfn main() -> Int { let values: [Int] = [10]; try { index-modify-value-first(values, index(), value(), \"+\"); return 9; } catch e { return calls + 42; } }",
            42,
        ),
        (
            "fn value(values: [Int]) -> Int { values[0] = 100; return 1; }\nfn main() -> Int { let values: [Int] = [10]; index-modify-value-first(values, 0, value(values), \"+\"); return values[0]; }",
            101,
        ),
        (
            "fn main() -> Int { let values: [Int] = [0, 0, 0]; values[1 + 1] = 7; return values[2]; }",
            7,
        ),
        (
            "fn write(values: [Int]) -> Int { values[1] = 99; return 7; }\nfn main() -> Int { let values: [Int] = [10]; return write(values) + values[0]; }",
            11,
        ),
    ] {
        let path = temp_path("compound-store-effects.in");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_float_indexed_compound_assignment() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("float-indexed-compound.rs");
    fs::write(
        &path,
        "fn main() -> f64 { let mut values = [1.5]; values[0] += 2.0; values[0] }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Float(3.5)));
}

#[test]
fn owned_rust_field_compound_assignments_update_original_storage() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, initial, rhs, expected) in [
        ("+", 21, 21, 42),
        ("-", 21, 7, 14),
        ("*", 7, 3, 21),
        ("/", 21, 3, 7),
        ("%", 21, 4, 1),
        ("&", 7, 3, 3),
        ("|", 4, 3, 7),
        ("^", 7, 3, 4),
        ("<<", 3, 2, 12),
        (">>", 12, 2, 3),
    ] {
        let path = temp_path("field-compound.rs");
        let source = format!(
            "struct Counter {{ value: i64 }} fn main() -> i64 {{ let mut counter = Counter {{ value: {initial} }}; counter.value {op}= {rhs}; counter.value }}"
        );
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_field_compounds_read_after_rhs_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("field-compound-effects.in");
    fs::write(&path, "struct Counter {\n Int value\n}\nvar counter: Counter = 0;\nfn value() -> Int { counter.value = 100; return 1; }\nfn main() -> Int { counter.value = 21; field-modify-value-first(counter, \"value\", value(), \"+\"); return counter.value; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(101)));
}

#[test]
fn owned_rust_float_field_compound_assignment() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("float-field-compound.rs");
    fs::write(&path, "struct Counter { value: f64 } fn main() -> f64 { let mut counter = Counter { value: 1.5 }; counter.value += 2.0; counter.value }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Float(3.5)));
}

#[test]
fn owned_field_compound_throw_preserves_original_field() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("field-compound-throw.in");
    fs::write(&path, "struct Counter {\n Int value\n}\nvar counter: Counter = 0;\nfn value() -> Int { throw 5; }\nfn main() -> Int { counter.value = 21; try { field-modify-value-first(counter, \"value\", value(), \"+\"); return 9; } catch e { return counter.value + 21; } }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_boolean_bitwise_operations_preserve_boolean_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        ("fn main() -> bool { true & false }", false),
        ("fn main() -> bool { true | false }", true),
        ("fn main() -> bool { true ^ true }", false),
        (
            "fn combine(a: bool, b: bool) -> bool { for i in 0..1 { if i == 0 { return a ^ b; } } return false; } fn main() -> bool { combine(true, false) }",
            true,
        ),
        (
            "fn combine(a: bool) -> bool { for i in 0..1 { if i == 0 { return a ^ a; } } return true; } fn main() -> bool { combine(true) }",
            false,
        ),
        (
            "fn main() -> bool { let mut values = [true]; values[0] &= false; values[0] }",
            false,
        ),
        (
            "struct Flag { value: bool } fn main() -> bool { let mut flag = Flag { value: false }; flag.value |= true; flag.value }",
            true,
        ),
    ] {
        let path = temp_path("bool-bitwise.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Bool(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_boolean_bitwise_operators_evaluate_both_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, left, right, negate) in [
        ("&", "false", "true", "!"),
        ("|", "true", "false", ""),
        ("^", "true", "false", ""),
    ] {
        let source = format!(
            "var calls: Int = 0;\nfn left() -> Bool {{ calls = calls * 10 + 1; return {left}; }}\nfn right() -> Bool {{ calls = calls * 10 + 2; return {right}; }}\nfn main() -> Int {{ let result: Bool = left() {op} right(); if {negate}result {{ return calls + 30; }} return 99; }}"
        );
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("bool-bitwise-effects.in");
            fs::write(&path, &source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{source}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{source}: {profile:?}"
            );
        }
    }
}

#[test]
fn owned_bitwise_operands_do_not_mix_boolean_float_and_integer_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> bool { true & 1 }",
        "fn main() -> i64 { 1 | false }",
        "fn main() -> i64 { 1.5 ^ 2.0 }",
        "fn main() -> i64 { true << false }",
    ] {
        let path = temp_path("invalid-bitwise.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
    for (source, expected) in [
        ("int main() { return true & false; }", 0),
        ("int main() { return true | 4; }", 5),
        ("int main() { return true << 2; }", 4),
        (
            "int combine(bool left, bool right) { return left | right; } int main() { return combine(true, false); }",
            1,
        ),
    ] {
        let path = temp_path("cpp-bool-bitwise.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_c_for_loops_preserve_updates_and_control_flow() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for (source, expected) in [
            (
                "int main() { for (int i = 0; i < 1; i++) { return 42; } return 0; }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 5; ++i) { if (i == 2) continue; sum = sum + i; } return sum; }",
                8,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 7; i += 2) { sum += i; } return sum; }",
                12,
            ),
            (
                "int main() { int i = 42; for (int i = 0; i < 3; i++) {} return i; }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 4; i > 0; i--) { if (i == 2) break; sum = sum + i; } return sum; }",
                7,
            ),
            (
                "int main() { int i = 0; for (;;) { i = i + 1; if (i == 42) return i; } }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 3; i = i + 1) { int j = 0; while (j < 2) { j = j + 1; continue; } sum = sum + i; } return sum; }",
                3,
            ),
            (
                "int main() { int i = 0; for (i = 2; i < 4; i++) i = i + 1; return i; }",
                4,
            ),
            (
                "int combine(bool left, bool right) { for (int i = 0; i < 1; i++) { return left | right; } return 0; } int main() { return combine(true, false); }",
                1,
            ),
        ] {
            let path = temp_path(&format!("c-for.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_unsupported_loop_parts_are_refused_instead_of_dropped() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { for (int i = 0; i < 1; i++) { switch (i) { case 0: return 42; } } return 0; }",
            "int main() { int values[1]; for (int i = 0; i < 1; values[i]++) { return 42; } return 0; }",
        ] {
            let path = temp_path(&format!("unsupported-c-for.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_c_global_storage_is_shared_and_zero_initialized() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int value = 20; int helper() { value = 40; return 2; } int main() { helper(); return value + 2; }",
            "extern int value = 42; int main() { return value; }",
            "int /* const */ value = 41; int main() { value++; return value; }",
            "int value; int main() { return value + 42; }",
            "int first = 20, second = 22; int main() { return first + second; }",
            "static int value = 21; int helper() { value = value * 2; return value; } int main() { helper(); return value; }",
            "const int value = 42; int main() { return value; }",
            "int value = 42; int main() { { int value = 0; value++; } return value; }",
        ] {
            let path = temp_path(&format!("c-global.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_global_definitions_preserve_refusal_boundaries() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "const int value = 42; int main() { value = 0; return value; }",
            "extern int value; int main() { return value; }",
            "int helper() { return 42; } int value = helper(); int main() { return value; }",
            "int values[2]; int main() { return 42; }",
        ] {
            let path = temp_path(&format!("unsupported-c-global.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_cpp_compound_assignment_reads_target_after_rhs_once() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, expected) in [
        ("+", 43),
        ("-", 37),
        ("*", 120),
        ("/", 13),
        ("%", 1),
        ("&", 0),
        ("|", 43),
        ("^", 43),
        ("<<", 320),
        (">>", 5),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = format!(
                "int value = 21; int calls = 0; int helper() {{ calls++; value = 40; return 3; }} int main() {{ value {op}= helper(); return value + calls * 1000; }}"
            );
            let path = temp_path("cpp-compound-order.cpp");
            fs::write(&path, &source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{source}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected + 1000)),
                "{source}: {profile:?}"
            );
        }
    }
    for (source, expected) in [
        (
            "int value = 20; int helper() { value = 40; return 2; } int main() { value += helper(); return value; }",
            42,
        ),
        (
            "int value = 40; int main() { int result = 0; { int value = 20; value += 2; result = value; } return result + value; }",
            62,
        ),
        (
            "int __array_repeat_0 = 40; int main() { int value = 20; value += 2; return value + __array_repeat_0; }",
            62,
        ),
        (
            "int step = 0; int calls = 0; int helper() { calls++; step = 2; return 2; } int main() { for (; step < 4; step += helper()) {} return step + calls * 1000; }",
            1004,
        ),
    ] {
        let path = temp_path("cpp-compound-place.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_c_integer_literal_radices_reach_native_unchanged() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for literal in [
            "0x2A",
            "0X2a",
            "052",
            "0b101010",
            "42ULL",
            "0x2Aul",
            "0xFF - 0xD5",
        ] {
            let path = temp_path(&format!("c-radix.{extension}"));
            fs::write(
                &path,
                format!("int value = {literal}; int main() {{ return value; }}"),
            )
            .unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {literal}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {literal}"
            );
        }
    }
    for literal in ["4'2", "0'52", "0x2'A", "0b10'1010"] {
        let path = temp_path("cpp-separated-int.cpp");
        fs::write(&path, format!("int main() {{ return {literal}; }}")).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{literal}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{literal}");
    }
}

#[test]
fn owned_c_local_declarators_preserve_order_dependencies_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int value = 0; int helper() { value = 42; return 1; } int main() { int first = 0, ignored = helper(); return value; }",
            "int calls = 0; int first() { calls = calls * 10 + 1; return 0; } int second() { calls = calls * 10 + 2; return 0; } int main() { int a = first(), b = second(); return calls + 30; }",
            "int main() { int first = 20, second = first + 2; return first + second; }",
            "int value = 99; int main() { int value = 20, second = value + 2; return value + second; }",
            "int main() { for (int i = 0, j = 40; i < 2; i++) { if (i == 1) return j + i + 1; } return 0; }",
            "int main() { int i = 42; for (int i = 0, j = 0; i < 1; i++) { j++; } return i; }",
            "int calls = 0; int first() { calls = calls * 10 + 1; return 0; } int second() { calls = calls * 10 + 2; return 0; } int main() { for (int a = first(), b = second(); a < 1; a++) { return calls + 30; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-local-declarators.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_unsupported_later_declarators_are_not_discarded() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int first = 42, *pointer = 0; return first; }",
            "int main() { int first = 42, second; return first; }",
            "int main() { int first = 42, values[2] = {1, 2}; return first; }",
        ] {
            let path = temp_path(&format!("unsupported-c-local-declarators.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_c_numeric_boolean_conversions_preserve_control_flow_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { if (42) return 42; return 0; }",
            "int main() { if (0) return 0; return 42; }",
            "int main() { int count = 2, sum = 0; while (count) { count--; sum += 21; } return sum; }",
            "int main() { int sum = 0; for (int i = 2; i; i--) { sum += 21; } return sum; }",
            "int main() { int result = !0; result = result && 7; return result * 42; }",
            "int main() { return (!0) * 42; }",
            "int main() { return (7 && 9) * 42; }",
            "bool *identity(bool *value) { return value; } int main() { if (identity(0)) return 0; return 42; }",
            "int main() { bool value = 2; return value * 42; }",
            "bool value = 2; int main() { value = 7; return value * 42; }",
            "int convert(bool value) { return value; } int main() { return convert(7) * 42; }",
            "bool convert(int value) { return value; } int main() { return convert(7) * 42; }",
            "double value; int main() { if (value) return 0; return 42; }",
            "double value; int main() { bool result = value; if (!result) return 42; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 0; } int right() { calls = calls * 10 + 2; return 7; } int main() { if (left() && right()) return 0; return calls + 41; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 7; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() || right()) return calls + 41; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 7; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() && right()) return calls + 30; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 0; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() || right()) return calls + 30; return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-bool.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_non_c_frontends_keep_strict_boolean_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (extension, source) in [
        ("rs", "fn main() -> i64 { if 42 { return 42; } return 0; }"),
        (
            "rs",
            "fn main() -> i64 { if 1 && 2 { return 42; } return 0; }",
        ),
        ("in", "fn main() -> Int { if 42 { return 42; } return 0; }"),
        (
            "in",
            "fn main() -> Int { let value: Bool = 2; if value { return 42; } return 0; }",
        ),
    ] {
        let path = temp_path(&format!("strict-bool.{extension}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{extension}: {source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn raw_c_floating_conditions_preserve_zero_and_nan_truth_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (value, expected) in [
        (0.0, 0),
        (-0.0, 0),
        (1.0, 42),
        (-1.0, 42),
        (f64::INFINITY, 42),
        (f64::NEG_INFINITY, 42),
        (f64::NAN, 42),
    ] {
        let mut module = crate::in_lang_parse::parse_in_source(
            "var value: Float = 0.0;\nfn main() -> Int { if value { return 42; } return 0; }",
        )
        .unwrap();
        let crate::core_ir::Decl::Global { init, .. } = &mut module.decls[0] else {
            panic!("missing global");
        };
        *init = Some(Box::new(crate::core_ir::Expr::FloatLit(
            crate::core_ir::FloatVal(value),
        )));
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &module);
        let request = default_request(
            PathBuf::from("raw-c-float-condition.icore"),
            CompileTarget::Jit,
            Some("main"),
            None,
        );
        let result = super::jit::compile_jit(&module, "FloatingConditions", &request).unwrap();
        assert_eq!(
            result.eval_result,
            Some(EvalValue::Int(expected)),
            "{value:?}"
        );
    }
}

#[test]
fn raw_c_boolean_stores_use_field_and_element_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Flag { Bool value }\nfn main() -> Int { let flag: Flag = Flag { value: false }; flag.value = 2; if flag.value { return 42; } return 0; }",
        "fn main() -> Int { let flags = [false]; flags[0] = 2; if flags[0] { return 42; } return 0; }",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &module);
        let request = default_request(
            PathBuf::from("raw-c-bool-store.icore"),
            CompileTarget::Jit,
            Some("main"),
            None,
        );
        let result = super::jit::compile_jit(&module, "BooleanStores", &request).unwrap();
        assert_eq!(result.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_cpp_condition_initializers_keep_effects_bindings_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (int ignored = helper(); true) return value; return 0; }",
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (helper(); 7) return value; return 0; }",
        "int value = 0; int helper() { value = 42; return 0; } int main() { if (int result = helper(); result) return 0; else return value; }",
        "int result = 42; int main() { if (int result = 0; result) return 0; return result; }",
        "int main() { if (int result = 42) return result; return 0; }",
        "int main() { if (bool result = 7) return result * 42; return 0; }",
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (/* comment */ int ignored = helper(); true) return value; return 0; }",
    ] {
        let path = temp_path("cpp-condition-init.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_cpp_scalar_conversion_does_not_guess_objects_or_references() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Painting { operator bool() { return true; } }; Painting value; int main() { if (value) return 42; return 0; }",
        "struct Double { operator bool() { return true; } }; Double value; int main() { if (value) return 42; return 0; }",
        "struct Bool { operator bool() { return true; } }; Bool value; int main() { if (value) return 42; return 0; }",
        "struct u8 { operator bool() { return true; } }; u8 value; int main() { if (value) return 42; return 0; }",
        "void clear(bool &value) { value = false; } int main() { bool value = true; clear(value); return value; }",
        "long double value = 1; int main() { if (value / 2) return 42; return 0; }",
    ] {
        let path = temp_path("cpp-non-scalar-conversion.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_c_parameter_assignment_reuses_the_parameter_frame_slot() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "void reset(int value) { value = 0; } int main() { reset(1); return 42; }",
            "int reset(int value) { value = 42; return value; } int main() { return reset(1); }",
            "bool reset(bool value) { value = false; return value; } int main() { if (reset(true)) return 0; return 42; }",
            "int last(int a, int b, int c, int d, int e, int f, int g, int h, int value) { value = 42; return value; } int main() { return last(0,0,0,0,0,0,0,0,1); }",
        ] {
            let path = temp_path(&format!("c-param-store.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_stack_arguments_survive_later_argument_calls() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = r#"int helper(int a, int b, int c, int d, int e, int f, int g, int h, int i) {
    for (int n = 0; n < 1; n++) { return 2; }
    return 0;
}
int combine(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j) {
    return i * 10 + j;
}
int main() { return combine(0,0,0,0,0,0,0,0,4,helper(0,0,0,0,0,0,0,0,99)); }
"#;
    for extension in ["c", "cpp"] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path(&format!("stack-argument-effects.{extension}"));
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {profile:?}"
            );
        }
    }
}

#[test]
fn owned_native_calls_stage_many_scalar_and_aggregate_words() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let params = (0..32)
        .map(|index| format!("int p{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = (0..32)
        .map(|index| if index == 0 || index == 31 { "21" } else { "0" })
        .collect::<Vec<_>>()
        .join(",");
    let scalar = format!(
        "int combine({params}) {{ for (int i = 0; i < 1; i++) {{ return p0 + p31; }} return 0; }} int main() {{ return combine({args}); }}"
    );
    let fields = (0..10)
        .map(|index| format!("Int f{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let values = (0..10)
        .map(|index| {
            format!(
                "f{index}: {}",
                if index == 0 || index == 9 { 21 } else { 0 }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let aggregate = format!(
        "struct Wide {{\n{fields}\n}}\nfn combine(value: Wide) -> Int {{ for i in 0..1 {{ return value.f0 + value.f9; }} return 0; }}\nfn main() -> Int {{ let value: Wide = Wide {{ {values} }}; return combine(value); }}"
    );
    for (extension, source) in [("c", scalar), ("in", aggregate)] {
        let path = temp_path(&format!("many-argument-words.{extension}"));
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_c_decimal_floating_literals_reach_native_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "double value = 1.5; int main() { if (value > 1.0) return 42; return 0; }",
            "double value = 1.25e1; int main() { if (value == 12.5) return 42; return 0; }",
            "double value = .5; int main() { if (value + .5 == 1.) return 42; return 0; }",
            "double value = 1e-3; int main() { if (value == 0.001) return 42; return 0; }",
            "float value = 0.1f; int main() { if (value > 0.1) return 42; return 0; }",
            "double twice(double value) { for (int i = 0; i < 1; i++) return value * 2.0; return 0.0; } int main() { if (twice(1.5) == 3.0) return 42; return 0; }",
            "bool value = .5; int main() { if (value) return 42; return 0; }",
        ] {
            let path = temp_path(&format!("c-float-literal.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
    let path = temp_path("cpp-float-separator.cpp");
    fs::write(
        &path,
        "double value = 1'2.5; int main() { if (value == 12.5) return 42; return 0; }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_cpp_while_declarations_reevaluate_and_keep_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "int calls = 0; int next() { calls++; if (calls < 3) return 1; return 0; } int main() { int sum = 0; while (int value = next()) { sum += 21; if (sum > 42) return 99; } return sum; }",
        "int calls = 0; int next() { calls++; if (calls < 3) return 1; return 0; } int main() { int iterations = 0; while (int value = next()) { iterations++; if (iterations > 2) return 99; continue; } return calls + 39; }",
        "int value = 42; int main() { while (int value = 0) { return 0; } return value; }",
        "int main() { while (bool value = 2) { return value * 42; } return 0; }",
        "int calls = 0; int next() { calls++; return 1; } int main() { while (int value = next()) { break; } return calls + 41; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("cpp-while-decl.cpp");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_do_while_keeps_posttest_effects_continue_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int value = 0; do { value++; } while (0); return value + 41; }",
            "int calls = 0; int condition() { calls++; return 0; } int main() { int value = 0; do { value = 1; continue; } while (condition()); return value + calls + 40; }",
            "int calls = 0; int condition() { calls++; return 1; } int main() { do { break; } while (condition()); return calls + 42; }",
            "int value = 0; int main() { int iterations = 0; do { int value = 7; iterations++; if (iterations > 1) return 99; continue; } while (value); return iterations + 41; }",
            "int main() { int value = 0; do { value++; int inner = 0; while (inner < 2) { inner++; continue; } } while (value < 2); return value + 40; }",
            "int main() { int value = 0; do value++; while (value < 2); return value + 40; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-do-while.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_conversions_preserve_effects_and_truncate() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "double widen(int value) { return value; } int narrow(double value) { return value; } int main() { return narrow(widen(40) + 2.75); }",
            "int main() { double value = 41; value = value + 1.9; int result = value; return result; }",
            "int main() { double value = -42.9; return -value; }",
            "int main() { double value = true; return value + 41; }",
            "int calls = 0; double next() { calls++; return 41.9; } int take(int value) { return value; } int main() { return take(next()) + calls; }",
            "int main() { double value = 40.75; return value + 1.5; }",
            "double value = 42; int main() { return value; }",
            "int main() { double value = -0.5; return -value * 84; }",
            "int narrow(double value) { return value; } int main() { return narrow(-42.9) + 84; }",
            "int main() { double value = 42.75; return (value > 42) + 41; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-conversion.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_casts_preserve_value_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "unsigned char value = 298; int main() { return value; }",
            "unsigned char callback(int value) { return 42; } int check(unsigned char (*cb)(int)) { if (cb == callback) return 42; return 7; } int main() { return check(callback); }",
            "unsigned char callback(int value) { return 42; } int check(unsigned char cb(int)) { if (cb == callback) return 42; return 7; } int main() { return check(callback); }",
            "unsigned char callback(int value) { return 42; } unsigned char (*factory(void))(int) { return callback; } int main() { if (factory() == callback) return 42; return 7; }",
            "unsigned char (answer)(int *pointer) { return 298; } int main() { return answer(0); }",
            "int main() { unsigned char value = 298; return value; }",
            "int main() { unsigned short value = 65578; return value; }",
            "int main() { unsigned char value = 0; value = 298; return value; }",
            "int main() { unsigned char value = 255; value += 43; return value; }",
            "int main() { unsigned char value = 255; value++; return value + 42; }",
            "int answer(unsigned char value) { return value; } int main() { return answer(298); }",
            "unsigned char answer() { return 298; } int main() { return answer(); }",
            "int answer(unsigned char); int main() { return answer(298); } int answer(unsigned char value) { return value; }",
            "int calls = 0; int next() { calls++; return 298; } int main() { unsigned char value = next(); if (calls != 1) return 0; return value; }",
            "int calls = 0; int next() { calls++; return 298; } int main() { unsigned char value = 0; value = next(); if (calls != 1) return 0; return value; }",
            "int calls = 0; int next() { calls++; return 298; } int answer(unsigned char value) { return value; } int main() { int value = answer(next()); if (calls != 1) return 0; return value; }",
            "int calls = 0; int next() { calls++; return 298; } unsigned char answer() { return next(); } int main() { int value = answer(); if (calls != 1) return 0; return value; }",
            "int main() { int value = 298; { unsigned char value = 298; if (value != 42) return 0; } if (value != 298) return 0; return 42; }",
            "int main() { unsigned char value = 298; { int value = 298; if (value != 298) return 0; } return value; }",
            "int main() { int value = 298; return (unsigned char)value; }",
            "int main() { int value = 298; return (unsigned /* width */ char)value; }",
            "int main() { int value = -214; return (unsigned char)value; }",
            "int main() { int value = 65578; return (unsigned short)value; }",
            "int main() { int value = -65494; return (short unsigned int)value; }",
            "int main() { int value = -42; return (signed char)value + 84; }",
            "int main() { int value = -32768; return (signed short)value + 32810; }",
            "int main() { double value = -42.9; return (signed char)value + 84; }",
            "int main() { double value = 42.9; return (unsigned char)value; }",
            "int main() { _Bool value = 1; return (unsigned short)value + 41; }",
            "int main() { int value = 65834; return (unsigned char)(unsigned short)value; }",
            "int main() { long long value = 4294967338; return (unsigned int)value; }",
            "int main() { long long value = 4294967295; return (signed int)value + 43; }",
            "int calls = 0; int next() { calls++; return 298; } int main() { int value = (const unsigned char)next(); return value + calls - 1; }",
            "int main() { double value = 42.9; return (int)value; }",
            "int main() { int value = 21; return (int)((double)value * 2.0); }",
            "int main() { double value = -0.5; return (int)((double)(int)value + 42.0); }",
            "int main() { double value = -42.9; return (int)value + 84; }",
            "int calls = 0; double next() { calls++; return 41.9; } int main() { return (int)next() + calls; }",
            "int main() { double value = 2.5; return (int)((_Bool)value) + 41; }",
            "int main() { int value = 42; return (const int)value; }",
            "int calls = 0; double next() { calls++; return 2.5; } int main() { return (int)(_Bool)next() + calls + 40; }",
            "int main() { double value = -0.0; return (int)(_Bool)value + 42; }",
        ] {
            let source = if extension == "cpp" {
                format!("struct u8 {{}}; {source}")
            } else {
                source.to_owned()
            };
            let source = source.replace("_Bool", if extension == "cpp" { "bool" } else { "_Bool" });
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-cast.{extension}"));
                fs::write(&path, &source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn raw_unsigned_word_operations_preserve_values_and_checked_failure_effects() {
    use crate::core_ir::Expr;
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, expected) in [("udiv", i64::MAX), ("urem", 1), ("lshr", i64::MAX)] {
        for zero in [false, true] {
            if zero && op == "lshr" {
                continue;
            }
            let right = if zero { 0 } else { 2 };
            let expected = if op == "lshr" {
                4611686018427387903
            } else {
                expected
            };
            let tail = if zero {
                "let result=helper(); if trace==12 && result==1 {return 42;} return 7;".to_owned()
            } else {
                format!(
                    "let result=helper(); if trace==123 && result=={expected} {{return 42;}} return 7;"
                )
            };
            let source = format!(
                "var trace:Int=0;\nfn left()->Int{{trace=trace*10+1;return -1;}}\nfn right()->Int{{trace=trace*10+2;return {right};}}\nfn helper()->Int{{let answer=left()/right();trace=trace*10+3;return answer;}}\nfn main()->Int{{{tail}}}"
            );
            let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let Expr::Binary { op: operator, .. } = expr
                    && operator == "/"
                {
                    *operator = op.into();
                }
            });
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request = default_request(
                    PathBuf::from("raw-unsigned.in"),
                    CompileTarget::Jit,
                    Some("main"),
                    None,
                );
                request.profile = profile;
                let result = super::jit::compile_jit(&module, "UnsignedEffects", &request).unwrap();
                assert_eq!(
                    result.eval_result,
                    Some(EvalValue::Int(42)),
                    "{op}: zero={zero}: {profile:?}"
                );
            }
        }
    }
}

#[test]
fn owned_c_unsigned_expressions_refuse_unrepresented_mixed_integer_ranks() {
    for extension in ["c", "cpp"] {
        for source in [
            "int main(){unsigned int value=1; long long other=-1; if(value>other)return 42;return 7;}",
            "int main(){unsigned int value=1; if(value<4294967295)return 42;return 7;}",
            "int main(){unsigned int value=1; return value+4294967296U;}",
        ] {
            let path = temp_path(&format!("c-unsigned-unknown-rank.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{source}: {report:?}");
            assert!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unsupported-c-integer-rank")),
                "{source}: {report:?}"
            );
        }
    }
}

#[test]
fn owned_c_unsigned_integer_expressions_preserve_width_order_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main(){unsigned int value=4294967295; if(value>0)return 42;return 7;}",
            "int main(){unsigned int value=1; if(value>-1)return 7;return 42;}",
            "int main(){unsigned int value=4294967295; if(value+1==0)return 42;return 7;}",
            "int main(){if(0xffffffff>0 && 1U < -1 && 4294967295u > 0)return 42;return 7;}",
            "int main(){unsigned int value=4294967295; if((value+2)*2-4==4294967294U)return 42;return 7;}",
            "int main(){unsigned int value=4294967295; if(value/2==2147483647 && value%2==1)return 42;return 7;}",
            "int main(){unsigned int value=2147483648U; if((value>>31)==1 && (value<<1)==0)return 42;return 7;}",
            "int main(){unsigned int value=1; if(-value==4294967295U && ~value==4294967294U)return 42;return 7;}",
            "int main(){unsigned int value=4294967295; int negative=-1; if(value==negative && !(value!=negative))return 42;return 7;}",
            "int main(){unsigned char value=255; if(value+1==256 && -value==-255)return 42;return 7;}",
            "int main(){unsigned int value=4294967295; double other=4294967295.0; if(value==other)return 42;return 7;}",
            "unsigned int source(){return 4294967295U;} int main(){if(source()>0 && source()/2==2147483647)return 42;return 7;}",
            "int calls=0; unsigned int source(){calls++;return 4294967295U;} int main(){if(source()/2==2147483647 && calls==1)return 42;return 7;}",
            "int calls=0; unsigned int source(){calls++;return 4294967295U;} int main(){if(1 || source()>0){if(calls==0)return 42;}return 7;}",
            "int main(){unsigned int value=1; if(value+!0==2)return 42;return 7;}",
            "int main(){unsigned int value=4294967295U; value++; if(value!=0)return 7; value--; if(value!=4294967295U)return 7; value+=2; value*=2; if(value==2)return 42;return 7;}",
            "int main(){if(-1U==4294967295U && 1U < -1U)return 42;return 7;}",
            "int main(){unsigned char small=0; unsigned int value=1; value+=~small; if(value==0)return 42;return 7;}",
            "int main(){unsigned char small=1; unsigned int value=1; value+=-small; if(value==0)return 42;return 7;}",
            "int main(){unsigned char small=255; if(+small+1==256 && +1U < -1)return 42;return 7;}",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-unsigned-expressions.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_casts_refuse_pointer_object_and_extended_types() {
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int value = 42; return (int*)value; }",
            "int main() { int value = 42; return (long double)value; }",
            "int main() { int value = 42; return (Unknown)value; }",
        ] {
            let path = temp_path(&format!("c-unsupported-cast.{extension}"));
            fs::write(&path, source).unwrap();
            let request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_rust_integer_casts_truncate_and_extend() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let value: i64 = 298; return (value as u8) as i64; }",
        "fn main() -> i64 { let value: i64 = 170; return (value as i8) as i64 + 128; }",
        "fn main() -> i64 { let value: i64 = 65578; return (value as u16) as i64; }",
        "fn main() -> i64 { let value: i64 = 32768; return (value as i16) as i64 + 32810; }",
        "fn main() -> i64 { let value: i64 = 4294967338; return (value as u32) as i64; }",
        "fn main() -> i64 { let value: i64 = 2147483648; return (value as i32) as i64 + 2147483690; }",
        "fn main() -> i64 { let value: i64 = -214; return (value as u8) as i64; }",
        "fn main() -> i64 { let value: bool = true; return (value as u8) as i64 + 41; }",
        "fn main() -> i64 { let value: bool = false; return (value as i64) + 42; }",
        "fn main() -> i64 { let value: i64 = 42; return (value as usize) as isize as i64; }",
        "fn main() -> i64 { let value: i64 = 298; return (value as core::primitive::u8) as i64; }",
        "fn main() -> i64 { let value: i64 = 298; return (value as ::std::primitive::u8) as i64; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("rust-integer-cast.rs");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_rust_floating_width_casts_round_and_keep_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let value = 16777217.0; if (value as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn main() -> i64 { let value: f64 = 16777217.0; if (value as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn value() -> f64 { return 16777217.0; } fn main() -> i64 { if (value() as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn main() -> i64 { let value = 0.1f32; if (value as f64) > 0.1f64 { return 42; } return 0; }",
        "fn main() -> i64 { let value = 42f32; if value as f64 == 42f64 { return 42; } return 0; }",
        "fn main() -> i64 { let value: f64 = 42.5; if value as ::core::primitive::f64 == 42.5 { return 42; } return 0; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("rust-float-width.rs");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_signed_shift_sources_preserve_sign_and_compound_updates() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (extension, source) in [
        (
            "in",
            include_str!("../../../conformance/types/signed-shift-effects.in"),
        ),
        (
            "rs",
            "fn half(value: i64, count: i64) -> i64 { return value >> count; } fn main() -> i64 { let mut value: i64 = -168; value >>= 1; return -half(value, 1); }",
        ),
        (
            "c",
            "long long value = -84; long long ones = -1; long long minimum = -9223372036854775807LL - 1; int main() { int count = 1; if ((ones >> count) != -1) return 99; count = 63; if ((minimum >> count) != -1) return 99; count = 1; value >>= count; return -value; }",
        ),
        (
            "cpp",
            "long long value = -84; long long ones = -1; long long minimum = -9223372036854775807LL - 1; int main() { int count = 1; if ((ones >> count) != -1) return 99; count = 63; if ((minimum >> count) != -1) return 99; count = 1; value >>= count; return -value; }",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path(&format!("signed-shift.{extension}"));
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {profile:?}"
            );
        }
    }
}

#[test]
fn owned_in_index_compound_updates_evaluate_place_once_before_rhs() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        include_str!("../../../conformance/types/array-return-writable.in"),
        include_str!("../../../conformance/control-flow/in-index-compound-effects.in"),
        "var trace: Int = 0;\nfn base() -> [Int] { trace = trace * 10 + 1; return [39]; }\nfn index() -> Int { trace = trace * 10 + 2; return 0; }\nfn rhs() -> Int { trace = trace * 10 + 3; return 2; }\nfn main() -> Int { base()[index()] += rhs(); return trace - 81; }",
        "fn mutate(values: [Float]) -> Float { values[0] = 40.0; return 2.0; }\nfn main() -> Int { var values: [Float] = [20.0]; values[0] += mutate(values); if values[0] == 22.0 { return 42; } return 0; }",
        "var calls: Int = 0;\nfn index() -> Int { calls += 1; return calls - 1; }\nfn main() -> Int { var values = [39, 0]; values[index()] += 2; return values[0] + calls; }",
        "var trace: Int = 0;\nfn index() -> Int { trace = trace * 10 + 1; return 0; }\nfn rhs() -> Int { trace = trace * 10 + 2; return 2; }\nfn main() -> Int { var values = [28]; values[index()] += rhs(); return values[0] + trace; }",
        "fn mutate(values: [Int]) -> Int { values[0] = 40; return 2; }\nfn main() -> Int { var values = [20]; values[0] += mutate(values); return values[0] + 20; }",
        "fn main() -> Int { var values = [-84]; values[0] >>= 1; return -values[0]; }",
        "fn main() -> Int { var values = [21]; values[0] <<= 1; return values[0]; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("in-index-compound.in");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_global_field_projection_does_not_discard_constructor_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (source, expected) in [
            (
                "struct Point { Int x; Int y }\nvar calls: Int = 0;\nfn effect() -> Int { calls += 1; return 0; }\nvar seed: Int = Point { x: 42, y: effect() }.x;\nfn main() -> Int { return seed + calls; }",
                None,
            ),
            (
                "struct Point { Int x; Int y }\nvar seed: Int = Point { x: 42, y: 0 }.x;\nfn main() -> Int { return seed; }",
                Some(EvalValue::Int(42)),
            ),
        ] {
            let path = temp_path("global-field-projection.in");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert_eq!(
                report.success,
                expected.is_some(),
                "{profile:?}: {report:?}"
            );
            assert_eq!(report.eval_result, expected, "{profile:?}: {report:?}");
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn malformed_string_literals_and_patterns_refuse_execution() {
    let mut sources = vec![("in", r#"fn main()->String{return "a"b"c";}"#.to_string())];
    for pattern in [r#""a"b""#, r#""a\""#] {
        sources.push((
            "icore",
            serde_json::json!({
                "icoreVersion":2,
                "decls":[{"kind":"function","name":"main","params":[],"return":"Int","body":[{
                    "kind":"match","scrutinee":{"kind":"string","value":"a"},
                    "arms":[{"pattern":pattern,"body":[{"kind":"return","value":42}]},
                            {"pattern":"_","body":[{"kind":"return","value":0}]}]
                }]}]
            })
            .to_string(),
        ));
    }
    for (extension, source) in sources {
        let path = temp_path(&format!("malformed-string.{extension}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        assert!(!report.success, "{report:?}");
        assert!(report.eval_result.is_none(), "{report:?}");
        let reason = report.reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("pattern") || reason.contains("unresolved identifier"),
            "{report:?}"
        );
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn in_struct_patterns_execute_statement_value_and_guarded_arms() {
    let schema = "struct Entry {\n Int value\n String text\n}\n";
    for body in [
        r#"match entry { Entry {value:x,text:"a,b{c}"} {return x+22;} _ {return 0;} }"#,
        r#"match entry { Entry {value:x,text:"a,b{c}"} if false -> {return 0;} Entry {value:x,text:"a,b{c}"} if x==20 -> {return x+22;} _ {return 0;} }"#,
        r#"return match entry { Entry {value:x,text:"a,b{c}"} -> {x+22} _ {0} };"#,
    ] {
        let path = temp_path("struct-match.in");
        fs::write(&path, format!("{schema}fn main()->Int{{let entry:Entry=Entry{{value:20,text:\"a,b{{c}}\"}};{body}}}")).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{body}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{body}: {profile:?}: {report:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("struct-match-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{body}: {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
        }
        let canonical =
            crate::in_canonical::canonicalize_in_source(&fs::read_to_string(&path).unwrap())
                .unwrap();
        fs::write(&path, canonical).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        assert!(report.success, "{body}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(42)),
            "{body}: {report:?}"
        );
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn in_multiline_strings_preserve_exact_runtime_text_and_canonical_output() {
    for expected in [
        "{",
        "}",
        "before\n//inside\nend",
        "before  \n  after\n\nend",
        "first\nprotocol:\n//literal\nlast",
        "first\\\n",
        "π:\n  // λ\n🦀}",
    ] {
        let literal = expected.replace('"', "\\\"");
        for source in [
            format!("fn main()->String{{return \"{literal}\";}}\n"),
            format!("fn main()->String{{\n let text:String=\"{literal}\";\n return text;\n}}\n"),
            format!("fn main()->String:\n  let text:String=\"{literal}\";\n  return text;\n"),
        ] {
            let path = temp_path("multiline-string.in");
            for text in [
                &source,
                &crate::in_canonical::canonicalize_in_source(&source).unwrap(),
            ] {
                fs::write(&path, text).unwrap();
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    let mut request =
                        default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(report.success, "{source}: {profile:?}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Str(expected.into())),
                        "{source}: {profile:?}: {report:?}"
                    );
                }
            }
            fs::remove_file(path).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn match_scrutinee_boundaries_execute_without_consuming_following_blocks() {
    for source in [
        r#"struct Entry { Int value }
fn main()->Int {match (Entry{value:42}) {Entry{value:answer}{return answer;}}}"#,
        r#"fn main()->Int {match ({42}) {42{return 42;} _{return 0;}}}"#,
        r#"struct Entry { Int value }
fn main()->Int {match [42] {[answer] {return answer;} _ {return 0;}}}"#,
        r#"fn main()->Int {match "{" {"{"{return 42;} _{return 0;}}}"#,
        r#"fn main()->Int {return match "{" {"{"{42} _{0}};}"#,
        r#"fn main()->Int {let value=42;match value {answer: {return answer;}} {0;}}"#,
    ] {
        let path = temp_path("match-header.in");
        for text in [
            source.to_string(),
            crate::in_canonical::canonicalize_in_source(source).unwrap(),
        ] {
            fs::write(&path, &text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?}: {text}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}: {report:?}"
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn rust_static_string_globals_use_native_data_storage() {
    let source = temp_path("rust-static-string.rs");
    fs::write(&source, "static TEXT:&str=\"λ\"; static VALUE:i64=42; fn main()->i64{if TEXT==\"λ\" {return VALUE;}0}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{profile:?}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(42)),
            "{profile:?}: {report:?}"
        );
    }
    fs::remove_file(source).unwrap();
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_explicit_global_receiver_methods_respect_parameter_and_local_shadows() {
    for (original, expected) in [
        (
            "var number:Int=42;\nfn main()->String{return number.toStr();}",
            "42",
        ),
        (
            "const number:Int=42;\nstruct Item{Int x}\nfn Item-toStr(value:Item)->String{return \"item\";}\nfn parameter(number:Item)->String{return number.toStr();}\nfn local()->String{let number:Item=Item{x:1};return number.toStr();}\nfn main()->String{if parameter(Item{x:2})!=\"item\" || local()!=\"item\"{return \"wrong shadow\";}return number.toStr();}",
            "42",
        ),
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let source = temp_path("explicit-global-receiver.in");
                fs::write(&source, text).unwrap();
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?} {text}: {report:?}");
                assert_eq!(report.eval_result, Some(EvalValue::Str(expected.into())));
                fs::remove_file(source).unwrap();
            }
        }
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_integer_strings_preserve_extremes_lifetimes_and_print_digits() {
    let cases = [
        ("(-9223372036854775807 - 1)", "-9223372036854775808"),
        ("9223372036854775807", "9223372036854775807"),
        ("0", "0"),
        ("-1", "-1"),
        ("1", "1"),
        ("9", "9"),
        ("10", "10"),
        ("-10", "-10"),
        ("1000000000000", "1000000000000"),
    ];
    let mut body = String::new();
    for (index, (value, _)) in cases.iter().enumerate() {
        body.push_str(&format!("let text{index}=convert({value});"));
    }
    for (index, (_, expected)) in cases.iter().enumerate() {
        body.push_str(&format!(
            "if text{index}!=\"{expected}\"{{return \"wrong digits\";}}"
        ));
    }
    body.push_str("return text0+\"|\"+text1+\"|\"+text2;");
    let expected = "-9223372036854775808|9223372036854775807|0";
    let printed = "-9223372036854775808|9223372036854775807";
    let original = format!(
        "var calls:Int=0;\nfn convert(value:Int)->String{{calls+=1;return value.toStr();}}\nfn stringentry()->String{{{body}}}\nfn main()->Int{{let result=stringentry();if result!=\"{expected}\" || calls!={}{{return 0;}}__inrt_print_int((-9223372036854775807 - 1));__inrt_print(\"|\");__inrt_print_int(9223372036854775807);return 42;}}",
        cases.len()
    );
    let canonical = crate::in_canonical::canonicalize_in_source(&original).unwrap();
    for text in [original.as_str(), canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("native-integer-string.in");
            let binary = temp_path("native-integer-string");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("stringentry"),
                None,
            );
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Str(expected.into())));
            request.target = CompileTarget::Native;
            request.entry = Some("main".into());
            request.out = Some(binary.clone());
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{profile:?} {report:?}"
            );
            let output = std::process::Command::new(&binary).output().unwrap();
            assert_eq!(output.status.code(), Some(42), "{profile:?}: {output:?}");
            assert_eq!(output.stdout, printed.as_bytes(), "{profile:?}");
            fs::remove_file(binary).unwrap();
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_inferred_globals_preserve_values_and_constant_chains() {
    let original = "const selected=alias;\nconst alias=prefix;\nconst prefix=\"λ\";\nvar word=\"雪\";\nvar enabled=true;\nvar ratio=1.5;\nvar values=[20,22];\nfn stringentry()->String{return selected+\"|\"+word;}\nfn floatentry()->Float{return ratio;}\nfn main()->Int{if enabled && ratio==1.5 && values[0]+values[1]==42 && stringentry()==\"λ|雪\"{return 42;}return 0;}";
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("inferred-global-values.in");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("stringentry"),
                None,
            );
            request.profile = profile;
            for (entry, expected) in [
                ("stringentry", EvalValue::Str("λ|雪".into())),
                ("floatentry", EvalValue::Float(1.5)),
                ("main", EvalValue::Int(42)),
            ] {
                request.entry = Some(entry.into());
                let report = compile_owned(&request);
                assert!(report.success, "{entry} {profile:?} {text}: {report:?}");
                assert_eq!(report.eval_result, Some(expected), "{entry} {profile:?}");
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("inferred-global-values");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?} {text}: {report:?}"
                );
                let output = std::process::Command::new(&binary).output().unwrap();
                assert_eq!(output.status.code(), Some(42), "{profile:?}: {output:?}");
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn owned_macos_x86_jit_decodes_results_and_recreates_runtime_state() {
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (extension, text, expected) in [
            (
                "in",
                "var state:Int=41;\nfn main()->Int{state=state+1;return state;}",
                Some(EvalValue::Int(42)),
            ),
            ("in", "fn main()->Int{return 43;}", Some(EvalValue::Int(43))),
            (
                "in",
                "var state:Int=41;\nfn main()->Int{state=state+1;return state;}",
                Some(EvalValue::Int(42)),
            ),
            (
                "in",
                "fn main()->Bool{return true;}",
                Some(EvalValue::Bool(true)),
            ),
            (
                "in",
                "fn main()->Float{return 1.25+2.5;}",
                Some(EvalValue::Float(3.75)),
            ),
            ("in", "fn main()->void{return;}", None),
            (
                "rs",
                "struct Point{x:i64} fn make()->Point{Point{x:42}} fn main()->i64{make().x}",
                Some(EvalValue::Int(42)),
            ),
        ] {
            let path = temp_path(&format!("macos-x86-public-jit.{extension}"));
            fs::write(&path, text).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}/{text}: {report:?}");
            assert_eq!(
                report.reason_code.as_deref(),
                Some("jit-executed"),
                "{report:?}"
            );
            assert_eq!(
                report.eval_result, expected,
                "{profile:?}/{text}: {report:?}"
            );
        }
    }
    // Enabling in-memory execution must not advertise native artifact support.
    assert!(!crate::native_emit::lower::host_supports_native_subset());
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_global_initializers_snapshot_earlier_scalar_and_string_values() {
    let original = r#"var word:String="λ雪";
var sentinel:Int=42;
var selected:String=word;
var original:Int=20;
var copied:Int=original;
var flag:Bool=true;
var copiedflag:Bool=flag;
var number:Float=1.5;
var copiednumber:Float=number;
fn stringentry()->String{word="changed";return selected;}
fn main()->Int{word="changed";original=1;flag=false;number=2.5;if selected!="λ雪" || word!="changed" || copied!=20 || !copiedflag || copiednumber!=1.5 || sentinel!=42{return 0;}return 42;}
"#;
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("initial-global-snapshots.in");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("stringentry"),
                None,
            );
            request.profile = profile;
            for (entry, expected) in [
                ("stringentry", EvalValue::Str("λ雪".into())),
                ("main", EvalValue::Int(42)),
            ] {
                request.entry = Some(entry.into());
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?}/{entry}: {report:?}");
                assert_eq!(report.eval_result, Some(expected), "{profile:?}/{entry}");
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("initial-global-snapshots");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_array_initializers_snapshot_earlier_scalar_and_string_values() {
    let original = r#"var word:String="λ雪";
var seed:Int=20;
var flag:Bool=true;
var number:Float=1.5;
var words:[[String]]=[[word,""]];
var values:[Int]=[seed,22];
var flags:[[Bool]]=[[flag]];
var numbers:[[Float]]=[[number]];
var sentinel:Int=42;
fn stringentry()->String{word="changed";return words[0][0];}
fn main()->Int{word="changed";seed=1;flag=false;number=2.5;if words[0][0]!="λ雪" || words[0][1]!="" || values[0]+values[1]!=42 || !flags[0][0] || numbers[0][0]!=1.5 || sentinel!=42{return 0;}values[0]=21;if seed!=1 || values[0]!=21{return 0;}return 42;}
"#;
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("array-initial-snapshots.in");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("stringentry"),
                None,
            );
            request.profile = profile;
            for (entry, expected) in [
                ("stringentry", EvalValue::Str("λ雪".into())),
                ("main", EvalValue::Int(42)),
            ] {
                request.entry = Some(entry.into());
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?}/{entry}: {report:?}");
                assert_eq!(report.eval_result, Some(expected), "{profile:?}/{entry}");
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("array-initial-snapshots");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_declared_function_aliases_preserve_overrides_and_exact_collisions() {
    for original in [
        "var calls:Int=0;\nfn env-temp-dir()->Int{calls+=1;return 42;}\nfn main()->Int{let result=env_temp_dir();if calls!=1{return 0;}return result;}",
        "fn answer-name-more()->Int{return 42;}\nfn main()->Int{return answer_name-more();}",
        "fn answer-name()->Int{return 0;}\nfn answer_name()->Int{return 42;}\nfn main()->Int{return answer_name();}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let source = temp_path("function-alias.in");
                fs::write(&source, text).unwrap();
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?} {text}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?} {text}"
                );
                fs::remove_file(source).unwrap();
            }
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_aliased_incompatible_wrapper_declarations_remain_refused() {
    let source = temp_path("aliased-wrapper-contract.in");
    fs::write(
        &source,
        "extern rust fn env-temp-dir()->Float;\nfn main()->Float{return env_temp_dir();}\n",
    )
    .unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(!report.success, "{profile:?}: {report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("incompatible"),
            "{profile:?}: {report:?}"
        );
    }
    fs::remove_file(source).unwrap();
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_defined_intrinsics_keep_effects_and_override_inline_and_host_calls() {
    let original = "var calls:Int=0;\nfn env-temp-dir()->Int{calls+=1;return 42;}\nfn process-run(value:Int)->Int{calls+=1;return value;}\nfn str-len(value:Int)->Int{calls+=1;return value;}\nfn vector-clear(value:Int)->Int{calls+=1;return value;}\nfn main()->Int{if env_temp_dir()!=42 || process_run(42)!=42 || str_len(42)!=42 || vector_clear(42)!=42 || calls!=4{return 0;}return 42;}";
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("defined-intrinsic-override.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{profile:?}");
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("defined-intrinsic-override");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_string_array_globals_preserve_nested_values_and_mutable_slots() {
    let original = r#"var before:Int=20;
var words:[String]=["λ","雪",""];
var nested:[[String]]=[["left","right"],["tail"]];
var after:Int=22;
fn stringentry()->String{return nested[1][0];}
fn main()->Int{if before+after!=42 || words[0]!="λ" || words[1]!="雪" || words[2]!="" || nested[0][1]!="right" || stringentry()!="tail"{return 0;}words[0]="changed";if words[0]=="changed" && words[1]=="雪"{return 42;}return 0;}"#;
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for text in [original, canonical.as_str()] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("string-array-globals.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {text}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{profile:?}");
            request.entry = Some("stringentry".into());
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Str("tail".into())));
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("string-array-globals");
                request.entry = Some("main".into());
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_string_array_globals_return_embedded_nul_payloads() {
    let expected = "λ\0雪";
    let original =
        format!("var words:[String]=[\"{expected}\",\"\"];\nfn main()->String{{return words[0];}}");
    let canonical = crate::in_canonical::canonicalize_in_source(&original).unwrap();
    for text in [original.as_str(), canonical.as_str()] {
        let source = temp_path("string-array-nul.in");
        fs::write(&source, text).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Str(expected.into())),
                "{profile:?}"
            );
        }
        fs::remove_file(source).unwrap();
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_rust_string_array_statics_execute_reads() {
    let source = temp_path("rust-string-array-static.rs");
    fs::write(&source,r#"static WORDS: [&str;2]=["λ","雪"]; fn main()->i64{if WORDS[0]=="λ" && WORDS[1]=="雪"{return 42;}return 0;}"#).unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{profile:?} {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{profile:?}");
    }
    fs::remove_file(source).unwrap();
}

#[test]
fn owned_quoted_catch_patterns_preserve_original_and_canonical_execution() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for literal in [r#""a{b""#, r#""a}b""#, r#""escaped\"{,}雪""#] {
        let original = format!(
            "fn main()->Int {{try {{throw {literal};}} catch \"different {{ pattern\" {{return 1;}}\n catch ({literal}) {{return 42;}} catch _ {{return 0;}}}}\n"
        );
        let canonical = crate::in_canonical::canonicalize_in_source(&original).unwrap();
        for text in [original.as_str(), canonical.as_str()] {
            let source = temp_path("quoted-catch-boundary.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{profile:?}: {text}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}"
                );
            }
            fs::remove_file(source).unwrap();
        }
    }
}
#[test]
fn owned_for_headers_preserve_quoted_braces_and_iterator_evaluation() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for original in [
        r#"fn main()->Int{let count=0;for text in ["a{b","雪}c"] {if text=="a{b" || text=="雪}c"{count=count+1;}}return count+40;}"#,
        r#"var calls:Int=0;
fn values(text:String)->[Int]{calls=calls+1;if text=="λ\"{雪}"{return [20,22];}return [0,0];}
fn main()->Int{let sum=0;for value in values("λ\"{雪}") {sum=sum+value;}if calls==1{return sum;}return 0;}"#,
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for (form, text) in [("original", original), ("canonical", canonical.as_str())] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let source = temp_path("for-header-values.in");
                fs::write(&source, text).unwrap();
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(source).unwrap();
                assert!(report.success, "{form}/{profile:?}/{text}: {report:?}");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("jit-executed"),
                    "{report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{form}/{profile:?}/{text}: {report:?}"
                );
            }
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_whole_struct_matches_copy_values_and_preserve_guard_scopes() {
    for original in [
        r#"struct Packet{Int x;Float weight;Bool ready;String label}
fn main()->Int{let p:Packet=Packet{x:20,weight:1.5,ready:true,label:"λ雪"};match p {whole {p.x=0;p.label="changed";if whole.x==20 && whole.weight==1.5 && whole.ready && whole.label=="λ雪"{return whole.x+22;}return 0;}}return 0;}"#,
        r#"struct Packet{Int x;Float weight;Bool ready;String label}
var calls:Int=0;
fn make()->Packet{calls+=1;return Packet{x:20,weight:1.5,ready:true,label:"λ雪"};}
fn reject(value:Packet)->Bool{calls+=10;return false;}
fn main()->Int{let whole:Int=999;let result:Int=0;match make(){whole if reject(whole){return 0;}whole {if whole.weight==1.5 && whole.ready && whole.label=="λ雪"{result=whole.x+22;}}}if whole!=999 || calls!=11{return 0;}return result;}"#,
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            let source = temp_path("whole-struct-match.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}"
                );
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let binary = temp_path("whole-struct-match-native");
                    request.target = CompileTarget::Native;
                    request.out = Some(binary.clone());
                    let report = compile_owned(&request);
                    assert!(
                        report.success && report.degradations.is_empty(),
                        "{profile:?}: {report:?}"
                    );
                    assert_eq!(
                        std::process::Command::new(&binary).status().unwrap().code(),
                        Some(42)
                    );
                    fs::remove_file(binary).unwrap();
                }
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_canonical_closures_preserve_typed_values_and_nested_calls() {
    for original in [
        "fn main()->Int{let f=fn(value:Float)->Float{let copy=value;return copy;};if f(1.5)==1.5{return 42;}return 0;}",
        "fn main()->Int{let f=fn()->Int{let inner=fn(value:Int)->Int{return value+1;};return inner(41);};return f();}",
        "fn main()->Int{let value:Int=999;let f=fn(value:Int)->Int{return value;};return f(42);}",
        "fn main()->Int{return (fn(value:Int)->Int{return value;})(42);}",
        "fn main()->Int{let f=fn(flag:Bool,text:String)->Int{if flag && text==\"λ雪\"{return 42;}return 0;};return f(true,\"λ雪\");}",
        "var calls:Int=0;\nfn main()->Int{let f=fn()->void{calls=42;return;};f();return calls;}",
        "fn main()->Int{return fn(value:Int)->Int{return value;}(42);}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            let source = temp_path("canonical-closure.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}"
                );
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn owned_scalar_array_struct_fields_preserve_layout_lifetime_and_effects() {
    for original in [
        "struct Holder{[Int] items}\nfn main()->Int{let h=Holder{items:[20,22]};return h.items[0]+h.items[1];}",
        "struct Holder{Int before;[Int] items;Int after}\nfn main()->Int{let h=Holder{before:20,items:[],after:22};if array-len(h.items)!=0{return 0;}return h.before+h.after;}",
        "struct Holder{Int before;[Int] items;Int after}\nfn main()->Int{let h=Holder{before:10,items:[20],after:12};return h.before+h.items[0]+h.after;}",
        "struct Holder{Int before;[Int] items;Int after}\nfn main()->Int{let h=Holder{before:10,items:[1,2,3],after:26};return h.before+h.items[0]+h.items[1]+h.items[2]+h.after;}",
        "struct Holder{Int before;[Int] items;Int after}\nstruct Outer{Int before;Holder holder;Int after}\nfn main()->Int{let h=Outer{before:1,holder:Holder{before:2,items:[20,16],after:2},after:1};return h.before+h.holder.before+h.holder.items[0]+h.holder.items[1]+h.holder.after+h.after;}",
        "struct Holder{[Int] items}\nfn id(items:[Int])->[Int]{return items;}\nfn sum(items:[Int])->Int{return items[0]+items[1];}\nfn main()->Int{let h=Holder{items:[20,22]};let items=id(h.items);return sum(items);}",
        "struct Holder{Int before;[Int] items;Int after}\nfn id(value:Holder)->Holder{return value;}\nfn make()->Holder{let value=Holder{before:1,items:[20,20],after:1};return id(value);}\nfn main()->Int{let first=make();let second=make();return first.before+first.items[0]+first.items[1]+first.after+second.items[0]-20;}",
        "struct Holder{Int before;[Int] items;Int after}\nfn main()->Int{let h=Holder{before:10,items:[0],after:12};h.items=[20];return h.before+h.items[0]+h.after;}",
        "struct Holder{Int before;[Int] items;Int after}\nfn Holder-replace(self:Holder,items:[Int])->Int{self.items=items;return 0;}\nfn main()->Int{let h=Holder{before:10,items:[0],after:12};h.replace([20]);return h.before+h.items[0]+h.after;}",
        "struct Holder{[Int] items}\nfn main()->Int{let values:[Int]=[0,22];let h=Holder{items:values};values[0]=20;let alias=h.items;alias[1]=22;return h.items[0]+h.items[1];}",
        "struct Holder{Int before;[Int] items;Int after}\nvar calls:Int=0;\nfn next()->Int{calls+=1;return 20;}\nfn main()->Int{let h=Holder{before:1,items:[next(),20],after:1};if calls!=1{return 0;}return h.before+h.items[0]+h.items[1]+h.after;}",
        "struct Holder{[Int] items}\nfn fail()->[Int]{throw 9;return [0];}\nfn main()->Int{let h=Holder{items:[20,22]};try{h.items=fail();return 0;}catch 9{return h.items[0]+h.items[1];}return 0;}",
        "struct Holder{[Bool] flags;[Float] amounts;[String] words;Int after}\nfn main()->Int{let h=Holder{flags:[true,false],amounts:[1.5,2.5],words:[\"λ雪\",\"\"],after:42};if !h.flags[0] || h.flags[1] || h.amounts[0]!=1.5 || h.amounts[1]!=2.5 || h.words[0]!=\"λ雪\" || h.words[1]!=\"\"{return 0;}return h.after;}",
        "struct Holder{[Int] items}\nfn values(h:Holder)->[Int]{return h.items;}\nfn make()->[Int]{let h=Holder{items:[20,22]};return values(h);}\nfn main()->Int{let items=make();return items[0]+items[1];}",
        "struct Holder{Int x;[Int] items;Int y}\nfn total(a:Int,b:Int,c:Int,d:Int,e:Int,f:Int,g:Int,h:Holder,z:Int)->Int{return a+b+c+d+e+f+g+h.x+h.items[0]+h.items[1]+h.y+z;}\nfn main()->Int{return total(1,1,1,1,1,1,1,Holder{x:1,items:[20,12],y:1},1);}",
        "struct Holder{[Int] items}\nvar trace:Int=0;\nfn first()->Int{trace=trace*10+1;return 1;}\nfn fail()->Int{trace=trace*10+2;throw 9;}\nfn last()->Int{trace=999;return 0;}\nfn main()->Int{let h=Holder{items:[20,22]};try{h.items=[first(),fail(),last()];return 0;}catch 9{if trace==12{return h.items[0]+h.items[1];}}return 0;}",
        "class Holder{flags:[Bool];amounts:[Float];words:[String];fn change()->Int{flags[0]=true;amounts[0]=2.5;words[0]=\"λ雪\";return 0;}}\nfn main()->Int{let h=Holder{flags:[false],amounts:[0.0],words:[\"\"]};h.change();if h.flags[0] && h.amounts[0]==2.5 && h.words[0]==\"λ雪\"{return 42;}return 0;}",
        "struct Holder{[Int] items}\nfn main()->Int{let h=Holder{items:if true{[20,22]}else{[0]}};let alias=h.items;h.items={let x=40;[x,2]};alias[0]=0;return h.items[0]+h.items[1];}",
        "class Holder{items:[Int];fn index()->Int{items=[0];return 0;}}\nfn main()->Int{let h=Holder{items:[42]};return h.items[h.index()];}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            let source = temp_path("array-struct-fields.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}/{text}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}/{text}: {report:?}"
                );
                let binary = temp_path("array-struct-fields-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}/{text}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{profile:?}/{text}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
fn canonical_components_preserve_full_native_metadata_sidecars() {
    let original = r#"component Services {
 target "aarch64-unknown-none"
 deterministic true
 checkpoint full
 import first: Reader
 import second: Writer
 export public-reader: Reader
 export public-writer: Writer
 capability console: DebugConsole(write)
 capability memory: PhysicalMemory(discover, map, protect)
}
interface Reader {
 fn read()->Int
}
interface Writer {
 fn write(value:Int)->void
}
fn main()->Int{return 42;}
"#;
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    let before = crate::in_lang_parse::parse_in_source(original).unwrap();
    let after = crate::in_lang_parse::parse_in_source(&canonical).unwrap();
    assert_eq!(before, after, "{canonical}");
    assert!(
        after
            .decls
            .iter()
            .any(|decl| matches!(decl, Decl::Component { name, .. } if name == "Services"))
    );
    let mut baseline: Option<serde_json::Value> = None;
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for text in [original, canonical.as_str()] {
            let source = temp_path("canonical-component.in");
            let artifact = temp_path("canonical-component.o");
            let metadata = artifact.with_extension("component-metadata.json");
            fs::write(&source, text).unwrap();
            let mut request = default_request(
                source.clone(),
                CompileTarget::Native,
                Some("main"),
                Some(artifact.clone()),
            );
            request.target_triple = Some("aarch64-unknown-none".into());
            request.linkage = NativeLinkage::StaticLib;
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{profile:?}: {report:?}"
            );
            let bytes = fs::read(&artifact).unwrap();
            assert_eq!(&bytes[..4], b"\x7fELF");
            assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
            assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 183);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&metadata).expect("component metadata sidecar"))
                    .unwrap();
            assert_eq!(value["component"], "Services");
            assert_eq!(value["target"], "aarch64-unknown-none");
            assert_eq!(value["entry"], "main");
            assert_eq!(value["deterministic"], true);
            assert_eq!(value["checkpoint"], "full");
            assert_eq!(
                value["imports"],
                serde_json::json!([
                    {"name":"first", "interface":"Reader"},
                    {"name":"second", "interface":"Writer"}
                ])
            );
            assert_eq!(
                value["exports"],
                serde_json::json!([
                    {"name":"public-reader", "interface":"Reader"},
                    {"name":"public-writer", "interface":"Writer"}
                ])
            );
            let capabilities = serde_json::json!([
                {"name":"console", "capability_type":"DebugConsole", "args":["write"]},
                {"name":"memory", "capability_type":"PhysicalMemory", "args":["discover","map","protect"]}
            ]);
            assert_eq!(value["capabilities_required"], capabilities);
            assert_eq!(value["capabilities_exported"], capabilities);
            if let Some(expected) = &baseline {
                assert_eq!(&value, expected, "{profile:?}: {text}");
            } else {
                baseline = Some(value);
            }
            fs::remove_file(source).unwrap();
            fs::remove_file(&artifact).unwrap();
            fs::remove_file(metadata).unwrap();
            let _ = fs::remove_file(artifact.with_extension("abi.json"));
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_structured_producer_field_projections_preserve_values_and_effects() {
    for original in [
        // Both condition directions select the array header from the chosen producer.
        #[cfg(target_arch = "aarch64")]
        "struct Holder{[Int] items}\nfn main()->Int{let first=(if true{Holder{items:[20,22]}}else{Holder{items:[0]}}).items;let second=(if false{Holder{items:[0]}}else{Holder{items:[20,22]}}).items;return first[0]+second[1];}",
        // The projection crosses nested nominal layout and retains adjacent fields.
        #[cfg(target_arch = "aarch64")]
        "struct Inner{Int before;[Int] items;Int after}\nstruct Outer{Int before;Inner inner;Int after}\nfn main()->Int{let values=(if true{Outer{before:1,inner:Inner{before:2,items:[20,22],after:3},after:4}}else{Outer{before:0,inner:Inner{before:0,items:[0],after:0},after:0}}).inner.items;return values[0]+values[1];}",
        // Scalar/String fields share the same root preservation rule.
        "struct Entry{Int value;String text;Bool ready;Float weight}\nfn main()->Int{let value=(if true{Entry{value:42,text:\"λ雪\",ready:true,weight:1.5}}else{Entry{value:0,text:\"wrong\",ready:false,weight:0.0}}).value;let text=(if false{Entry{value:0,text:\"wrong\",ready:false,weight:0.0}}else{Entry{value:42,text:\"λ雪\",ready:true,weight:1.5}}).text;if text==\"λ雪\"{return value;}return 0;}",
        // A block producer keeps its lexical locals and evaluates once.
        #[cfg(target_arch = "aarch64")]
        "struct Holder{[Int] items}\nvar calls:Int=0;\nfn make()->Holder{calls+=1;return Holder{items:[20,21]};}\nfn main()->Int{let values=({let local=make();local}).items;return values[0]+values[1]+calls;}",
        // Match-value producer only evaluates the selected arm.
        #[cfg(target_arch = "aarch64")]
        "struct Holder{[Int] items}\nvar calls:Int=0;\nfn make()->Holder{calls+=1;return Holder{items:[20,21]};}\nfn fail()->Holder{throw 9;return Holder{items:[0]};}\nfn main()->Int{let values=(match 1{0 -> {fail()} _ -> {make()}}).items;return values[0]+values[1]+calls;}",
        // Every constructor field runs, including unprojected siblings; the
        // unselected conditional branch must not run or throw.
        #[cfg(target_arch = "aarch64")]
        "struct Holder{Int before;[Int] items;Int after}\nvar calls:Int=0;\nfn before()->Int{calls=calls*10+1;return 0;}\nfn after()->Int{calls=calls*10+2;return 0;}\nfn fail()->Holder{throw 9;return Holder{before:0,items:[0],after:0};}\nfn main()->Int{let values=(if true{Holder{before:before(),items:[20,22],after:after()}}else{fail()}).items;if calls!=12{return 0;}return values[0]+values[1];}",
        // A throwing selected producer cannot commit a field replacement.
        #[cfg(target_arch = "aarch64")]
        "struct Holder{[Int] items}\nfn fail()->Holder{throw 9;return Holder{items:[0]};}\nfn main()->Int{let saved=Holder{items:[20,22]};try{saved.items=(if true{fail()}else{Holder{items:[0]}}).items;return 0;}catch 9{return saved.items[0]+saved.items[1];}return 0;}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            let source = temp_path("producer-field-projection.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}"
                );
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let artifact = temp_path("producer-field-projection-native");
                    request.target = CompileTarget::Native;
                    request.out = Some(artifact.clone());
                    let report = compile_owned(&request);
                    assert!(
                        report.success && report.degradations.is_empty(),
                        "{profile:?}: {report:?}"
                    );
                    assert_eq!(
                        std::process::Command::new(&artifact)
                            .status()
                            .unwrap()
                            .code(),
                        Some(42)
                    );
                    fs::remove_file(&artifact).unwrap();
                    let _ = fs::remove_file(artifact.with_extension("abi.json"));
                }
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_nested_scalar_leaf_stores_preserve_receivers_types_and_effects() {
    for original in [
        "struct Inner{Int value}\nstruct Outer{Int before;Inner inner;Int after}\nfn main()->Int{let value=Outer{before:10,inner:Inner{value:0},after:12};value.inner.value=20;return value.before+value.inner.value+value.after;}",
        // Implicit receiver field roots must retain the complete nested path.
        "struct Inner{Int value}\nclass Outer{before:Int;inner:Inner;after:Int;fn change()->Int{inner.value=20;return 0;}}\nfn main()->Int{let value=Outer{before:10,inner:Inner{value:0},after:12};value.change();return value.before+value.inner.value+value.after;}",
        "struct Inner{Bool ready;Float weight;String text}\nstruct Outer{Int before;Inner inner;Int after}\nfn main()->Int{let value=Outer{before:20,inner:Inner{ready:false,weight:0.0,text:\"wrong\"},after:22};value.inner.ready=true;value.inner.weight=1.5;value.inner.text=\"λ雪\";if value.inner.ready && value.inner.weight==1.5 && value.inner.text==\"λ雪\"{return value.before+value.after;}return 0;}",
        "struct Inner{Int value}\nstruct Outer{Inner inner}\nvar calls:Int=0;\nfn next()->Int{calls+=1;return 41;}\nfn main()->Int{let value=Outer{inner:Inner{value:0}};value.inner.value=next();return value.inner.value+calls;}",
        "struct Inner{Int value}\nstruct Outer{Inner inner}\nfn fail()->Int{throw 9;return 0;}\nfn main()->Int{let value=Outer{inner:Inner{value:42}};try{value.inner.value=fail();return 0;}catch 9{return value.inner.value;}return 0;}",
        "struct Inner{Int value}\nstruct Outer{Inner inner}\nvar value:Int=999;\nfn original()->Int{return value;}\nfn main()->Int{let value=Outer{inner:Inner{value:0}};value.inner.value=42;if original()!=999{return 0;}return value.inner.value;}",
    ] {
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        for text in [original, canonical.as_str()] {
            let source = temp_path("nested-leaf-store.in");
            fs::write(&source, text).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{profile:?}: {text}"
                );
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    let artifact = temp_path("nested-leaf-store-native");
                    request.target = CompileTarget::Native;
                    request.out = Some(artifact.clone());
                    let report = compile_owned(&request);
                    assert!(
                        report.success && report.degradations.is_empty(),
                        "{profile:?}: {report:?}"
                    );
                    assert_eq!(
                        std::process::Command::new(&artifact)
                            .status()
                            .unwrap()
                            .code(),
                        Some(42)
                    );
                    fs::remove_file(&artifact).unwrap();
                    let _ = fs::remove_file(artifact.with_extension("abi.json"));
                }
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_raw_native_calls_preserve_scalar_abi_order_and_wrappers() {
    for text in [
        "fn answer()->Int{return 42;}\nfn main()->Int{return invoke-native(answer);}",
        "fn next(value:Int)->Int{return value+1;}\nfn main()->Int{let pointer:Int=next;return invoke_native1(pointer,41);}",
        "var calls:Int=0;\nfn add(a:Int,b:Int)->Int{return a+b;}\nfn pointer()->Int{calls=calls*10+1;return add;}\nfn first()->Int{calls=calls*10+2;return 20;}\nfn second()->Int{calls=calls*10+3;return 22;}\nfn main()->Int{let value=invoke-native2(pointer(),first(),second());if calls==123{return value;}return 0;}",
        "fn invoke(pointer:Int)->Int{return invoke-native(pointer);}\nfn invoke1(pointer:Int,value:Int)->Int{return invoke-native1(pointer,value);}\nfn answer()->Int{return 42;}\nfn next(value:Int)->Int{return value+1;}\nfn main()->Int{return invoke(answer)+invoke1(next,41)-42;}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("raw-native-call.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{text}: {report:?}"
            );
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{text}");
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("raw-native-call");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{text}: {report:?}"
                );
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42)
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[test]
fn owned_raw_native_calls_emit_freestanding_objects_and_refuse_bad_contracts() {
    let source = temp_path("raw-native-contract.in");
    let artifact = temp_path("raw-native-contract.o");
    for triple in ["x86_64-unknown-none", "aarch64-unknown-none"] {
        fs::write(
            &source,
            "fn answer()->Int{return 42;}\nfn main()->Int{return invoke-native(answer);}",
        )
        .unwrap();
        let mut request = default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(artifact.clone()),
        );
        request.target_triple = Some(triple.into());
        request.linkage = NativeLinkage::StaticLib;
        let report = compile_owned(&request);
        assert!(
            report.success && report.degradations.is_empty(),
            "{triple}: {report:?}"
        );
        assert_eq!(&fs::read(&artifact).unwrap()[..4], b"\x7fELF");
        for text in [
            "fn main()->Int{return invoke-native(0);}",
            "fn main()->Int{return invoke-native1(1,true);}",
            "fn main()->Int{return invoke-native2(1,42);}",
            "fn bad(value:Float)->Int{return 42;}\nfn main()->Int{return invoke-native1(bad,41);}",
            "fn bad(value:Int)->Int{throw 7;return value;}\nfn main()->Int{return invoke-native1(bad,42);}",
        ] {
            fs::write(&source, text).unwrap();
            fs::write(&artifact, b"preserve output").unwrap();
            let report = compile_owned(&request);
            assert!(!report.success, "{triple}/{text}: {report:?}");
            assert_eq!(fs::read(&artifact).unwrap(), b"preserve output");
        }
    }
    fs::remove_file(source).unwrap();
    fs::remove_file(artifact).unwrap();
}

#[test]
fn owned_word_port_io_emits_real_x86_instructions_and_checks_contracts() {
    let source = temp_path("word-port-io.in");
    let artifact = temp_path("word-port-io.o");
    fs::write(&source, "fn main()->Int{outw(0x3f8,42);return inw(0x3f8);}").unwrap();
    let mut request = default_request(
        source.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(artifact.clone()),
    );
    request.target_triple = Some("x86_64-unknown-none".into());
    request.linkage = NativeLinkage::StaticLib;
    let report = compile_owned(&request);
    assert!(
        report.success && report.degradations.is_empty(),
        "{report:?}"
    );
    let bytes = fs::read(&artifact).unwrap();
    assert_eq!(&bytes[..4], b"\x7fELF");
    // Compile only: privileged port instructions must never execute on the host.
    assert!(
        bytes.windows(2).any(|pair| pair == [0x66, 0xef]),
        "missing OUT DX,AX"
    );
    assert!(
        bytes.windows(2).any(|pair| pair == [0x66, 0xed]),
        "missing IN AX,DX"
    );
    for name in ["inb", "inw", "inl", "outb", "outw", "outl"] {
        let output = name.starts_with("out");
        let invalid = if output {
            vec![
                format!("{name}(1);"),
                format!("{name}(1,2,3);"),
                format!("{name}(true,2);"),
                format!("{name}(1,2.0);"),
            ]
        } else {
            vec![
                format!("{name}();"),
                format!("{name}(1,2);"),
                format!("{name}(true);"),
            ]
        };
        for call in invalid {
            fs::write(&source, format!("fn main()->Int{{{call}return 42;}}")).unwrap();
            fs::write(&artifact, b"preserve output").unwrap();
            let report = compile_owned(&request);
            assert!(!report.success, "{call}: {report:?}");
            assert_eq!(fs::read(&artifact).unwrap(), b"preserve output");
        }
    }
    fs::remove_file(source).unwrap();
    fs::remove_file(artifact).unwrap();
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(target_os = "linux", target_arch = "x86_64")
))]
fn owned_declared_word_io_names_preserve_function_overrides() {
    let source = temp_path("word-io-overrides.in");
    fs::write(&source, "fn outw(value:Int)->Int{return value;}\nfn inw(value:Int)->Int{return value;}\nfn main()->Int{return outw(20)+inw(22);}").unwrap();
    let request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
    let report = compile_owned(&request);
    assert!(
        report.success && report.degradations.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
    fs::remove_file(source).unwrap();
}

#[test]
fn sci_components_preserve_canonical_metadata_and_clear_stale_sidecars() {
    let original = r#"component Services {
 target "x86_64-unknown-none"
 deterministic true
 checkpoint full
 import reader: Reader
 export writer: Writer
 capability console: DebugConsole(write)
 capability memory: PhysicalMemory(discover, map, protect)
}
interface Reader {
 fn read()->Int
}
interface Writer {
 fn write(value:Int)->void
}
fn main()->Int{return 42;}
"#;
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    let plain = "fn main()->Int{return 42;}";
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        for text in [original, canonical.as_str()] {
            let source = temp_path("sci-metadata.in");
            fs::write(&source, text).unwrap();
            let mut baseline = None;
            for sci in [false, true] {
                let artifact = temp_path(if sci { "component.sci" } else { "component.o" });
                let metadata = artifact.with_extension("component-metadata.json");
                let mut request = default_request(
                    source.clone(),
                    CompileTarget::Native,
                    Some("main"),
                    Some(artifact.clone()),
                );
                request.target_triple = Some("x86_64-unknown-none".into());
                request.linkage = NativeLinkage::StaticLib;
                request.profile = profile;
                request.emit = sci.then_some(OwnedEmit::Sci { base: 0x100000 });
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{profile:?}: {report:?}"
                );
                let bytes = fs::read(&artifact).unwrap();
                if sci {
                    assert!(bytes.len() > crate::native_emit::sci::SCI_MANIFEST_SIZE);
                    assert_eq!(
                        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                        crate::native_emit::sci::SCI_MAGIC
                    );
                    assert_eq!(
                        u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
                        0x100040
                    );
                } else {
                    assert_eq!(&bytes[..4], b"\x7fELF");
                }
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
                assert_eq!(value["target"], "x86_64-unknown-none");
                assert_eq!(value["entry"], "main");
                assert_eq!(
                    value["imports"],
                    serde_json::json!([{"name":"reader","interface":"Reader"}])
                );
                assert_eq!(
                    value["exports"],
                    serde_json::json!([{"name":"writer","interface":"Writer"}])
                );
                assert_eq!(
                    value["capabilities_required"],
                    serde_json::json!([
                        {"name":"console","capability_type":"DebugConsole","args":["write"]},
                        {"name":"memory","capability_type":"PhysicalMemory","args":["discover","map","protect"]}
                    ])
                );
                if let Some(expected) = &baseline {
                    assert_eq!(&value, expected);
                } else {
                    baseline = Some(value);
                }

                // Reusing the exact artifact path must remove its old generated metadata.
                fs::write(&source, plain).unwrap();
                let report = compile_owned(&request);
                assert!(report.success, "{report:?}");
                assert!(!metadata.exists(), "stale metadata: {}", metadata.display());
                fs::remove_file(&artifact).unwrap();
                // A fresh component-free build also produces no sidecar.
                let report = compile_owned(&request);
                assert!(report.success, "{report:?}");
                assert!(!metadata.exists());
                fs::remove_file(&artifact).unwrap();
                let _ = fs::remove_file(artifact.with_extension("abi.json"));
                fs::write(&source, text).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn owned_float_logical_not_uses_numeric_zero_once() {
    let original = "var calls:Int=0;\nfn next()->Float{calls+=1;return -0.0;}\nfn norm(value:Float)->Bool{return !value;}\nfn main()->Int{if !next()==false{return 7;}if calls!=1{return 7;}if norm(0.0)==false{return 7;}if norm(-0.0)==false{return 7;}if norm(2.5)==true{return 7;}if norm(1.0/0.0)==true{return 7;}if norm(0.0/0.0)==true{return 7;}return 42;}";
    let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        for text in [original, canonical.as_str()] {
            let source = temp_path("float-not.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{profile:?}: {report:?}"
            );
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {report:?}"
            );
            let _ = fs::remove_file(source);
        }
    }
}

#[test]
fn explicit_native_backend_failure_preserves_reason_code_and_detail() {
    let source = temp_path("native-failure-detail.in");
    fs::write(&source, "fn main(value:Float)->Int{return 42;}").unwrap();
    for target in ["i386-unknown-none", "thumbv8m.main-none-eabi"] {
        let out = temp_path("native-failure-detail.o");
        let mut request = default_request(
            source.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(out.clone()),
        );
        request.target_triple = Some(target.into());
        request.linkage = NativeLinkage::StaticLib;
        for existing in [false, true] {
            if existing {
                fs::write(&out, b"preserve artifact").unwrap();
            }
            let report = compile_owned(&request);
            assert!(!report.success, "{report:?}");
            assert_eq!(
                report.reason_code.as_deref(),
                Some("native-target-not-implemented")
            );
            let error = report.error.as_deref().unwrap();
            assert!(
                error.contains("backend:") && error.contains("Float"),
                "{error}"
            );
            assert_eq!(report.reason.as_deref(), Some(error));
            if existing {
                assert_eq!(fs::read(&out).unwrap(), b"preserve artifact");
            } else {
                assert!(!out.exists());
            }
        }
        fs::remove_file(out).unwrap();
    }
    fs::remove_file(source).unwrap();
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "x86_64")
    )
))]
fn jit_result_type_uses_exact_resolved_entry() {
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (extension, source, entry, expected, exit) in [
            (
                "icore",
                r#"{"icoreVersion":2,"decls":[{"kind":"function","name":"App.main","params":[],"return":"Bool","body":[{"kind":"return","value":false}]},{"kind":"function","name":"main","params":[],"return":"Int","body":[{"kind":"return","value":42}]}]}"#,
                Some("main"),
                EvalValue::Int(42),
                42,
            ),
            (
                "in",
                "fn App.main()->Bool{return true;}",
                None,
                EvalValue::Bool(true),
                1,
            ),
            (
                "in",
                "fn App.main()->Bool{return true;}",
                Some("App.main"),
                EvalValue::Bool(true),
                1,
            ),
        ] {
            let path = temp_path(&format!("entry-kind.{extension}"));
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, entry, None);
            request.profile = profile;
            // Preserve the complete declaration surface so the earlier suffix
            // declaration cannot disappear under executable DCE.
            request.linkage = NativeLinkage::StaticLib;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(expected),
                "{profile:?}: {report:?}"
            );
            assert_eq!(report.eval_exit_code, Some(exit));
            fs::remove_file(path).unwrap();
        }
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "x86_64")
    )
))]
fn owned_scalar_mir_executes_and_preserves_native_fallback() {
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (source, expected, uses_mir) in [
            (
                "fn add(x:Int,y:Int)->Int{return x+y;}fn main()->Int{return add(20,22);}",
                Some(EvalValue::Int(42)),
                true,
            ),
            (
                "fn f(n:Int)->Int{if n==0{return 1;}return n*f(n - 1);}fn main()->Int{return f(3)*7;}",
                Some(EvalValue::Int(42)),
                true,
            ),
            (
                "fn yes()->Bool{return true;}fn main()->Bool{return yes();}",
                Some(EvalValue::Bool(true)),
                true,
            ),
            (
                "fn nop()->Void{return;}fn main()->Void{nop();return;}",
                None,
                true,
            ),
            (
                "fn main()->String{return \"hello\";}",
                Some(EvalValue::Str("hello".into())),
                false,
            ),
            (
                "var denominator:Int=0;fn main()->Int{return 42/denominator;}",
                Some(EvalValue::Int(1)),
                false,
            ),
            (
                "var denominator:Int=0;fn main()->Int{return 42%denominator;}",
                Some(EvalValue::Int(1)),
                false,
            ),
            // Checked arithmetic preserves its current-function failure boundary.
            (
                "fn inner(denominator:Int)->Int{return 42/denominator;}fn main()->Int{return 100+inner(0);}",
                Some(EvalValue::Int(101)),
                false,
            ),
            (
                "fn inner(denominator:Int)->Int{return 42%denominator;}fn main()->Int{return 100+inner(0);}",
                Some(EvalValue::Int(101)),
                false,
            ),
            // A mutable global preserves a nonconstant checked-division fallback.
            (
                "var denominator:Int=2;fn main()->Int{return 84/denominator;}",
                Some(EvalValue::Int(42)),
                false,
            ),
        ] {
            let path = temp_path("scalar-mir.in");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {source}: {report:?}");
            assert_eq!(report.eval_result, expected, "{profile:?} {source}");
            assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
            assert_eq!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("scalar MIR")),
                uses_mir,
                "{report:?}"
            );
            fs::remove_file(path).unwrap();
        }
        let path = temp_path("scalar-mir-error.in");
        fs::write(&path, "fn main()->Int{throw 42;return 0;}").unwrap();
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(!report.success, "{report:?}");
        assert_eq!(
            report.error.as_deref(),
            Some("jit-uncaught-error: entry `main` threw 42"),
            "{report:?}"
        );
        assert!(report.execution_started);
        fs::remove_file(path).unwrap();
        let path = temp_path("scalar-mir-compile-only.rs");
        fs::write(&path, "fn main()->(i64,i64){(20,22)}").unwrap();
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert!(report.eval_result.is_none());
        assert!(
            !report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("scalar MIR"))
        );
        fs::remove_file(path).unwrap();
    }
}

#[test]
#[cfg(any(
    all(
        target_os = "macos",
        any(target_arch = "aarch64", target_arch = "x86_64")
    ),
    all(
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "x86_64")
    )
))]
fn owned_scalar_mir_uses_resolved_entry_and_normalized_calls() {
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (source, entry) in [
            ("fn App.main()->Int{return 42;}", None),
            ("fn App.main()->Int{return 42;}", Some("App.main")),
            ("fn answer()->Int{return 42;}", None),
            (
                "fn answer-value()->Int{return 42;}fn main()->Int{return answer_value();}",
                Some("main"),
            ),
        ] {
            let path = temp_path("scalar-mir-resolved.in");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, entry, None);
            request.profile = profile;
            // Existing executable DCE does not retain library-only entry names.
            request.linkage = NativeLinkage::StaticLib;
            let report = compile_owned(&request);
            assert!(report.success, "{profile:?} {source}: {report:?}");
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
            assert_eq!(report.eval_exit_code, Some(42));
            assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
            assert!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("scalar MIR")),
                "{report:?}"
            );
            fs::remove_file(path).unwrap();
        }
    }
}

#[test]
fn owned_scalar_mir_preserves_native_intrinsic_contracts() {
    if !super::jit::host_supports_jit() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (name, diagnostic) in [
            (
                "__inrt_atomic_order_0",
                "canonical atomic intrinsic name is redeclared",
            ),
            ("__inrt_print_int", "inrt call arity mismatch"),
        ] {
            let path = temp_path("scalar-mir-reserved.in");
            // Recursion prevents inlining from erasing the reserved call.
            fs::write(&path, format!("fn {name}(n:Int,m:Int)->Int{{if n==0{{return 42;}}return {name}(n - 1,m);}}fn main()->Int{{return {name}(3,0);}}")).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            request.linkage = NativeLinkage::StaticLib;
            let report = compile_owned(&request);
            // Native x86 permits this defined runtime-name override; ARM dispatches the builtin.
            if cfg!(target_arch = "x86_64") && name == "__inrt_print_int" {
                assert!(report.success, "{profile:?} {name}: {report:?}");
                assert!(report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
                assert_eq!(report.eval_exit_code, Some(42));
                assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
            } else {
                assert!(!report.success, "{profile:?} {name}: {report:?}");
                assert!(!report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, None);
                assert!(
                    report
                        .error
                        .as_deref()
                        .is_some_and(|error| error.contains(diagnostic)),
                    "{report:?}"
                );
            }
            assert!(
                !report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("scalar MIR")),
                "{report:?}"
            );
            fs::remove_file(path).unwrap();
        }
    }
}

#[test]
fn executable_jit_keeps_resolved_fallback_before_dce() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("entry-dce.rs");
    fs::write(&path, "fn answer()->i64{42}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        // The fallback remains deliberately compile-only under the existing
        // Rust entry policy; executable DCE must still preserve its definition.
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        assert_eq!(request.linkage, NativeLinkage::Executable);
        let report = compile_owned(&request);
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert!(!report.execution_started);
        assert_eq!(report.eval_result, None);
        // Exact entry spelling remains actually invocable under the same linkage.
        request.entry = Some("answer".into());
        let report = compile_owned(&request);
        assert!(report.success, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
        assert!(report.execution_started);
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn jit_without_optimizer_root_keeps_library_discovery_policy() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("entry-less-library.rs");
    fs::write(&path, "fn first()->i64{20} fn second()->i64{22}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, None, None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert_eq!(
            report.typed_function_count, 2,
            "both discovery functions must remain: {report:?}"
        );
        assert!(!report.execution_started);
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn python_augmented_rhs_preserves_lazy_branches_and_old_binding_order() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("python-augmented.py");
    for (rhs, expected) in [
        ("(10 if False else 22)", 42),
        ("(value := 40)", 60),
        ("next_value(10 if False else 22)", 42),
    ] {
        fs::write(&path, format!("def next_value(value:int)->int:\n    return value\ndef main()->int:\n    value=20\n    value += {rhs}\n    return value\n")).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{rhs} {profile:?}: {report:?}");
            assert!(report.execution_started, "{report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{rhs} {profile:?}"
            );
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn icore_explicit_float_helpers_preserve_values_and_type_boundaries() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("explicit-float.icore");
    let original = serde_json::json!({"icoreVersion":2,"decls":[
        {"kind":"function","name":"mix","params":[{"name":"left","type":"Float"},{"name":"right","type":"Float"}],"return":"Float","body":[{"kind":"return","value":{"kind":"binary","op":"+","lhs":{"kind":"ident","name":"left"},"rhs":{"kind":"ident","name":"right"}}}]},
        {"kind":"function","name":"main","params":[],"return":"Float","body":[{"kind":"return","value":{"kind":"call","callee":"mix","args":[{"kind":"float","value":20.5},{"kind":"float","value":21.5}]}}]}
    ]});
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (op, expected) in [
            ("+", 42.0f64),
            ("-", -1.0),
            ("*", 440.75),
            ("/", 20.5 / 21.5),
        ] {
            let mut module = original.clone();
            module["decls"][0]["body"][0]["value"]["op"] = op.into();
            fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started,
                "{op} {profile:?}: {report:?}"
            );
            let Some(EvalValue::Float(actual)) = report.eval_result else {
                panic!("{report:?}")
            };
            assert_eq!(actual.to_bits(), expected.to_bits(), "{op} {profile:?}");
        }
        for (value, expected) in [
            (serde_json::json!(-0.0), -0.0f64),
            (serde_json::json!(9007199254740993u64), 9007199254740992.0),
        ] {
            let mut module = original.clone();
            module["decls"][1]["body"][0]["value"] =
                serde_json::json!({"kind":"float","value":value});
            fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started,
                "{profile:?}: {report:?}"
            );
            let Some(EvalValue::Float(actual)) = report.eval_result else {
                panic!("{report:?}")
            };
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        for op in ["xor", "^", "&", "|"] {
            let mut module = original.clone();
            module["decls"][0]["body"][0]["value"]["op"] = op.into();
            fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                !report.success && !report.execution_started,
                "{op} {profile:?}: {report:?}"
            );
            assert_eq!(report.eval_result, None);
            assert!(
                report
                    .reason_code
                    .as_deref()
                    .is_some_and(|code| code.starts_with("verify-")),
                "{report:?}"
            );
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn icore_shifts_execute_parameterized_helpers_with_word_counts() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("canonical-shifts.icore");
    let original = serde_json::json!({"icoreVersion":2,"decls":[
        {"kind":"function","name":"shift","params":[{"name":"left","type":"Int"},{"name":"right","type":"Int"}],"return":"Int","body":[{"kind":"return","value":{"kind":"binary","op":"<<","lhs":{"kind":"ident","name":"left"},"rhs":{"kind":"ident","name":"right"}}}]},
        {"kind":"function","name":"main","params":[],"return":"Int","body":[{"kind":"return","value":{"kind":"call","callee":"shift","args":[1,1]}}]}
    ]});
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (op, left, count, expected) in [
            ("<<", 21i64, 1i64, 42i64),
            (">>", 84, 1, 42),
            ("<<", 1, 32, 4294967296),
            ("<<", 1, 64, 1),
            ("<<", 1, -1, i64::MIN),
            (">>", i64::MIN, 64, i64::MIN),
            (">>", i64::MIN, -1, -1),
            (">>", -84, 1, -42),
            ("lshr", -84, 1, 9223372036854775766),
            ("lshr", -84, 64, -84),
            ("lshr", -84, -1, 1),
        ] {
            let mut module = original.clone();
            module["decls"][0]["body"][0]["value"]["op"] = op.into();
            module["decls"][1]["body"][0]["value"]["args"] = serde_json::json!([left, count]);
            fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started,
                "{op} {count} {profile:?}: {report:?}"
            );
            assert!(
                report
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("scalar MIR")),
                "{op} {count} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{op} {count} {profile:?}: {report:?}"
            );
        }
        for op in ["<<", ">>"] {
            for invalid in ["Float", "Bool", "Mixed"] {
                let mut module = original.clone();
                module["decls"][0]["body"][0]["value"]["op"] = op.into();
                if invalid == "Mixed" {
                    module["decls"][0]["params"][1]["type"] = "Bool".into();
                    module["decls"][1]["body"][0]["value"]["args"] = serde_json::json!([1, true]);
                } else {
                    module["decls"][0]["return"] = invalid.into();
                    for param in module["decls"][0]["params"].as_array_mut().unwrap() {
                        param["type"] = invalid.into();
                    }
                    module["decls"][1]["body"] = serde_json::json!([{"kind":"return","value":42}]);
                }
                fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success && !report.execution_started && report.eval_result.is_none(),
                    "{op} {invalid} {profile:?}: {report:?}"
                );
                assert!(
                    report
                        .reason_code
                        .as_deref()
                        .is_some_and(|code| code.starts_with("verify-")),
                    "{report:?}"
                );
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn canonical_bitwise_operators_execute_parameterized_helpers() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("canonical-xor.icore");
    for source in [
        r#"{"icoreVersion":2,"decls":[{"kind":"function","name":"mix","params":[{"name":"left","type":"Int"},{"name":"right","type":"Int"}],"return":"Int","body":[{"kind":"return","value":{"kind":"binary","op":"xor","lhs":{"kind":"ident","name":"left"},"rhs":{"kind":"ident","name":"right"}}}]},{"kind":"function","name":"main","params":[],"return":"Int","body":[{"kind":"return","value":{"kind":"call","callee":"mix","args":[63,21]}}]}]}"#,
        r#"{"icoreVersion":2,"decls":[{"kind":"function","name":"mix","params":[{"name":"left","type":"Bool"},{"name":"right","type":"Bool"}],"return":"Bool","body":[{"kind":"return","value":{"kind":"binary","op":"xor","lhs":{"kind":"ident","name":"left"},"rhs":{"kind":"ident","name":"right"}}}]},{"kind":"function","name":"main","params":[],"return":"Int","body":[{"kind":"if","cond":{"kind":"call","callee":"mix","args":[true,false]},"then_body":[{"kind":"return","value":42}],"else_body":[{"kind":"return","value":7}]}]}]}"#,
    ] {
        let original: serde_json::Value = serde_json::from_str(source).unwrap();
        let boolean = original["decls"][0]["return"] == "Bool";
        for op in ["xor", "&", "|"] {
            let cases = if boolean {
                vec![
                    (serde_json::json!([false, false]), 7),
                    (serde_json::json!([false, true]), 42),
                    (serde_json::json!([true, false]), 42),
                    (serde_json::json!([true, true]), 7),
                ]
            } else {
                vec![(
                    match op {
                        "&" => serde_json::json!([63, 42]),
                        "|" => serde_json::json!([40, 2]),
                        _ => serde_json::json!([63, 21]),
                    },
                    42,
                )]
            };
            for (args, xor_expected) in cases {
                let expected = if boolean {
                    let left = args[0].as_bool().unwrap();
                    let right = args[1].as_bool().unwrap();
                    match (op, left, right) {
                        ("&", true, true) => 42,
                        ("&", _, _) => 7,
                        ("|", false, false) => 7,
                        ("|", _, _) => 42,
                        _ => xor_expected,
                    }
                } else {
                    42
                };
                let mut module = original.clone();
                module["decls"][0]["body"][0]["value"]["op"] = op.into();
                let field = if boolean { "cond" } else { "value" };
                module["decls"][1]["body"][0][field]["args"] = args;
                fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    let mut request =
                        default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        report.success && report.execution_started,
                        "{op} {profile:?}: {report:?}"
                    );
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(expected)),
                        "{op} {profile:?}: {report:?}"
                    );
                }
            }
            if !boolean {
                for invalid in ["String", "Mixed"] {
                    let mut module = original.clone();
                    module["decls"][0]["body"][0]["value"]["op"] = op.into();
                    if invalid == "String" {
                        module["decls"][0]["return"] = "String".into();
                        for param in module["decls"][0]["params"].as_array_mut().unwrap() {
                            param["type"] = "String".into();
                        }
                        module["decls"][1]["body"] =
                            serde_json::json!([{"kind":"return","value":42}]);
                    } else {
                        module["decls"][0]["params"][1]["type"] = "Bool".into();
                        module["decls"][1]["body"][0]["value"]["args"] =
                            serde_json::json!([5, true]);
                    }
                    fs::write(&path, serde_json::to_string(&module).unwrap()).unwrap();
                    for profile in [
                        crate::emit_profile::EmitProfile::Default,
                        crate::emit_profile::EmitProfile::Lean,
                        crate::emit_profile::EmitProfile::Harden,
                    ] {
                        let mut request =
                            default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                        request.profile = profile;
                        let report = compile_owned(&request);
                        assert!(
                            !report.success && !report.execution_started,
                            "{op} {invalid} {profile:?}: {report:?}"
                        );
                        assert_eq!(report.eval_result, None);
                        assert!(
                            report
                                .reason_code
                                .as_deref()
                                .is_some_and(|code| code.starts_with("verify-")),
                            "{report:?}"
                        );
                    }
                }
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn lua_return_inference_refuses_unowned_operator_semantics_before_execution() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("lua-return-proof.lua");
    for (body, expected) in [
        (
            "if 63 ~ 21 == 42 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=63; if value ~ 21 == 42 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if bitvalue() ~ 21 == 42 then return 42 else return 7 end",
            Some(42),
        ),
        ("if true ~ false then return 42 else return 7 end", None),
        (
            "if 63.0 ~ 21.0 == 42 then return 42 else return 7 end",
            None,
        ),
        ("if 63.5 ~ 21 == 42 then return 42 else return 7 end", None),
        ("if 20 ~= 22 then return 42 else return 7 end", Some(42)),
        (
            "if flag() ~= false then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if flag() ~= true then return 42 else return 7 end",
            Some(7),
        ),
        ("if 42 ~= 42 then return 42 else return 7 end", Some(7)),
        ("if 20.0 ~= 22.0 then return 42 else return 7 end", Some(42)),
        ("if 42.0 ~= 42.0 then return 42 else return 7 end", Some(7)),
        (
            "if true ~= false then return 42 else return 7 end",
            Some(42),
        ),
        ("if true ~= true then return 42 else return 7 end", Some(7)),
        (
            "if \"a\" ~= \"b\" then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if \"a\" ~= \"a\" then return 42 else return 7 end",
            Some(7),
        ),
        (
            "local value=20;if value ~= 22 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=20.0;if value ~= 22.0 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=true;if value ~= false then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=\"a\";if value ~= \"b\" then return 42 else return 7 end",
            Some(42),
        ),
        ("if 1 ~= 1.0 then return 42 else return 7 end", None),
        ("if 1 ~= true then return 42 else return 7 end", None),
        ("if true & true then return 42 else return 7 end", None),
        ("if false | true then return 42 else return 7 end", None),
        ("if 5 & 3 == 1 then return 42 else return 7 end", Some(42)),
        ("if 5 | 2 == 7 then return 42 else return 7 end", Some(42)),
        (
            "local value=5; if value & 3 == 1 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=5; if value | 2 == 7 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if \"12\" + \"30\" == \"1230\" then return 42 else return 7 end",
            None,
        ),
        (
            "if 20 + 22 == 42 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if 20.0 + 22.0 == 42.0 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=20; if value + 22 == 42 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=20.0; if value + 22.0 == 42.0 then return 42 else return 7 end",
            Some(42),
        ),
        ("if 3 / 2 == 1 then return 42 else return 7 end", None),
        (
            "if 3.0 / 2.0 == 1.5 then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local value=3.0; if value / 2.0 == 1.5 then return 42 else return 7 end",
            Some(42),
        ),
        ("if -5 % 2 == -1 then return 42 else return 7 end", None),
        ("if 2 ^ 3 == 1 then return 42 else return 7 end", None),
        ("if 1 << 64 == 1 then return 42 else return 7 end", None),
        ("if -8 >> 1 == -4 then return 42 else return 7 end", None),
        ("if 0 and true then return 42 else return 7 end", None),
        (
            "local flag=true; if not flag then return 42 else return 7 end",
            Some(7),
        ),
        (
            "local flag=true; if flag and true then return 42 else return 7 end",
            Some(42),
        ),
        (
            "local flag=false; if flag or true then return 42 else return 7 end",
            Some(42),
        ),
        (
            "if flag() and true then return 42 else return 7 end",
            Some(42),
        ),
        ("if not flag() then return 42 else return 7 end", Some(7)),
        ("return not 0", None),
        ("return -5 % 2", None),
        ("return 2 ^ 3", None),
        ("local value = 5; value = value % 2; return value", None),
        ("local value = 20 + 22; return value", Some(42)),
        ("local value = -1 + 43; return value", Some(42)),
        (
            "local value = -0x8000000000000000 - 0x7fffffffffffffd6; return value",
            Some(42),
        ),
        ("local value = 20; value = 42; return value", Some(42)),
    ] {
        fs::write(
            &path,
            format!("function flag() return true end\nfunction bitvalue() return 63 end\nfunction main()\n{body}\nend\n"),
        )
        .unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            if let Some(value) = expected {
                assert!(report.success, "{body} {profile:?}: {report:?}");
                assert!(report.execution_started, "{report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(value)),
                    "{body} {profile:?}"
                );
            } else {
                assert!(!report.success, "{body} {profile:?}: {report:?}");
                assert!(!report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, None, "{body} {profile:?}");
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn objective_c_free_functions_normalize_numeric_values_without_runtime_support() {
    if !super::jit::host_supports_jit() {
        return;
    }
    let path = temp_path("objc-numeric.m");
    for source in [
        "int shift(_Bool value,int count){return value<<count;}int main(){return shift(1,1)+40;}",
        "int shift(int value,_Bool count){return value<<count;}int main(){return shift(21,1);}",
        "int shift(_Bool value,_Bool count){return value<<count;}int main(){return shift(1,1)+40;}",
        "int calculate(_Bool value){return value+41;}int main(){return calculate(1);}",
    ] {
        fs::write(&path, source).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started,
                "{source} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{source} {profile:?}: {report:?}"
            );
        }
    }
    for source in [
        "int main(){return objc_runtime_missing();}",
        "int main(){return [receiver performAction];}",
    ] {
        fs::write(&path, source).unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                !report.success && !report.execution_started && report.eval_result.is_none(),
                "{source} {profile:?}: {report:?}"
            );
            assert!(
                report
                    .reason_code
                    .as_deref()
                    .is_some_and(|code| code.starts_with("verify-")),
                "{report:?}"
            );
        }
    }
    fs::remove_file(path).unwrap();
    let generic = temp_path("strict-bool-shift.in");
    fs::write(&generic,"fn shift(value:Bool,count:Int)->Int{return value<<count;}fn main()->Int{return shift(true,1);}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(generic.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(
            !report.success && !report.execution_started && report.eval_result.is_none(),
            "{profile:?}: {report:?}"
        );
        assert_eq!(report.reason_code.as_deref(), Some("verify-type-mismatch"));
    }
    fs::remove_file(generic).unwrap();
}

#[test]
fn objective_c_numeric_normalization_preserves_word32_artifact_target() {
    let path = temp_path("objc-word32.m");
    fs::write(
        &path,
        "int shift(_Bool value,_Bool count){return value<<count;}int main(){return shift(1,1)+40;}",
    )
    .unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let output = temp_path("objc-word32.o");
        let mut request = default_request(
            path.clone(),
            CompileTarget::Native,
            Some("main"),
            Some(output.clone()),
        );
        request.profile = profile;
        request.linkage = NativeLinkage::StaticLib;
        request.target_triple = Some("i386-unknown-none".into());
        let report = compile_owned(&request);
        assert!(
            report.success && !report.execution_started,
            "{profile:?}: {report:?}"
        );
        let bytes = fs::read(&output).unwrap();
        assert_eq!(&bytes[..4], b"\x7fELF");
        assert_eq!(bytes[4], 1, "ELF32 class");
        assert_eq!(
            u16::from_le_bytes(bytes[16..18].try_into().unwrap()),
            1,
            "relocatable object"
        );
        assert_eq!(
            u16::from_le_bytes(bytes[18..20].try_into().unwrap()),
            3,
            "i386 machine"
        );
        fs::remove_file(output).unwrap();
    }
    fs::remove_file(path).unwrap();
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn boolean_negation_structured_consumers_keep_integer_results() {
    for body in [
        "let values=[-value];return values[0];",
        "let result=(if(value){-value}else{0});return result;",
    ] {
        let text = format!(
            "fn norm(value:Bool)->Int{{{body}}}fn main()->Int{{if norm(false)!=0{{return 7;}}return norm(true);}}"
        );
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let source = temp_path("boolean-negation-structured.in");
            fs::write(&source, &text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{body} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(-1)),
                "{body} {profile:?}: {report:?}"
            );
            let _ = fs::remove_file(source);
        }
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn logical_not_conditions_preserve_integer_receiver_methods_and_validation() {
    for (operand, truth) in [
        ("!42", false),
        ("!2.5", false),
        ("!\"hello\"", false),
        ("!(-0.0)", true),
        ("!true", false),
    ] {
        let conditional = format!(
            "(if({operand}){{{}}}else{{{}}})",
            if truth { 42 } else { 0 },
            if truth { 0 } else { 42 }
        );
        for body in [
            format!("return {conditional}.toStr();"),
            format!("let result={conditional};return result.toStr();"),
            format!("let result=[{conditional}];return result[0].toStr();"),
            format!("match {conditional}{{captured{{return captured.toStr();}}}}"),
            format!("let result:Int={conditional};return result.toStr();"),
        ] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let source = temp_path("logical-not-inference.in");
                fs::write(&source, format!("fn main()->String{{{body}}}")).unwrap();
                let mut request =
                    default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.execution_started && report.degradations.is_empty(),
                    "{body} {profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Str("42".into())),
                    "{body} {profile:?}: {report:?}"
                );
                let _ = fs::remove_file(source);
            }
        }
    }
    for (text, code) in [
        (
            "fn main()->Bool{return !missing;}",
            "verify-unresolved-symbol",
        ),
        (
            "fn sink()->Void{return;}fn main()->Bool{return !sink();}",
            "verify-type-mismatch",
        ),
        ("fn main()->Bool{return !();}", "verify-type-mismatch"),
    ] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let source = temp_path("logical-not-invalid.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                !report.success && !report.execution_started,
                "{text} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some(code),
                "{text} {profile:?}: {report:?}"
            );
            assert!(report.artifact_path.is_none(), "{report:?}");
            let _ = fs::remove_file(source);
        }
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn array_literal_scalar_values_materialize_once_across_profiles() {
    use crate::emit_profile::EmitProfile;
    let mut nested = "true".to_string();
    for _ in 0..8 {
        nested = format!("![{nested}]");
    }
    let nested_source = format!(
        "struct Row{{Int x;Bool flag}}fn consume(a:Int,b:Int,c:Int,d:Int,e:Int,f:Int,g:Int,row:Row)->Int{{if row.flag{{return 7;}}return a+b+c+d+e+f+g+row.x;}}fn main()->Int{{return consume(1,2,3,4,5,6,7,Row{{x:14,flag:{nested}}});}}"
    );
    for text in [
        nested_source.as_str(),
        "fn norm(value:[Int])->Bool{return !value;}fn main()->Int{if norm([1,2]){return 7;}return 42;}",
        "var calls:Int=0;fn norm(value:[Int])->Bool{calls+=1;return !value;}fn main()->Int{if norm([1,2])||calls!=1{return 7;}return 42;}",
        "fn main()->Int{let values=[1,2];if !values{return 7;}return 42;}",
        "var trace:Int=0;fn left()->Int{trace=trace*10+1;return 1;}fn right()->Int{trace=trace*10+2;return 2;}fn main()->Int{if ![left(),right()]||trace!=12{return 7;}return 42;}",
        "var trace:Int=0;fn item()->Int{trace+=1;return 1;}fn main()->Int{if false&&![item()]{return 7;}if trace!=0{return 7;}return 42;}",
        "fn sum(a:Int,b:Int)->Int{return a+b;}fn main()->Int{if sum(42,if ![1,2]{7}else{0})!=42{return 7;}return 42;}",
        "fn main()->Int{if (if true{![1,2]}else{true}){return 7;}return 42;}",
        "fn main()->Int{let value=(match (true){true -> {![1,2]}false -> {true}});if value{return 7;}return 42;}",
        "fn main()->Int{let value={let unused=0;![1,2]};if value{return 7;}return 42;}",
        "struct Row{Int x;Int y}fn main()->Int{if ![Row{x:20,y:22}]{return 7;}return 42;}",
        "fn main()->Int{let values=[0];values[0]=if ![1,2]{7}else{42};return values[0];}",
    ] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let source = temp_path("array-scalar-value.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started && report.degradations.is_empty(),
                "{text} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{text} {profile:?}: {report:?}"
            );
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                let binary = temp_path("array-scalar-value-native");
                request.target = CompileTarget::Native;
                request.out = Some(binary.clone());
                let report = compile_owned(&request);
                assert!(report.success, "{text} {profile:?}: {report:?}");
                assert_eq!(
                    std::process::Command::new(&binary).status().unwrap().code(),
                    Some(42),
                    "{text} {profile:?}"
                );
                fs::remove_file(binary).unwrap();
            }
            fs::remove_file(source).unwrap();
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn scalar_array_values_reject_unmaterializable_layouts() {
    for text in [
        "fn main()->Bool{return ![()];}",
        "fn main()->Bool{return ![[1],[2]];}",
        "struct Inner{Int x}struct Outer{Inner inner}fn main()->Bool{return ![Outer{inner:Inner{x:42}}];}",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = temp_path("array-scalar-invalid.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                !report.success && !report.execution_started && report.eval_result.is_none(),
                "{text} {profile:?}: {report:?}"
            );
            assert!(report.artifact_path.is_none(), "{report:?}");
            fs::remove_file(source).unwrap();
        }
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_retained_neg_dispatch_executes_generic_owned_shared_mutable_and_field_proofs() {
    for (name, source, expected) in [
        (
            "three-receivers",
            r#"
            struct Row{x:i64}
            impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}
            impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}
            impl std::ops::Neg for &mut Row{type Output=i64;fn neg(self)->i64{99}}
            fn negate<T:std::ops::Neg<Output=i64>>(value:T)->i64{-value}
            fn main()->i64{let mut row=Row{x:42};negate(Row{x:42})+negate(&row)+negate(&mut row)}
        "#,
            264,
        ),
        (
            "borrowed-field",
            r#"
            struct Row{x:i64}struct Holder<'a>{value:&'a Row}
            impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}
            impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}
            fn apply(holder:&Holder)->i64{-holder.value}
            fn main()->i64{let row=Row{x:42};let holder=Holder{value:&row};apply(&holder)}
        "#,
            88,
        ),
        (
            "two-forwarding-aliases",
            r#"
            struct Row{x:i64}
            impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}
            mod child {
                mod nested {pub fn negate<T:std::ops::Neg<Output=i64>>(value:T)->i64{-value}}
                pub use nested::negate as relay;
            }
            use child::relay as run;
            fn main()->i64{let row=Row{x:42};run(&row)}
        "#,
            88,
        ),
        (
            "primitive-generic",
            r#"fn negate<T:std::ops::Neg<Output=T>>(value:T)->T{-value}fn main()->i64{negate(42i64)}"#,
            -42,
        ),
    ] {
        let path = temp_path(&format!("retained-neg-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{name} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{name} {profile:?}: {report:?}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn retained_neg_bounds_do_not_discharge_unrelated_or_wrong_source_obligations() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn negate<T:std::ops::Neg<Output=i64>+MissingTrait>(value:T)->i64{-value}fn main()->i64{negate(Row{x:42})}",
        "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn negate<T:MissingTrait>(value:T)->i64{-value}fn main()->i64{negate(Row{x:42})}",
        "trait Neg{type Output;}struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn negate<T:Neg<Output=i64>>(value:T)->i64{-value}fn main()->i64{negate(Row{x:42})}",
        "fn negate<T:std::ops::Neg<Output=u64>>(value:T)->u64{-value}fn main()->i64{negate(42i64) as i64}",
        "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i32;fn neg(self)->i32{42}}fn negate<T:std::ops::Neg<Output=i32>>(value:T)->i64{-value}fn main()->i64{negate(Row{x:42})}",
        "fn negate<T:std::ops::Neg<Output=T>>(value:T)->T{-value}fn main()->i64{negate(42u64) as i64}",
    ] {
        let path = temp_path("retained-neg-bound-unproved.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let artifact = temp_path("retained-neg-bound-unproved-artifact");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(artifact.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success && !report.execution_started && report.eval_result.is_none(),
                    "{source} {target:?} {profile:?}: {report:?}"
                );
                assert!(
                    report.artifact_path.is_none()
                        && report.executable_path.is_none()
                        && report.external_invocations.is_empty()
                        && !artifact.exists(),
                    "{report:?}"
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
}
#[test]
fn raw_dependency_artifacts_retain_neg_output_proof_until_generic_normalization() {
    use crate::core_ir::{Expr, Stmt, rust_generics};
    let dependency=crate::compiler::rust_front::parse_rust_artifact_source("pub struct Row{x:i64}impl std::ops::Neg for Row{type Output=usize;fn neg(self)->usize{42}}pub fn negate<T:std::ops::Neg<Output=usize>>(value:T)->usize{-value}","dep").unwrap();
    let mut main =
        crate::compiler::rust_front::parse_rust_artifact_source("fn main()->i64{42}", "main")
            .unwrap();
    crate::cargo_linker::merge_dependency_artifacts(&mut main, vec![("dep".into(), dependency)])
        .unwrap();
    let mut proofs = Vec::new();
    crate::core_opt::map_decl_exprs_mut(&mut main.semantic.decls, &mut |expression| {
        if let Expr::Unary { op, .. } = expression
            && let Some(proof) = rust_generics::decode_neg_implementation(op)
        {
            proofs.push(proof);
        }
    });
    assert_eq!(
        proofs.len(),
        1,
        "raw artifact merge must retain authoritative proof"
    );
    assert_eq!(
        proofs[0].output,
        crate::core_ir::Typ::Named("#rust-integer-usize".into())
    );
    let normalized =
        crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &main.semantic);
    for decl in &normalized.decls {
        if let crate::core_ir::Decl::Function { body, .. } = decl {
            assert!(!body.iter().any(|statement|matches!(statement,Stmt::Expr(Expr::Unary{op,..})if rust_generics::is_neg_implementation(op))),"private implementation metadata must be removed before emission");
        }
    }
}

#[test]
fn retained_neg_post_inline_failure_aborts_before_execution() {
    let path = temp_path("ambiguous-post-inline-neg.rs");
    fs::write(
        &path,
        r#"
        struct Row{x:i64}
        impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}
        impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{99}}
        fn negate<T:std::ops::Neg<Output=i64>>(value:T)->i64{-value}
        fn main()->i64{negate(Row{x:42})}
    "#,
    )
    .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(!report.success, "{profile:?}: {report:?}");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("rust-unary-resolution-failed"),
            "{profile:?}: {report:?}"
        );
        assert!(report.reason.as_deref().unwrap_or("").contains("ambiguous"));
        assert!(report.eval_result.is_none());
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn directory_rust_crate_root_is_parsed_once_and_retains_neg_binding() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let root = temp_path("retained-root-project");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname=\"retained-root-control\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\npath=\"src/lib.rs\"\n").unwrap();
    fs::write(src.join("lib.rs"), "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn main()->i64{-Row{x:42}}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let mut request = default_request(src.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(
            report.success && report.execution_started,
            "{profile:?}: {report:?}"
        );
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(77)),
            "{profile:?}: {report:?}"
        );
    }
    #[cfg(unix)]
    {
        let alias = src.join("alias.rs");
        std::os::unix::fs::symlink(src.join("lib.rs"), &alias).unwrap();
        let report = compile_owned(&default_request(
            alias,
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        assert!(
            report.success && report.execution_started,
            "canonical source identity: {report:?}"
        );
        assert_eq!(report.eval_result, Some(EvalValue::Int(77)));
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn distinct_rust_extra_sources_cannot_share_a_neg_proof_target_origin() {
    let root = temp_path("retained-extra-conflict");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn main()->i64{-Row{x:42}}").unwrap();
    fs::write(root.join("b.rs"), "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{88}}fn unrelated()->i64{42}").unwrap();
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        let artifact = root.join(format!("{profile:?}.artifact"));
        let mut request = default_request(
            root.clone(),
            CompileTarget::Jit,
            Some("main"),
            Some(artifact.clone()),
        );
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(
            !report.success && !report.execution_started,
            "{profile:?}: {report:?}"
        );
        assert_eq!(
            report.reason_code.as_deref(),
            Some("dependency-resolution-failed"),
            "{report:?}"
        );
        assert_eq!(report.eval_result, None);
        assert!(!artifact.exists());
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn inline_argument_snapshots_preserve_order_once_lazy_and_nonlocal_exit() {
    for (name, source) in [
        (
            "duplicated-unused-lazy",
            "var trace:Int=0;fn touch(value:Int)->Int{trace=trace*10+value;return value;}fn duplicate(value:Int)->Int{return value+value;}fn ignore(value:Int)->Int{return 42;}fn positive(value:Int)->Bool{return value>0;}fn main()->Int{let first=duplicate(touch(1));let second=ignore(touch(2));if false&&positive(touch(3)){return 7;}if true||positive(touch(4)){if first==2&&second==42&&trace==12{return 42;}}return 7;}",
        ),
        (
            "global-before-write",
            "var state:Int=20;fn mutate()->Int{state=22;return 22;}fn combine(left:Int,right:Int)->Int{return left+right;}fn main()->Int{let result=combine(state,mutate());if result==42&&state==22{return 42;}return 7;}",
        ),
        (
            "argument-order",
            "var trace:Int=0;fn touch(value:Int)->Int{trace=trace*10+value;return value;}fn reverse(left:Int,right:Int)->Int{return right*10+left;}fn main()->Int{let value=reverse(touch(1),touch(2));if value==21&&trace==12{return 42;}return 7;}",
        ),
        (
            "nonlocal-exit",
            "fn ignore(value:Int)->Int{return 7;}fn main()->Int{return ignore({if true{return 42;}7});}",
        ),
    ] {
        let path = temp_path(&format!("inline-snapshot-{name}.in"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{name} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{name} {profile:?}: {report:?}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn inline_snapshots_preserve_unused_argument_type_validation() {
    let path = temp_path("inline-snapshot-invalid-argument.in");
    fs::write(&path, "fn text()->String{return \"wrong\";}fn ignore(value:Int)->Int{return 42;}fn main()->Int{return ignore(text());}").unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
        request.profile = profile;
        let report = compile_owned(&request);
        assert!(!report.success, "{profile:?}: {report:?}");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("verify-type-mismatch"),
            "{profile:?}: {report:?}"
        );
        assert!(!report.execution_started, "{profile:?}: {report:?}");
    }
    fs::remove_file(path).unwrap();
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn effectful_snapshot_inlining_preserves_array_formal_refusals_across_profiles() {
    for (name, source) in [
        (
            "effectful-wrong-array",
            r#"fn ignore(value:[i64;2])->i64{42}fn producer(flag:bool)->&'static str{if flag{return "left";}return "right";}fn main()->i64{ignore(producer(true))}"#,
        ),
        (
            "pure-wrong-array-with-effect",
            r#"fn ignore(value:[i64;2],side:i64)->i64{42}fn producer(flag:bool)->i64{if flag{return 1;}return 2;}fn main()->i64{ignore("wrong",producer(true))}"#,
        ),
    ] {
        let path = temp_path(&format!("snapshot-proof-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(!report.success, "{name} {profile:?}: {report:?}");
            assert!(
                report.eval_result.is_none(),
                "{name} {profile:?}: {report:?}"
            );
            assert!(!report.execution_started, "{name} {profile:?}: {report:?}");
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_impl_local_associated_outputs_execute_and_refuse_unresolved_contracts() {
    for (name, source, expected) in [
        (
            "owned",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->Self::Output{let value:Self::Output=77;value}}fn main()->i64{-Row{x:42}}\n",
            Some(77),
        ),
        (
            "borrowed",
            "struct Row{x:i64}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->Self::Output{88}}fn main()->i64{let value=Row{x:42};-&value}\n",
            Some(88),
        ),
        (
            "aggregate",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=Self;fn neg(self)->Self::Output{self}}fn main()->i64{(-Row{x:42}).x}\n",
            Some(42),
        ),
        (
            "u64-output",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=u64;fn neg(self)->Self::Output{18446744073709551615u64}}fn main()->i64{if (-Row{x:42})==18446744073709551615u64{42}else{7}}\n",
            Some(42),
        ),
        (
            "trait-alias",
            "use std::ops::Neg as Minus;struct Row{x:i64}impl Minus for Row{type Output=i64;fn neg(self)->Self::Output{77}}fn main()->i64{-Row{x:42}}",
            Some(77),
        ),
        (
            "scoped-traits",
            "struct Row{x:i64}trait Other{type Output;fn other(self)->Self::Output;}impl Other for Row{type Output=u64;fn other(self)->Self::Output{42u64}}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->Self::Output{77}}fn main()->i64{if (-Row{x:42})==77&&<Row as Other>::other(Row{x:42})==42u64{42}else{7}}",
            Some(42),
        ),
        (
            "qualified-projection",
            "struct Row{x:i64}trait Other{type Output;}impl Other for Row{type Output=i64;}impl std::ops::Neg for Row{type Output=<Self as Other>::Output;fn neg(self)->Self::Output{77}}fn main()->i64{-Row{x:42}}",
            None,
        ),
        (
            "cycle-invalid",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=Self::Output;fn neg(self)->Self::Output{77}}fn main()->i64{-Row{x:42}}",
            None,
        ),
        (
            "signature-invalid",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->u64{77}}fn main()->i64{-Row{x:42}}",
            None,
        ),
    ] {
        let path = temp_path(&format!("associated-output-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            if let Some(expected) = expected {
                assert!(report.success, "{name} {profile:?}: {report:?}");
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(expected)),
                    "{name} {profile:?}: {report:?}"
                );
            } else {
                assert!(!report.success, "{name} {profile:?}: {report:?}");
                assert!(!report.execution_started, "{name} {profile:?}: {report:?}");
                assert!(
                    report.eval_result.is_none(),
                    "{name} {profile:?}: {report:?}"
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_value_only_associated_cycles_refuse_even_in_dead_conditions() {
    for condition in [
        "if false { Self::Output::new(); }",
        "false && { Self::Output::new(); true };",
        "true || { Self::Output::new(); false };",
    ] {
        let source = format!(
            "struct Row;trait Build{{type Output;fn make()->i64;}}impl Build for Row{{type Output=Self::Output;fn make()->i64{{{condition}42}}}}fn main()->i64{{<Row as Build>::make()}}"
        );
        let parsed = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
        let error = crate::native_emit::c_backend::emit_c_module(&parsed).unwrap_err();
        assert!(error.contains("source-type-proof"), "{error}");
        let path = temp_path("associated-value-only-cycle.rs");
        fs::write(&path, &source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("associated-value-only-cycle.artifact");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success,
                    "{condition} {target:?} {profile:?}: {report:?}"
                );
                assert!(!report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, None);
                assert_eq!(report.artifact_path, None);
                assert!(!out.exists());
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_tuple_references_preserve_negation_receiver_identity() {
    for (name, source, expected) in [
        (
            "direct-borrow",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn evaluate()->i64{let row=Row{x:42};-&row}fn main()->i64{evaluate()}",
            88,
        ),
        (
            "inferred-tuple",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn evaluate()->i64{let row=Row{x:42};let pair=(&row,0i64);-pair.0}fn main()->i64{evaluate()}",
            88,
        ),
        (
            "typed-tuple",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn evaluate()->i64{let row=Row{x:42};let pair:(&Row,i64)=(&row,0);-pair.0}fn main()->i64{evaluate()}",
            88,
        ),
        (
            "tuple-return",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn reference(row:&Row)->(&Row,i64){(row,0)}fn evaluate()->i64{let row=Row{x:42};-reference(&row).0}fn main()->i64{evaluate()}",
            88,
        ),
        (
            "tuple-ufcs",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn evaluate()->i64{let row=Row{x:42};let pair=(&row,0i64);<&Row as std::ops::Neg>::neg(pair.0)}fn main()->i64{evaluate()}",
            88,
        ),
        (
            "combined-shapes",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn main()->i64{let row=Row{x:42};let borrowed=(&row,0i64);let owned=(Row{x:1},0i64);-borrowed.0+-owned.0}",
            165,
        ),
    ] {
        let path = temp_path(&format!("tuple-reference-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(report.success, "{name} {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{name} {profile:?}: {report:?}"
            );
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_associated_cycles_cannot_disappear_through_zero_argument_inlining() {
    for (name, output, entry, expected) in [
        ("cycle", "Self::Output", "<Row as Other>::make()", None),
        ("concrete", "i64", "<Row as Other>::make()", Some(42)),
        ("unused-cycle", "Self::Output", "42", None),
    ] {
        let source = format!(
            "trait Other{{type Output;fn make()->Self::Output;}}struct Row;\
             impl Other for Row{{type Output={output};fn make()->Self::Output{{42}}}}\
             fn main()->i64{{{entry}}}"
        );
        let path = temp_path(&format!("associated-cycle-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            if let Some(expected) = expected {
                assert!(
                    report.success && report.execution_started,
                    "{name} {profile:?}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(expected)),
                    "{name} {profile:?}: {report:?}"
                );
            } else {
                assert!(
                    !report.success && !report.execution_started,
                    "{name} {profile:?}: {report:?}"
                );
                assert!(
                    report.eval_result.is_none(),
                    "{name} {profile:?}: {report:?}"
                );
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("rust-invalid-associated-type")
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_integer_source_errors_refuse_all_profiles_without_artifacts() {
    for source in [
        "mod hidden{fn helper()->i64{42}}fn main()->i64{crate::hidden::helper()}",
        "mod hidden{pub type helper=i64;fn helper()->i64{42}}fn main()->i64{crate::hidden::helper()}",
        "fn Value(){}type Byte=Value;fn main()->i64{let value:Byte;42}",
        "fn Value(){}type Byte=Value;fn main()->i64{if false{let value:Byte;}42}",
        "fn main()->i64{u8::wrapping_add(true,1);42}",
        "fn main()->i64{u8::wrapping_add(1,1.0);42}",
        "fn main(args:Vec<String>)->i64{u8::wrapping_add(\"x\",1);42}",
        "fn main()->i64{let value=u8::wrapping_sub(1u8);42}",
        "fn main()->i64{if false{u8::wrapping_mul(1u8,2u8,3u8);}42}",
        "fn main()->i64{let value=true || u8::wrapping_add()==0u8;42}",
        "fn main()->i64{let value=u8::wrapping_mul(2u16,3u8);42}",
        "fn main()->i64{if false{u8::wrapping_sub(2u8,1u16);}42}",
        "fn main()->i64{let value=false && {let result:u16=u8::wrapping_add(1u8,2u8);true};42}",
        "fn main()->i64{if false{let value=u8::MAX+1u16;}42}",
        "fn main()->i64{if false{let value=u8::MAX==1u16;}42}",
        "fn main()->i64{if false{let value:i64=u8::MAX;}42}",
        "fn main()->i64{if false{let value:i8=u64::BITS;}42}",
        "fn main()->i64{let value=u64::MAX;if false{value();}42}",
        "fn main()->i64{if false{({u64::MAX})();}42}",
    ] {
        let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        assert!(crate::native_emit::c_backend::emit_c_module(&parsed).is_err());
        let path = temp_path("integer-source-invalid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for entry in [None, Some("main")] {
                for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                    let out = temp_path("integer-source-invalid.artifact");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success,
                        "{source} {target:?} {profile:?}: {report:?}"
                    );
                    assert!(!report.execution_started);
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.artifact_path, None);
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_used_type_alias_cycles_refuse_before_optimization_without_artifacts() {
    for (name, body) in [
        ("live-declaration", "let value:A;42"),
        ("cold-branch", "if false{let value:A;}42"),
        ("lazy-and", "false && {let value:A;true};42"),
        ("lazy-or", "true || {let value:A;false};42"),
    ] {
        let source = format!("type A=B;type B=A;fn main()->i64{{{body}}}");
        let parsed = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
        assert!(
            crate::typecheck::TypeChecker::new()
                .typed_proof_functions(&parsed.decls)
                .get("main")
                .is_some_and(crate::core_ir::type_proofs::ResolutionFailure::is_invalid),
            "{name}: {parsed:?}"
        );
        assert!(crate::native_emit::c_backend::emit_c_module(&parsed).is_err());
        let path = temp_path(&format!("type-alias-cycle-{name}.rs"));
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for entry in [None, Some("main")] {
                for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                    let out = temp_path("type-alias-cycle.artifact");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success && !report.execution_started,
                        "{name} {target:?} {entry:?} {profile:?}: {report:?}"
                    );
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{report:?}"
                    );
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_invalid_type_proofs_in_unused_code_refuse_before_entry_pruning() {
    for source in [
        "type A=B;type B=A;fn unused()->i64{let value:A;77}fn main()->i64{42}",
        "type Byte=u8;type Byte=u16;fn unused(){let value:Byte;}fn main()->i64{42}",
        "type A=B;type B=A;static UNUSED:A=0;fn main()->i64{42}",
    ] {
        let path = temp_path("unused-invalid-source.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for entry in [None, Some("main")] {
                for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                    let out = temp_path("unused-invalid-source.artifact");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success && !report.execution_started,
                        "{source} {target:?} {entry:?} {profile:?}: {report:?}"
                    );
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type")
                    );
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_type_alias_cycle_in_an_explicitly_selected_helper_refuses_without_artifacts() {
    let source = "type A=B;type B=A;fn bad()->i64{let value:A;77}fn main()->i64{42}";
    let path = temp_path("type-alias-cycle-selected-helper.rs");
    fs::write(&path, source).unwrap();
    for target in [CompileTarget::Jit, CompileTarget::Native] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let out = temp_path("type-alias-cycle-helper.artifact");
            let mut request = default_request(path.clone(), target, Some("bad"), Some(out.clone()));
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                !report.success && !report.execution_started,
                "{target:?} {profile:?}: {report:?}"
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some("rust-invalid-associated-type")
            );
            assert_eq!(report.eval_result, None);
            assert_eq!(report.artifact_path, None);
            assert_eq!(report.executable_path, None);
            assert_eq!(report.abi_path, None);
            assert!(!out.exists());
        }
    }
    fs::remove_file(path).unwrap();
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_concrete_aliases_and_scoped_imports_preserve_values() {
    for (name, source) in [
        (
            "concrete-chain",
            "type A=B;type B=i64;fn main()->i64{let value:A;42}",
        ),
        (
            "concrete-assignment",
            "type A=B;type B=i64;fn main()->i64{let value:A;value=42;value}",
        ),
        (
            "block-import-chain",
            "mod values{pub fn answer()->i64{42}}fn main()->i64{use values::answer as First;use First as Second;Second()}",
        ),
        (
            "block-import-chain-global-decoy",
            "fn First()->i64{7}mod values{pub fn answer()->i64{42}}fn main()->i64{use values::answer as First;use First as Second;Second()}",
        ),
        (
            "block-prefix-type-value-namespaces",
            "mod values{pub fn answer()->i64{42}}mod functions{pub fn values(){}}fn main()->i64{{use functions::values;use values::answer as Second;Second()}}",
        ),
        (
            "block-module-import-prefix",
            "mod values{pub fn answer()->i64{42}}fn main()->i64{use values as V;use V::answer as Second;Second()}",
        ),
        (
            "block-nested-module-import-prefix",
            "mod parent{pub mod values{pub fn answer()->i64{42}}pub fn run()->i64{use self::values as V;use V::answer as Second;Second()}}fn main()->i64{parent::run()}",
        ),
        (
            "separate-type-and-value-namespaces",
            "type Result=i64;fn Result(value:i64)->i64{if value==0{return 42;}Result(value-1)}fn main()->i64{Result(2)}",
        ),
        (
            "alias-cfg-control-0",
            "#[cfg(any())]type Byte=u16;#[cfg(not(any()))]type Byte=u8;fn main()->i64{Byte::wrapping_add(255,43) as i64}",
        ),
        (
            "alias-cfg-control-1",
            "mod other{pub type Byte=u16;}#[cfg(any())]use other::Byte;type Byte=u8;fn main()->i64{Byte::wrapping_add(255,43) as i64}",
        ),
        (
            "alias-cfg-control-2",
            "#[cfg(any())]type Byte=Byte;type Byte=u8;fn main()->i64{Byte::wrapping_add(255,43) as i64}",
        ),
        (
            "alias-cfg-control-3",
            "mod values{pub fn Byte()->i64{7}}use values::Byte;type Byte=u8;fn main()->i64{if Byte()==7{Byte::wrapping_add(255,43) as i64}else{0}}",
        ),
        (
            "alias-cfg-control-4",
            "mod values{pub fn Byte()->i64{7}}type Byte=u8;fn main()->i64{{use values::Byte;if Byte()==7{Byte::wrapping_add(255,43) as i64}else{0}}}",
        ),
        (
            "alias-cfg-control-5",
            "#[cfg_attr(all(),cfg(any()))]type Byte=u16;#[cfg_attr(any(),cfg(any()))]type Byte=u8;fn main()->i64{Byte::wrapping_add(255,43) as i64}",
        ),
        (
            "alias-cfg-control-6",
            "fn u8(){}type Byte=u8;fn main()->i64{Byte::wrapping_add(255,43) as i64}",
        ),
    ] {
        let path = temp_path(&format!("type-alias-control-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.execution_started,
                "{name} {profile:?}: {report:?}"
            );
            assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_external_module_calls_preserve_function_namespaces_across_profiles() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (name, files) in [
        (
            "direct-child",
            vec![
                ("main.rs", "mod child;fn main()->i64{child::helper()}"),
                ("child.rs", "pub fn helper()->i64{42}"),
            ],
        ),
        (
            "module-member-names",
            vec![
                (
                    "main.rs",
                    "mod child;fn main()->i64{child::wrapping_add(child::MIN,22)}",
                ),
                (
                    "child.rs",
                    "pub const MIN:i64=20;pub fn wrapping_add(left:i64,right:i64)->i64{left+right}",
                ),
            ],
        ),
        (
            "crate-sibling",
            vec![
                (
                    "main.rs",
                    "mod alpha;mod shared;fn main()->i64{alpha::answer()}",
                ),
                ("alpha.rs", "pub fn answer()->i64{crate::shared::helper()}"),
                ("shared.rs", "pub fn helper()->i64{42}"),
            ],
        ),
    ] {
        let root = temp_path(&format!("rust-module-call-{name}"));
        fs::create_dir_all(&root).unwrap();
        for (file, source) in files {
            fs::write(root.join(file), source).unwrap();
        }
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = root.join(format!("artifact-{profile:?}-{target:?}"));
                let mut request = default_request(
                    root.join("main.rs"),
                    target,
                    Some("main"),
                    Some(out.clone()),
                );
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{name} {profile:?} {target:?}: {report:?}");
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{report:?}");
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
                } else {
                    let result = std::process::Command::new(&out).output().unwrap();
                    assert_eq!(
                        result.status.code(),
                        Some(42),
                        "{name} {profile:?}: {result:?}"
                    );
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn rust_private_module_function_calls_refuse_after_root_binding() {
    for (external, rooted) in [(false, true), (true, true), (false, false), (true, false)] {
        let root = temp_path("rust-private-module-call");
        fs::create_dir_all(&root).unwrap();
        let source = if external {
            fs::write(root.join("hidden.rs"), "fn helper()->i64{42}").unwrap();
            "mod hidden;fn main()->i64{crate::hidden::helper()}"
        } else {
            "mod hidden{fn helper()->i64{42}}fn main()->i64{crate::hidden::helper()}"
        };
        let source = if rooted {
            source.to_owned()
        } else {
            source.replace("crate::hidden", "hidden")
        };
        fs::write(root.join("main.rs"), source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = root.join(format!("refused-{profile:?}-{target:?}"));
                let mut request = default_request(
                    root.join("main.rs"),
                    target,
                    Some("main"),
                    Some(out.clone()),
                );
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success && !report.execution_started,
                    "external={external} {profile:?} {target:?}: {report:?}"
                );
                assert_eq!(report.eval_result, None);
                assert!(!out.exists());
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn rust_explicit_generic_calls_preserve_concrete_kinds_and_scopes() {
    for source in [
        "type Byte=u8;fn choose<Byte>(value:Byte)->Byte{value}fn main()->i64{if choose::<u16>(65535u16)==65535u16 && Byte::MAX==255u8{42}else{7}}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{if choose::<bool>(true){choose::<i64>(42)}else{7}}",
        "fn choose<T>(value:T)->T{value}fn outer<T>(value:T)->T{choose::<T>(value)}fn main()->i64{outer::<i64>(42)}",
        "mod child{pub fn choose<T>(value:T)->T{value}}fn main()->i64{child::choose::<i64>(42)}",
        "type Byte=u16;fn choose<T>(value:T)->T{value}fn main()->i64{type Byte=u8;choose::<Byte>(42u8) as i64}",
        "mod values{pub fn choose<T>(value:T)->T{value}}use values::choose;fn main()->i64{choose::<u16>(42u16) as i64}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{let pick=choose::<i64>;pick(42)}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{let pick=choose::<u8>;let next=pick;next(42u8) as i64}",
    ] {
        let path = temp_path("rust-explicit-generic.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = temp_path("rust-explicit-generic-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{source} {profile:?} {target:?}: {report:?}"
                );
                if target == CompileTarget::Jit {
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42)
                    );
                    fs::remove_file(&out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_explicit_generic_type_errors_cannot_disappear_in_cold_branches() {
    for source in [
        "fn choose<T>(value:T)->T{value}fn main()->i64{choose::<u8>(65535u16) as i64}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{if false{choose::<u8>(65535u16);}42}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{choose::<u8,u16>(42u8) as i64}",
        "mod hidden{fn choose<T>(value:T)->T{value}}fn main()->i64{crate::hidden::choose::<i64>(42)}",
        "fn choose<T>(value:T)->T{42u8}fn main()->i64{choose::<u8>(0u8) as i64}",
        "fn choose<T>(value:T)->T{value+value}fn main()->i64{choose::<u16>(21u16) as i64}",
        "type A=A;fn choose<T>()->i64{42}fn main()->i64{choose::<A>()}",
        "fn choose<T>(value:T)->T{let ignored=value;value}fn main()->i64{choose::<u16>(42u16) as i64}",
        "fn choose<T>()->i64{42}fn main()->i64{choose::<NoSuchType>()}",
        "trait Allowed{}impl Allowed for u8{}struct Row;impl Row{fn choose<T:Allowed>(v:T)->T{v}}fn main()->i64{Row::choose::<u16>(42u16) as i64}",
        "struct Row<T>(T);fn choose<T>()->i64{42}fn main()->i64{choose::<Row>()}",
        "fn choose<T>()->i64{42}fn main()->i64{choose::<bool<u8>>()}",
        "type A=(&NoSuchType,);fn choose<T>()->i64{42}fn main()->i64{choose::<A>()}",
        "fn choose<T>(value:&T)->T{*value}fn main()->i64{choose::<u8>(&42u8) as i64}",
        "type A=A;struct Row{value:A}fn choose<T>()->i64{42}fn main()->i64{choose::<Row>()}",
        "fn choose<T>(value:&T)->T{*value}fn main()->i64{if false{choose::<u8>(&42u8);}42}",
        "fn choose<T>(value:T)->T{value}fn main()->i64{choose::<u8>(42u16) as i64}",
        "fn bad()->i64 where i64:Iterator{42}fn main()->i64{bad()}",
    ] {
        let path = temp_path("rust-explicit-generic-invalid.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = temp_path("rust-explicit-generic-invalid-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success && !report.execution_started,
                    "{source} {profile:?} {target:?}: {report:?}"
                );
                assert_eq!(report.eval_result, None);
                assert!(!out.exists());
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_fixed_array_copies_preserve_values_and_effects() {
    for (name, source) in [
        (
            "local-i64",
            "fn main()->i64{let a=[20i64,22i64];let mut b=a;b[0]=0;a[0]+a[1]}",
        ),
        (
            "assignment-i64",
            "fn main()->i64{let a=[20i64,22i64];let mut b=[0i64,0i64];b=a;b[0]=0;a[0]+a[1]}",
        ),
        (
            "assignment-inferred",
            "fn main()->i64{let a=[20i64,22i64];let mut b=[0,0];b=a;b[0]=0;a[0]+a[1]}",
        ),
        (
            "explicit-u8",
            "fn main()->i64{let a=[20u8,22u8];let mut b=a;b[0]=0u8;(a[0] as i64)+(a[1] as i64)}",
        ),
        (
            "bool-copy",
            "fn main()->i64{let a=[true,false];let mut b=a;b[0]=false;if a[0] && !a[1]{42}else{7}}",
        ),
        (
            "float-copy",
            "fn main()->i64{let a=[20.5f64,21.5f64];let mut b=a;b[0]=0.0f64;(a[0]+a[1]) as i64}",
        ),
        (
            "nested-shadow",
            "fn main()->i64{let a=[20i64,22i64];{let a=[1u8,2u8];let mut b=a;b[0]=0u8;if a[0]!=1u8{return 7;}}let mut b=a;b[0]=0;a[0]+a[1]}",
        ),
        (
            "rhs-block-once",
            "fn main()->i64{let mut trace=0i64;let a=[20i64,22i64];let mut b={trace=trace+1;a};b[0]=0;if trace==1{a[0]+a[1]}else{7}}",
        ),
        (
            "conditional-alias",
            "fn main()->i64{let a=[20i64,22i64];let mut b=if true{a}else{a};b[0]=0;a[0]+a[1]}",
        ),
        (
            "match-alias",
            "fn main()->i64{let a=[20i64,22i64];let mut b=match 1i64{0=>a,_=>a};b[0]=0;a[0]+a[1]}",
        ),
        (
            "returned-array",
            "fn values()->[i64;2]{[20i64,22i64]}fn main()->i64{let a=values();let mut b=a;b[0]=0;a[0]+a[1]}",
        ),
        (
            "byvalue-argument",
            "fn take(mut items:[i64;2])->i64{items[0]=0;items[1]}fn main()->i64{let a=[20i64,22i64];let seen=take(a);if seen==22{a[0]+a[1]}else{7}}",
        ),
        (
            "empty-bool-copy",
            "fn main()->i64{let a:[bool;0]=[];let b=a;if a.len()==0 && b.len()==0{42}else{7}}",
        ),
        (
            "constant-repeat-copy",
            "const N:usize=2;fn main()->i64{let a=[21i64;N];let mut b=a;b[0]=0;a[0]+a[1]}",
        ),
    ] {
        let path = temp_path("rust-fixed-array-copy.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = temp_path("rust-fixed-array-copy-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{name} {profile:?} {target:?}: {report:?}"
                );
                assert!(report.external_invocations.is_empty(), "{name}: {report:?}");
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{name}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{name} {profile:?}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        report.executable_path.as_deref(),
                        Some(out.to_string_lossy().as_ref()),
                        "{name}: {report:?}"
                    );
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{name} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_fixed_array_type_errors_refuse_before_optimization() {
    for (name, source) in [
        (
            "cold-wrong-length-call",
            "fn accept(items:[i64;2])->i64{items[0]}fn main()->i64{if false{accept([1i64]);}42}",
        ),
        (
            "conditional-lengths",
            "fn main()->i64{let ignored=if true{[1i64]}else{[2i64,3i64]};42}",
        ),
        (
            "match-lengths",
            "fn main()->i64{let ignored=match 0i64{0=>[1i64],_=>[2i64,3i64]};42}",
        ),
        (
            "conditional-explicit-widths",
            "fn main()->i64{let ignored=if true{[1u8]}else{[2u16]};42}",
        ),
        (
            "mixed-literal-live",
            "fn main()->i64{let ignored=[1u8,2u16];42}",
        ),
        (
            "mixed-literal-cold",
            "fn main()->i64{if false{let ignored=[1u8,2u16];}42}",
        ),
        (
            "inferred-range-backprop",
            "fn main()->i64{let a=[300];let b:[u8;1]=a;42}",
        ),
        (
            "cold-wrong-length-local",
            "fn main()->i64{if false{let ignored:[i64;2]=[1i64];}42}",
        ),
    ] {
        let path = temp_path("rust-fixed-array-invalid.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                for entry in [None, Some("main")] {
                    let out = temp_path("rust-fixed-array-invalid-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success && !report.execution_started,
                        "{name} {profile:?} {target:?} {entry:?}: {report:?}"
                    );
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty(), "{name}: {report:?}");
                    assert!(!out.exists(), "{name}: refused build left {out:?}");
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_fixed_array_external_constants_keep_copy_semantics() {
    let directory = temp_path("fixed-array-external");
    fs::create_dir(&directory).unwrap();
    let source = directory.join("main.rs");
    fs::write(
        directory.join("values.rs"),
        "pub const N:usize={let mut n=0;while n<2{n+=1;}n};",
    )
    .unwrap();
    fs::write(
        &source,
        "mod values;use values::N;fn main()->i64{let a=[21i64;N];let mut b=a;b[0]=0;a[0]+a[1]}",
    )
    .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            let out = directory.join(format!("program-{profile:?}-{target:?}"));
            let mut request =
                default_request(source.clone(), target, Some("main"), Some(out.clone()));
            request.profile = profile;
            let report = compile_owned(&request);
            assert!(
                report.success && report.degradations.is_empty(),
                "{profile:?} {target:?}: {report:?}"
            );
            assert!(report.external_invocations.is_empty(), "{report:?}");
            if target == CompileTarget::Jit {
                assert!(report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
            } else {
                assert_eq!(
                    std::process::Command::new(&out).status().unwrap().code(),
                    Some(42),
                    "{report:?}"
                );
            }
        }
    }
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn rust_empty_fixed_arrays_link_with_native_callers() {
    let directory = temp_path("empty-array-native-caller");
    fs::create_dir(&directory).unwrap();
    let source = directory.join("empty.rs");
    let caller = directory.join("caller.c");
    fs::write(&source, "fn empty()->[i64;0]{let a:[i64;0]=[];let b=a;b}").unwrap();
    fs::write(
        &caller,
        r#"
#include <stdint.h>
struct Array { uint64_t *data; uint64_t length; };
extern struct Array empty(void);
int main(void) {
    struct Array first=empty(), second=empty();
    return first.data && second.data && first.data!=second.data
        && first.length==0 && second.length==0 ? 0 : 1;
}
"#,
    )
    .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let object = directory.join(format!("empty-{profile:?}.o"));
        let executable = directory.join(format!("caller-{profile:?}"));
        let mut request = default_request(
            source.clone(),
            CompileTarget::Native,
            Some("empty"),
            Some(object.clone()),
        );
        request.profile = profile;
        request.linkage = NativeLinkage::StaticLib;
        let report = compile_owned(&request);
        assert!(
            report.success && report.external_invocations.is_empty(),
            "{report:?}"
        );
        let linked = std::process::Command::new("clang")
            .arg(&caller)
            .arg(&object)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(linked.status.success(), "{linked:?}");
        assert_eq!(
            std::process::Command::new(&executable)
                .status()
                .unwrap()
                .code(),
            Some(0)
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rust_fixed_array_external_count_errors_survive_unused_functions() {
    let directory = temp_path("fixed-array-external-invalid");
    fs::create_dir(&directory).unwrap();
    let source = directory.join("main.rs");
    fs::write(directory.join("values.rs"), "pub const N:u8=3;").unwrap();
    fs::write(
        &source,
        "mod values;fn unused()->i64{let a=[7;values::N];42}fn main()->i64{42}",
    )
    .unwrap();
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for entry in [None, Some("main")] {
                let out = directory.join(format!("program-{profile:?}-{target:?}-{entry:?}"));
                let mut request = default_request(source.clone(), target, entry, Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    !report.success && !report.execution_started,
                    "{profile:?} {target:?} {entry:?}: {report:?}"
                );
                assert_eq!(report.eval_result, None);
                assert_eq!(report.eval_exit_code, None);
                assert_eq!(report.artifact_path, None);
                assert_eq!(report.executable_path, None);
                assert_eq!(report.abi_path, None);
                assert!(report.external_invocations.is_empty(), "{report:?}");
                assert!(!out.exists(), "{report:?}");
            }
        }
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn rust_constant_negation_preserves_cold_values_and_nominal_dispatch() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (name, source) in [
        (
            "cold-global",
            "const N:i64=if true{42}else{-i64::MIN};fn main()->i64{N}",
        ),
        (
            "cold-inline",
            "fn main()->i64{(const{if true{42i8}else{-i8::MIN}}) as i64}",
        ),
        (
            "cold-global-alias",
            "const MIN:i64=i64::MIN;const N:i64=if true{42}else{-MIN};fn main()->i64{N}",
        ),
        (
            "nominal-negation",
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{self.x}}fn main()->i64{-Row{x:42}}",
        ),
    ] {
        let path = temp_path("rust-constant-negation-valid.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let out = temp_path("rust-constant-negation-valid-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{name} {profile:?} {target:?}: {report:?}");
                assert!(report.degradations.is_empty(), "{name}: {report:?}");
                assert!(report.external_invocations.is_empty(), "{name}: {report:?}");
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{name}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{name}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        report.executable_path.as_deref(),
                        Some(out.to_string_lossy().as_ref())
                    );
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{name} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_constant_negation_errors_survive_entry_pruning() {
    for (name, source) in [
        ("live-global", "const BAD:i64=-i64::MIN;fn main()->i64{BAD}"),
        ("live-inline", "fn main()->i64{(const{-i8::MIN}) as i64}"),
        (
            "unused-global-alias",
            "const MIN:i64=i64::MIN;const BAD:i64=-MIN;fn main()->i64{42}",
        ),
        (
            "unused-forward-alias",
            "const BAD:i64=-MIN;const MIN:i64=i64::MIN;fn main()->i64{42}",
        ),
        (
            "unused-inline-alias",
            "const MIN:i64=i64::MIN;fn unused()->i64{const{-MIN}}fn main()->i64{42}",
        ),
    ] {
        let path = temp_path("rust-constant-negation-invalid.rs");
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                for entry in [None, Some("main")] {
                    let out = temp_path("rust-constant-negation-invalid-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success && !report.execution_started,
                        "{name} {profile:?} {target:?} {entry:?}: {report:?}"
                    );
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{name}: {report:?}"
                    );
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty(), "{name}: {report:?}");
                    assert!(!out.exists(), "{name}: refused build left {out:?}");
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_unused_alias_declarations_refuse_before_entry_pruning() {
    for source in [
        "type Id<T>=T<u8>;fn main()->i64{let value:Id<i64>=42;value}",
        "type Bad=u8<u16>;fn main()->i64{42}",
        "type Bad=bool<u8>;fn main()->i64{42}",
        "type A<#[cfg(feature=\"x\")] T=u8>=u8;type B=A<u8,u16>;fn main()->i64{42}",
        "struct Row<#[cfg(feature=\"x\")] T>{#[cfg(feature=\"x\")] value:T}type B=Row<u8,u16>;fn main()->i64{42}",
        "mod owner{pub type Id<T=super::Id>=T;}use owner::Id;fn main()->i64{42}",
        "mod owner{pub type Id<T>=super::Id<T>;}use owner::Id;fn main()->i64{42}",
        "type Id<T=Id>=T;fn main()->i64{42}",
        "type A<T=B>=T;type B<U=A>=U;fn main()->i64{42}",
        "type A<T>=A<T>;fn main()->i64{42}",
        "type A<T>=B<T>;type B<U>=A<U>;fn main()->i64{42}",
        "type A<T>=(T,A<T>);fn main()->i64{42}",
        "type A<T>=A<(T,)>;fn main()->i64{42}",
        "type Identity<T>=T;type Wrong=Identity<u8,u16>;fn main()->i64{42}",
        "struct Row<T>{value:T}type Broken<T>=(T,Row);fn main()->i64{42}",
        "fn value(){}type Broken<T>=(T,value);fn main()->i64{42}",
        "type Bad<T,T>=(T,T);fn main()->i64{42}",
        "type A=B;type B=A;fn main()->i64{42}",
        "type Byte=u8;type Byte=u16;fn main()->i64{42}",
        "fn unused(){fn nested(){type A=A;}}fn main()->i64{42}",
        "fn unused(){mod child{type A=A;}}fn main()->i64{42}",
        "fn unused(){let callback=||{type A=A;};}fn main()->i64{42}",
        "const N:i64={type A=A;42};fn main()->i64{42}",
        "type A=A;",
    ] {
        let path = temp_path("unused-alias-declaration.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("unused-alias-declaration-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(
                        !report.success && !report.execution_started,
                        "{source}: {report:?}"
                    );
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{source}: {report:?}"
                    );
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_unused_alias_declarations_preserve_valid_and_deferred_scopes() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "type A=B;type B=u8;fn main()->i64{42}",
        "fn unused(){{mod same{pub type A=u8;}type X=same::A;}{mod same{pub type A=u16;}type X=same::A;}}fn main()->i64{42}",
        "type A=u8;fn unused(){type A=u16;mod child{type X=super::A;type Y=self::X;}}fn main()->i64{42}",
        "#[cfg(any())]mod disabled{type A=A;}fn main()->i64{42}",
        "#[cfg(feature=\"unknown\")]mod deferred{type A=A;}fn main()->i64{42}",
    ] {
        let path = temp_path("unused-alias-declaration-valid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("unused-alias-declaration-valid-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started);
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}: {report:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_generic_alias_applications_execute_with_source_type_semantics() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "trait Trait{type Assoc<U>;}type A<T:Trait,U>=T::Assoc<U>;fn main()->i64{42}",
        "trait Trait{type Assoc<U>;}type Id<V>=V;type A<T:Trait,U>=T::Assoc<Id<U>>;fn main()->i64{42}",
        "type Id<T>=T<>;fn main()->i64{let value:Id<i64>=42;value}",
        "type Id=u8<>;fn main()->i64{if Id::wrapping_add(255,43)==42{42}else{7}}",
        "type Borrow<'a,T>=&'a T;fn unused(value:&u8){let local:Borrow<u8>=value;}fn main()->i64{42}",
        "type Id<T=u8>=T;fn main()->i64{if Id::<u8>::wrapping_add(255,43)==42{42}else{7}}",
        "type Id<T=u8>=T;type Concrete=Id;fn main()->i64{if Concrete::wrapping_add(255,43)==42{42}else{7}}",
        "mod owner{pub type Id<T=super::Id<u8>>=T;}use owner::Id;fn main()->i64{let value:Id=255;if value.wrapping_add(43)==42{42}else{7}}",
        "mod owner{pub type Identity<T>=T;}use owner::Identity;fn main()->i64{let value:Identity<Identity<u8>>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Pick<'a,a>=a;fn main()->i64{42}",
        "type Identity<U>=U;type Alias<#[cfg(feature=\"x\")] T=Identity<u8,u16>>=u8;fn main()->i64{42}",
        "type Id<T=Id<u8>>=T;fn main()->i64{let value:Id=255;if value.wrapping_add(43)==42{42}else{7}}",
        "fn main()->i64{let pair:(u8,u8)=(255,43);if pair.0.wrapping_add(pair.1)==42{42}else{7}}",
        "type Identity<T>=T;fn main()->i64{let value:Identity<bool>=true;if value{42}else{7}}",
        "type Identity<T>=T;fn main()->i64{let value:Identity<f64>=20.5;if value+21.5==42.0{42}else{7}}",
        "type Identity<T>=T;fn main()->i64{let value:Identity<Identity<u8>>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Identity<T>=T;fn main()->i64{let value:Identity<u8>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Byte<T=u8>=T;fn main()->i64{let value:Byte=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Identity<T>=T;type Nested<U>=Identity<U>;fn main()->i64{let value:Nested<u8>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Pair<T>=(T,T);fn main()->i64{let pair:Pair<i64>=(20,22);pair.0+pair.1}",
        "mod owner{pub type T=u16;pub type Identity<U>=U;}type T=u8;fn main()->i64{let value:owner::Identity<T>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type T=u16;type Identity<U>=U;fn main()->i64{type T=u8;let value:Identity<T>=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type PairDefault<A=u8,B=A>=(A,B);fn main()->i64{let pair:PairDefault=(255,43);if pair.0.wrapping_add(pair.1)==42{42}else{7}}",
        "mod owner{pub type T=u8;pub type Defaulted<U=T>=U;}type T=u16;fn main()->i64{let value:owner::Defaulted=255;if value.wrapping_add(43)==42{42}else{7}}",
        "type Pair<T>=[T;2];fn main()->i64{let a:Pair<i64>=[20,22];let mut b=a;b[0]=0;a[0]+a[1]}",
    ] {
        let path = temp_path("generic-alias-application.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("generic-alias-application-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}: {report:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_generic_alias_applications_require_owned_source_facts() {
    for source in [
        "type Identity<'a,T>=T;fn main()->i64{let value:Identity<u8>=42;value as i64}",
        "type Id<T=u8>=T;fn main()->i64{if Id::wrapping_add(255,43)==42{42}else{7}}",
        "type Id<T=u8>=T;fn main()->i64{if Id::MAX==255{42}else{7}}",
    ] {
        let path = temp_path("generic-alias-value-owner.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("generic-alias-value-owner-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(!report.execution_started, "{source}: {report:?}");
                    assert_eq!(report.eval_result, None, "{source}: {report:?}");
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                    if target == CompileTarget::Native {
                        assert!(!report.success, "{source}: {report:?}");
                    }
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_unused_generic_alias_parameters_refuse_before_entry_pruning() {
    for source in [
        "type Ignored<T>=u8;fn main()->i64{42}",
        "type A<T:Copy>=u8;fn main()->i64{42}",
        "type A<T> where T:Copy =u8;fn main()->i64{42}",
        "type A<T=u8,U=T>=U;fn main()->i64{42}",
        "type Ignore<T>=u8;type A<T>=Ignore<T>;fn main()->i64{42}",
        "type Ignore<T=u8>=u8;type A<T>=Ignore<T>;fn main()->i64{42}",
        "type Ignore<T>=u8;type Next<T>=Ignore<T>;type A<T>=Next<T>;fn main()->i64{42}",
        "type First<A,B>=A;type Outer<T,U>=First<T,U>;fn main()->i64{42}",
        "trait Trait{type Item;}type A<T:Trait<Item=u8>>=u8;fn main()->i64{42}",
        "type A<T=u8,const N:usize=1>=[u8;N];fn main()->i64{42}",
        "type A<#[cfg(all())] T>=u8;fn main()->i64{42}",
    ] {
        let path = temp_path("unused-generic-alias.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("unused-generic-alias-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(!report.success, "{source}: {report:?}");
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{source}: {report:?}"
                    );
                    assert!(!report.execution_started);
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_f32_values_preserve_source_width_and_rounding() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let a:f32=16777216.0;let b:f32=1.0;if a+b==16777216.0{42}else{7}}",
        "fn main()->i64{let a=16777216f32;let b=1f32;if a+b==16777216f32{42}else{7}}",
        "fn main()->i64{let a:f32=16777216.0;let b:f32=1.0;if a+b-a==0.0{42}else{7}}",
        "fn main()->i64{let a:f64=16777216.0;let b:f64=1.0;if a+b==16777217.0{42}else{7}}",
        "fn check(x:f32)->bool{x==16777216.0}fn main()->i64{if check(16777217.0){42}else{7}}",
        "fn add(x:f32,y:f32)->f32{x+y}fn main()->i64{if add(16777216.0,1.0)==16777216.0{42}else{7}}",
        "fn main()->i64{let x:f32=16777217.0;if x==16777216.0{42}else{7}}",
        "fn main()->i64{let x:f32=1.0000000596046447753906250000001;if x==1.00000011920928955078125f32{42}else{7}}",
    ] {
        let path = temp_path("rust-f32-source-width.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-f32-source-width-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}: {report:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_f32_aggregate_values_preserve_width() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let values:[f32;1]=[16777217.0];if values[0]==16777216.0{42}else{7}}",
        "fn main()->i64{let original=[1.0f32,16777217.0];let mut copied=original;copied[0]=2.0;if original[0]==1.0 && copied[0]==2.0 && original[1]==16777216.0 && copied[1]==16777216.0{42}else{7}}",
        "fn main()->i64{let values=[0.1,0.2];if values[0]+values[1]==0.30000000000000004f64{42}else{7}}",
        "fn main()->i64{let values:[f32;2]=[16777217.0;2];if values[0]==16777216.0 && values[1]==16777216.0{42}else{7}}",
        "struct Row{value:f32}fn main()->i64{let mut row=Row{value:16777216.0};row.value=row.value+1.0;if row.value==16777216.0{42}else{7}}",
        "fn main()->i64{let mut values=(16777216.0f32,0.0f32);values.1=values.0+1.0;if values.1==16777216.0{42}else{7}}",
        "fn main()->i64{let a=[1.0000000596046447753906250000001];let x:f32=a[0];if x==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let a:[f32;1]=[1.0000000596046447753906250000001];let x:f32=a[0];if x==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let p=(1.0000000596046447753906250000001,);let x:f32=p.0;if x==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let p:(f32,)=(1.0000000596046447753906250000001,);let x:f32=p.0;if x==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let p=(1.0000000596046447753906250000001,);let q=(1.0000000596046447753906250000001,);let x:f32=p.0;let y:f64=q.0;if x==1.00000011920928955078125f32 && (y as f32)==1.0f32{42}else{7}}",
        "fn main()->i64{let p=((1.0000000596046447753906250000001,),);let x:f32=p.0.0;if x==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let a=[1.0000000596046447753906250000001];let b=a;let x:f32=b[0];if x==1.00000011920928955078125f32{42}else{7}}",
        "static mut COUNT:i64=0;fn main()->i64{let p=({unsafe{COUNT+=1;}1.0000000596046447753906250000001},);let x:f32=p.0;if x==1.00000011920928955078125f32 && unsafe{COUNT==1}{42}else{7}}",
    ] {
        let path = temp_path("rust-f32-aggregate.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-f32-aggregate-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}: {report:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
    for source in [
        "fn unused(){let values=[1.0f32,2.0f64];}fn main()->i64{42}",
        "fn main()->i64{let p=(1.0f64,);if false{let _x:f32=p.0;}42}",
    ] {
        let path = temp_path("rust-f32-mixed-array.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("rust-f32-mixed-array-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(!report.success, "{source}: {report:?}");
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{source}: {report:?}"
                    );
                    assert!(!report.execution_started);
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_f32_generic_call_contexts_preserve_precision() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn id<T>(x:T)->T{x}fn main()->i64{if id(1.0f32)==1.0f32{42}else{7}}",
        "fn id<T>(x:T)->T{x}fn main()->i64{let x:f32=id(16777217.0);if x==16777216.0{42}else{7}}",
        "fn id<T>(x:T)->T{x}fn main()->i64{let x=id::<f32>(16777217.0);if x==16777216.0{42}else{7}}",
        "fn first<T>(a:T,b:T)->T{a}fn main()->i64{if first(16777217.0,0.0f32)==16777216.0f32{42}else{7}}",
        "fn second<T>(a:T,b:T)->T{b}fn main()->i64{if second(0.0f32,16777217.0)==16777216.0f32{42}else{7}}",
        "fn id<T>(x:T)->T{x}fn main()->i64{let id=|value:f32|->f32{42.0};if id(1.0)==42.0f32{42}else{7}}",
        "fn main()->i64{let a=1.0000000596046447753906250000001;let f=||->f32{a};if f()==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let a:f32=1.0000000596046447753906250000001;let f=||->f32{a};if f()==1.00000011920928955078125f32{42}else{7}}",
        "fn main()->i64{let x=42;let f=||x;f()}",
        "fn main()->i64{let a:f32=20.0;let b:i64=22;let f=||{(a as i64)+b};f()}",
        "fn main()->i64{let factory=|x:f32|move ||x;let f=factory(42.0f32);if f()==42.0f32{42}else{7}}",
    ] {
        let path = temp_path("rust-f32-generic-context.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-f32-generic-context-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source}: {report:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}: {report:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
    let source = "fn id<T>(x:T)->T{x}fn main()->i64{let x:f32=id(1.0f64);42}";
    let path = temp_path("rust-f32-generic-mismatch.rs");
    fs::write(&path, source).unwrap();
    for target in [CompileTarget::Jit, CompileTarget::Native] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for entry in [None, Some("main")] {
                let out = temp_path("rust-f32-generic-mismatch-out");
                let mut request = default_request(path.clone(), target, entry, Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(!report.success, "{source}: {report:?}");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("rust-invalid-associated-type"),
                    "{source}: {report:?}"
                );
                assert!(!report.execution_started);
                assert_eq!(report.eval_result, None);
                assert_eq!(report.eval_exit_code, None);
                assert_eq!(report.artifact_path, None);
                assert_eq!(report.executable_path, None);
                assert_eq!(report.abi_path, None);
                assert!(report.external_invocations.is_empty());
                assert!(!out.exists());
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn mutable_captured_closures_refuse_before_execution() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let mut x=41;let mut f=||{x+=1;x};f()}",
        "fn main()->i64{let mut x=41;(||{x+=1;x})()}",
    ] {
        let path = temp_path("mutable-capture.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("mutable-capture-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(!report.success, "{report:?}");
                assert!(!report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, None);
                assert_eq!(report.eval_exit_code, None);
                assert_eq!(report.artifact_path, None);
                assert_eq!(report.executable_path, None);
                assert_eq!(report.abi_path, None);
                assert!(report.external_invocations.is_empty());
                assert!(!out.exists());
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn captured_environment_refusal_survives_native_normalization() {
    let source = "fn main()->i64{let mut x=41;let mut f=||{x+=1;x};f()}";
    let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
    crate::lower_core::desugar_module(&mut module);
    module = crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
    let before = format!("{module:#?}");
    assert!(
        crate::core_ir::type_proofs::present(&module.decls),
        "missing environment refusal: {before}"
    );
    super::optimize_retained_module(
        &mut module,
        &Default::default(),
        Some("main"),
        EmitProfile::Default,
        false,
        crate::integer_semantics::IntegerSemantics::Word64,
    )
    .unwrap();
    assert!(
        crate::core_ir::type_proofs::present(&module.decls),
        "before: {before}\nafter: {module:#?}"
    );
}

#[test]
fn rust_captured_match_guards_preserve_scope_and_execute() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{match 42{n if (||n==42)()=>return 42,_=>return 7}}",
        "fn main()->i64{let threshold=40;let f=||->i64{match 42{n if n>threshold=>return 42,_=>return 7};};f()}",
        "fn main()->i64{let threshold=40;let f=||->i64{match 42{n if n>threshold=>42,_=>7}};f()}",
        "fn main()->i64{let threshold=100;let f=||->i64{match 42{threshold if threshold==42=>return 42,_=>return 7};};let value=f();if threshold==100{value}else{7}}",
        "fn main()->i64{let threshold=100;let f=||->i64{match 42{threshold if threshold==42=>42,_=>7}};let value=f();if threshold==100{value}else{7}}",
        "fn main()->i64{let threshold=40;let f=||->i64{let answer=match 42{n if n>threshold=>42,_=>7};answer};f()}",
    ] {
        let path = temp_path("rust-captured-match-guard.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-captured-match-guard-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source} {profile:?}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source} {profile:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_integer_float_casts_preserve_precision_and_sign() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let value:i8=-127;let actual:f32=value as f32;if actual==-127.0f32{42}else{7}}",
        "fn main()->i64{let value:i8=-127;let actual:f64=value as f64;if actual==-127.0f64{42}else{7}}",
        "fn main()->i64{let value:u8=255;let actual:f32=value as f32;if actual==255.0f32{42}else{7}}",
        "fn main()->i64{let value:u8=255;let actual:f64=value as f64;if actual==255.0f64{42}else{7}}",
        "fn main()->i64{let value:i16=-32767;let actual:f32=value as f32;if actual==-32767.0f32{42}else{7}}",
        "fn main()->i64{let value:i16=-32767;let actual:f64=value as f64;if actual==-32767.0f64{42}else{7}}",
        "fn main()->i64{let value:u16=65535;let actual:f32=value as f32;if actual==65535.0f32{42}else{7}}",
        "fn main()->i64{let value:u16=65535;let actual:f64=value as f64;if actual==65535.0f64{42}else{7}}",
        "fn main()->i64{let value:i32=-2147483647;let actual:f32=value as f32;if actual==-2147483648.0f32{42}else{7}}",
        "fn main()->i64{let value:i32=-2147483647;let actual:f64=value as f64;if actual==-2147483647.0f64{42}else{7}}",
        "fn main()->i64{let value:u32=4294967295;let actual:f32=value as f32;if actual==4294967296.0f32{42}else{7}}",
        "fn main()->i64{let value:u32=4294967295;let actual:f64=value as f64;if actual==4294967295.0f64{42}else{7}}",
        "fn main()->i64{let value:i64=-9223372036854775807;let actual:f32=value as f32;if actual==-9223372036854775808.0f32{42}else{7}}",
        "fn main()->i64{let value:i64=-9223372036854775807;let actual:f64=value as f64;if actual==-9223372036854775808.0f64{42}else{7}}",
        "fn main()->i64{let value:u64=18446744073709551615;let actual:f32=value as f32;if actual==18446744073709551616.0f32{42}else{7}}",
        "fn main()->i64{let value:u64=18446744073709551615;let actual:f64=value as f64;if actual==18446744073709551616.0f64{42}else{7}}",
        "fn main()->i64{let value:isize=-9223372036854775807;let actual:f32=value as f32;if actual==-9223372036854775808.0f32{42}else{7}}",
        "fn main()->i64{let value:isize=-9223372036854775807;let actual:f64=value as f64;if actual==-9223372036854775808.0f64{42}else{7}}",
        "fn main()->i64{let value:usize=18446744073709551615;let actual:f32=value as f32;if actual==18446744073709551616.0f32{42}else{7}}",
        "fn main()->i64{let value:usize=18446744073709551615;let actual:f64=value as f64;if actual==18446744073709551616.0f64{42}else{7}}",
        "fn main()->i64{let value:i64=4611686293305294849;let actual:f32=value as f32;if actual==4611686568183201792.0f32{42}else{7}}",
        "fn main()->i64{let value:i64=4611686293305294849;let actual:f64=value as f64;if actual==4611686293305294848.0f64{42}else{7}}",
        "fn main()->i64{let value:u64=9223372586610589697;let actual:f32=value as f32;if actual==9223373136366403584.0f32{42}else{7}}",
        "fn main()->i64{let value:u64=9223372586610589697;let actual:f64=value as f64;if actual==9223372586610589696.0f64{42}else{7}}",
        "fn main()->i64{let value:i32=2147483647;let actual:f32=value as f32;if actual==2147483648.0f32{42}else{7}}",
        "fn main()->i64{let value:i32=2147483647;let actual:f64=value as f64;if actual==2147483647.0f64{42}else{7}}",
        "fn main()->i64{let value:u64=9223372036854775808;let actual:f32=value as f32;if actual==9223372036854775808.0f32{42}else{7}}",
        "fn main()->i64{let value:u64=9223372036854775808;let actual:f64=value as f64;if actual==9223372036854775808.0f64{42}else{7}}",
        "fn main()->i64{let mut count:i64=0;let value:i64=4611686293305294849;let actual:f32={count+=1;value} as f32;if actual==4611686568183201792.0f32 && count==1{42}else{7}}",
        "fn main()->i64{let mut count:i64=0;let value:i64=4611686293305294849;let actual:f64={count+=1;value} as f64;if actual==4611686293305294848.0f64 && count==1{42}else{7}}",
    ] {
        let path = temp_path("rust-integer-float.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-integer-float-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source} {profile:?}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source} {profile:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_integer_float_casts_preserve_default_integer_range_refusals() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let value=18446744073709551615;if false{let x=value as f32;}42}",
        "fn main()->i64{let value:i64=18446744073709551615;if false{let x=value as f64;}42}",
        "fn main()->i64{if false{let x=2147483648 as f32;}42}",
        "fn main()->i64{let value=2147483648;if false{let x=value as f64;}42}",
        "fn unused()->f32{2147483648 as f32}fn main()->i64{42}",
        "fn main()->i64{let value:i8=128;if false{let x=value as f32;}42}",
    ] {
        let path = temp_path("rust-integer-float-invalid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("rust-integer-float-invalid-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(!report.success, "{source} {profile:?}: {report:?}");
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{source}: {report:?}"
                    );
                    assert!(!report.execution_started);
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_integer_literal_casts_infer_source_types_and_preserve_explicit_sources() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let value=2147483648 as i64;if value==2147483648i64{42}else{7}}",
        "fn main()->i64{let value=2147483648 as u32;if value==2147483648u32{42}else{7}}",
        "fn main()->i64{let value=18446744073709551615 as u64;if value==u64::MAX{42}else{7}}",
        "fn main()->i64{let value=18446744073709551615 as usize;if value==usize::MAX{42}else{7}}",
        "fn main()->i64{let value=(-2147483649) as i64;if value==(-2147483649i64){42}else{7}}",
        "fn main()->i64{let value=-9223372036854775808 as i64;if value==i64::MIN{42}else{7}}",
        "fn main()->i64{let value=(!2147483648) as i64;if value==(-2147483649i64){42}else{7}}",
        "fn main()->i64{let value=(-(-2147483648)) as i64;if value==2147483648i64{42}else{7}}",
        "fn main()->i64{let value=256i64 as u8;if value==0u8{42}else{7}}",
        "fn main()->i64{let value=(-1i32) as u64;if value==u64::MAX{42}else{7}}",
        "fn main()->i64{let value=18446744073709551615usize as u64;if value==u64::MAX{42}else{7}}",
        "fn main()->i64{if (true as u8)==1u8{42}else{7}}",
        "fn main()->i64{let mut count=0;let value={count+=1;256i64} as u8;if value==0u8 && count==1{42}else{7}}",
        "fn main()->i64{let mut count=0;let value={count+=1;2147483648} as i64;if value==2147483648i64 && count==1{42}else{7}}",
        "fn main()->i64{let value=-{2147483648} as i64;if value==(-2147483648i64){42}else{7}}",
        "fn main()->i64{let value={let prior=-1;0} as u8;if value==0u8{42}else{7}}",
        "fn main()->i64{let mut count=0;let value=(unsafe{count+=1;2147483648}) as i64;if value==2147483648i64 && count==1{42}else{7}}",
        "fn main()->i64{let value=(const{2147483648}) as i64;if value==2147483648i64{42}else{7}}",
    ] {
        let path = temp_path("rust-integer-literal-cast.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-integer-literal-cast-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{source} {profile:?}: {report:?}");
                assert!(report.degradations.is_empty(), "{source}: {report:?}");
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{source}: {report:?}");
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source} {profile:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_integer_literal_casts_refuse_invalid_sources_before_execution() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let value=256 as u8;42}",
        "fn main()->i64{if false{let value=128 as i8;}42}",
        "fn main()->i64{let value=(-129) as i8;42}",
        "fn main()->i64{let value=(!256) as u8;42}",
        "fn main()->i64{let value=-1 as u64;42}",
        "fn main()->i64{let value=(-0) as u8;42}",
        "fn main()->i64{let value=-(0) as u8;42}",
        "fn main()->i64{if false{let value=(-(-1)) as u8;}42}",
        "fn main()->i64{let value=18446744073709551615 as i64;42}",
        "fn main()->i64{let value=18446744073709551615 as u32;42}",
        "fn main()->i64{let value=2147483648 as i32;42}",
        "fn main()->i64{let prior=2147483648;let value=prior as i64;42}",
        "fn main()->i64{let value=(2147483648+0) as i64;42}",
        "fn main()->i64{let value=2147483648 as f64;42}",
        "fn unused()->u8{256 as u8}fn main()->i64{42}",
        "fn main()->i64{let value={-0} as u8;42}",
        "fn main()->i64{let value=-{0} as u8;42}",
        "fn main()->i64{let value={let prior=2147483648;0} as i64;42}",
        "fn main()->i64{let value={if 2147483648==0{};0} as i64;42}",
        "fn main()->i64{let value=(unsafe{-0}) as u8;42}",
        "fn main()->i64{let value=(const{-0}) as u8;42}",
        "fn main()->i64{let value=(const{let prior=2147483648;0}) as i64;42}",
    ] {
        let path = temp_path("rust-integer-literal-cast-invalid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                for entry in [None, Some("main")] {
                    let out = temp_path("rust-integer-literal-cast-invalid-out");
                    let mut request =
                        default_request(path.clone(), target, entry, Some(out.clone()));
                    request.profile = profile;
                    let report = compile_owned(&request);
                    assert!(!report.success, "{source} {profile:?}: {report:?}");
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{source}: {report:?}"
                    );
                    assert!(!report.execution_started);
                    assert_eq!(report.eval_result, None);
                    assert_eq!(report.eval_exit_code, None);
                    assert_eq!(report.artifact_path, None);
                    assert_eq!(report.executable_path, None);
                    assert_eq!(report.abi_path, None);
                    assert!(report.external_invocations.is_empty());
                    assert!(!out.exists());
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_ieee_bit_conversions_preserve_transfers_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let patterns32 = [
        0u32, 0x80000000, 1, 0x007fffff, 0x7f7fffff, 0x7f800000, 0xff800000, 0x7f800001,
        0xff800001, 0x7fc12345, 0xffc12345,
    ];
    let patterns64 = [
        0u64,
        0x8000000000000000,
        1,
        0x000fffffffffffff,
        0x7fefffffffffffff,
        0x7ff0000000000000,
        0xfff0000000000000,
        0x7ff0000000000001,
        0xfff0000000000001,
        0x7ff8123456789abc,
        0xfff8123456789abc,
    ];
    let checks = patterns32
        .iter()
        .map(|bits| format!("check32({bits}u32)"))
        .chain(patterns64.iter().map(|bits| format!("check64({bits}u64)")))
        .collect::<Vec<_>>()
        .join(" && ");
    for definitions in [
        "fn check32(bits:u32)->bool{f32::from_bits(bits).to_bits()==bits}fn check64(bits:u64)->bool{f64::from_bits(bits).to_bits()==bits}",
        "fn id32(value:f32)->f32{value}fn id64(value:f64)->f64{value}fn check32(bits:u32)->bool{id32(f32::from_bits(bits)).to_bits()==bits}fn check64(bits:u64)->bool{id64(f64::from_bits(bits)).to_bits()==bits}",
        "struct Row32{value:f32}struct Row64{value:f64}fn check32(bits:u32)->bool{let row=Row32{value:f32::from_bits(bits)};row.value.to_bits()==bits}fn check64(bits:u64)->bool{let row=Row64{value:f64::from_bits(bits)};row.value.to_bits()==bits}",
        "fn check32(bits:u32)->bool{let values=[f32::from_bits(bits)];values[0].to_bits()==bits}fn check64(bits:u64)->bool{let values=[f64::from_bits(bits)];values[0].to_bits()==bits}",
        "fn check32(bits:u32)->bool{let values=vec![f32::from_bits(bits)];values[0].to_bits()==bits}fn check64(bits:u64)->bool{let values=vec![f64::from_bits(bits)];values[0].to_bits()==bits}",
        "fn identity<T>(value:T)->T{value}fn check32(bits:u32)->bool{identity(f32::from_bits(bits)).to_bits()==bits}fn check64(bits:u64)->bool{identity(f64::from_bits(bits)).to_bits()==bits}",
        "fn check32(bits:u32)->bool{let id=|value:f32|->f32{value};id(f32::from_bits(bits)).to_bits()==bits}fn check64(bits:u64)->bool{let id=|value:f64|->f64{value};id(f64::from_bits(bits)).to_bits()==bits}",
        "static mut COUNT:i64=0;fn input32(bits:u32)->u32{unsafe{COUNT+=1;}bits}fn input64(bits:u64)->u64{unsafe{COUNT+=1;}bits}fn check32(bits:u32)->bool{let before=unsafe{COUNT};let value=f32::from_bits(input32(bits));value.to_bits()==bits && unsafe{COUNT==before+1}}fn check64(bits:u64)->bool{let before=unsafe{COUNT};let value=f64::from_bits(input64(bits));value.to_bits()==bits && unsafe{COUNT==before+1}}",
    ] {
        let source = format!("{definitions}fn main()->i64{{if {checks}{{42}}else{{7}}}}");
        let path = temp_path("rust-ieee-transfer.rs");
        fs::write(&path, &source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-ieee-transfer-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{source} {target:?}/{profile:?}: {report:?}"
                );
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started);
                    assert_eq!(
                        report.eval_result,
                        Some(EvalValue::Int(42)),
                        "{source} {profile:?}"
                    );
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_ieee_bit_conversions_refuse_invalid_primitive_sources() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main()->i64{let value=f32::from_bits(1i32);42}",
        "fn main()->i64{if false{let value=f32::from_bits(1usize);}42}",
        "fn main()->i64{let value=f64::from_bits(1i64);42}",
        "fn main()->i64{let bits=1usize;let value=f64::from_bits(bits);42}",
        "fn source()->usize{1usize}fn main()->i64{let value=f64::from_bits(source());42}",
        "fn main()->i64{let value=f64::from_bits(1u32);42}",
        "fn main()->i64{let value=f32::from_bits(1u64);42}",
        "fn main()->i64{let value:usize=1.0f64.to_bits();42}",
        "fn main()->i64{let value:u64=1.0f32.to_bits();42}",
        "fn main()->i64{let value=f32::from_bits();42}",
        "fn main()->i64{let value=f64::from_bits(1u64,2u64);42}",
        "fn main()->i64{let value=f32::to_bits(1.0f64);42}",
        "fn main()->i64{let value=f64::to_bits(1.0f32);42}",
    ] {
        let path = temp_path("rust-ieee-invalid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-ieee-invalid-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(!report.success, "{source}: {report:?}");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("rust-invalid-associated-type"),
                    "{source}: {report:?}"
                );
                assert!(!report.execution_started);
                assert_eq!(report.eval_result, None);
                assert_eq!(report.artifact_path, None);
                assert!(report.external_invocations.is_empty());
                assert!(!out.exists());
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_ieee_trait_static_calls_preserve_owned_scope_and_dispatch_priority() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        r#"mod defs{pub trait Make{fn value()->f32;}pub struct Row;impl Make for Row{fn value()->f32{42.0}}}type Make=u8;fn main()->i64{{use defs::Make;if defs::Row::value()==42.0f32{42}else{7}}}"#,
        r#"mod m{pub trait Make{fn value()->i64;}pub struct Hidden;impl Make for Hidden{fn value()->i64{42}}pub type Alias=Hidden;}use m::Make;fn main()->i64{m::Alias::value()}"#,
        r#"#[derive(Default)]struct Item;trait Make{fn default()->i64;}impl Make for Item{fn default()->i64{7}}impl Item{fn default()->i64{42}}fn main()->i64{Item::default()}"#,
        r#"pub struct Item;pub trait Make{fn value()->i64;}mod implementations{use super::Make;impl Make for crate::Item{fn value()->i64{42}}}fn main()->i64{Item::value()}"#,
        r#"mod traits{pub trait Make{fn value()->i64;}}use traits::Make as Alias;struct Item;impl Alias for Item{fn value()->i64{42}}fn main()->i64{Item::value()}"#,
        r#"trait Factory { fn value()->f32; } struct Item; impl Factory for Item { fn value()->f32{42.0} } fn main()->i64{if Item::value()==42.0f32{42}else{7}}"#,
        r#"trait Factory { fn value()->f32; } struct Item; impl Factory for Item{fn value()->f32{7.0}} impl Item{fn value()->f32{42.0}} fn main()->i64{if Item::value()==42.0f32{42}else{7}}"#,
        r#"mod source{pub trait Factory{fn value()->f32;} pub struct Item; impl Factory for Item{fn value()->f32{42.0}}} use source::Factory as Make; fn main()->i64{if source::Item::value()==42.0{42}else{7}}"#,
        r#"trait Factory{fn value()->f32;} struct Item; impl Factory for Item{fn value()->f32{42.0}} type Alias=Item; fn main()->i64{if Alias::value()==42.0{42}else{7}}"#,
        r#"#[cfg(any())] trait Hidden{fn value()->f32;} trait Factory{fn value()->f32;} struct Item; impl Factory for Item{#[cfg(not(any()))] fn value()->f32{42.0} #[cfg(any())] fn value()->f32{7.0}} fn main()->i64{if Item::value()==42.0{42}else{7}}"#,
        r#"mod source{pub trait Factory{fn value()->f32;} pub struct Item; impl Factory for Item{fn value()->f32{42.0}}} fn main()->i64{if <source::Item as source::Factory>::value()==42.0{42}else{7}}"#,
        r#"trait Factory{fn value()->i64;} struct Item; impl Factory for Item{fn value()->i64{42}} fn main()->i64{Item::value()}"#,
        r#"trait Make{type Output;fn value()->Self::Output;}struct Factory;impl Make for Factory{type Output=f32;fn value()->Self::Output{Self::Output::from_bits(2139095041u32)}}fn main()->i64{if Factory::value().to_bits()==2139095041u32{42}else{7}}"#,
    ] {
        let path = temp_path("rust-ieee-trait-static.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-ieee-trait-static-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(
                    report.success && report.degradations.is_empty(),
                    "{source} {target:?}/{profile:?}: {report:?}"
                );
                assert!(report.external_invocations.is_empty());
                if target == CompileTarget::Jit {
                    assert!(report.execution_started);
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
                } else {
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{source}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_ieee_trait_static_calls_refuse_invalid_source_contracts() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        r#"trait A{fn value()->f32;} trait B{fn value()->f32;} struct Item; impl A for Item{fn value()->f32{42.0}} impl B for Item{fn value()->f32{7.0}} fn main()->i64{if Item::value()==42.0{42}else{7}}"#,
        r#"mod source{pub trait Factory{fn value()->f32;} pub struct Item; impl Factory for Item{fn value()->f32{42.0}}} fn main()->i64{if source::Item::value()==42.0{42}else{7}}"#,
        r#"trait Make{fn value()->bool;}struct Row;impl Make for Row{fn value()->i64{42}}fn main()->i64{Row::value()}"#,
        r#"trait Make{fn value(x:bool)->i64;}struct Row;impl Make for Row{fn value(x:i64)->i64{x}}fn main()->i64{Row::value(42)}"#,
        r#"trait Make{fn value(x:i64)->i64;}struct Row;impl Make for Row{fn value()->i64{42}}fn main()->i64{Row::value()}"#,
        r#"trait Make{fn value()->Result<i64,bool>;}struct Row;impl Make for Row{fn value()->i64{42}}fn main()->i64{Row::value()}"#,
        r#"trait Make{fn value()->Result<i64,bool>;}struct Row;impl Make for Row{fn value()->Result<i64,u8>{Ok(42)}}fn main()->i64{match Row::value(){Ok(value)=>value,Err(_)=>7}}"#,
        r#"trait Make{fn value(x:&str)->i64;}struct Row;impl Make for Row{fn value(x:&String)->i64{42}}fn main()->i64{let x=String::from("x");Row::value(&x)}"#,
        r#"type Text=str;type Owned=String;trait Make{fn value(x:&Text)->i64;}struct Row;impl Make for Row{fn value(x:&Owned)->i64{42}}fn main()->i64{let x=String::from("x");Row::value(&x)}"#,
        r#"trait Make{fn value()->Self::Extra;}struct Row;impl Make for Row{type Extra=i64;fn value()->i64{42}}fn main()->i64{Row::value()}"#,
    ] {
        let path = temp_path("rust-ieee-trait-static-invalid.rs");
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path("rust-ieee-trait-static-invalid-out");
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(!report.success, "{source}: {report:?}");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("rust-invalid-associated-type"),
                    "{source}: {report:?}"
                );
                assert!(!report.execution_started);
                assert_eq!(report.eval_result, None);
                assert_eq!(report.artifact_path, None);
                assert!(report.external_invocations.is_empty());
                assert!(!out.exists());
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]
#[test]
fn rust_retained_neg_result_casts_preserve_operand_effects_once() {
    for (name, source) in [
        (
            "explicit-result-cast",
            r#"struct Row{x:i64}impl std::ops::Neg for Row{type Output=i32;fn neg(self)->i32{42}}fn negate<T:std::ops::Neg<Output=i32>>(value:T)->i64{(-value) as i64}fn main()->i64{negate(Row{x:7})}"#,
        ),
        (
            "operand-effect-once",
            r#"static mut COUNT:i64=0;struct Row{x:i64}impl std::ops::Neg for Row{type Output=i32;fn neg(self)->i32{42}}fn negate<T:std::ops::Neg<Output=i32>>(value:T)->i64{(-{unsafe{COUNT+=1;}value}) as i64}fn main()->i64{let answer=negate(Row{x:7});if unsafe{COUNT==1}{answer}else{7}}"#,
        ),
    ] {
        let path = temp_path(&format!("retained-neg-result-{name}.rs"));
        fs::write(&path, source).unwrap();
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for target in [CompileTarget::Jit, CompileTarget::Native] {
                let artifact = temp_path("retained-neg-result-native");
                let mut request = default_request(path.clone(), target, Some("main"), None);
                request.profile = profile;
                if target == CompileTarget::Native {
                    request.out = Some(artifact.clone());
                }
                let report = compile_owned(&request);
                assert!(report.success, "{name} {profile:?} {target:?}: {report:?}");
                assert!(report.degradations.is_empty(), "{report:?}");
                if target == CompileTarget::Jit {
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
                } else {
                    assert_eq!(
                        std::process::Command::new(&artifact)
                            .status()
                            .unwrap()
                            .code(),
                        Some(42)
                    );
                    fs::remove_file(&artifact).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_nested_function_aliases_preserve_lexical_callable_identity() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let cases = [
        (
            "bool-function-address",
            "static mut CALLS:i64=0;fn flag()->bool{unsafe{CALLS+=1;}true}fn main()->i64{let address=flag as usize;if address!=0 && unsafe{CALLS==0}{42}else{7}}",
        ),
        (
            "float-function-address",
            "static mut CALLS:i64=0;fn number()->f64{unsafe{CALLS+=1;}7.0}fn main()->i64{let address=number as usize;if address!=0 && unsafe{CALLS==0}{42}else{7}}",
        ),
        (
            "record-function-address",
            "static mut CALLS:i64=0;struct Record{value:i64}fn record()->Record{unsafe{CALLS+=1;}Record{value:7}}fn main()->i64{let address=record as usize;if address!=0 && unsafe{CALLS==0}{42}else{7}}",
        ),
        (
            "nested-alias",
            "fn answer()->i64{7}mod outer{mod inner{pub fn answer()->i64{42}}pub use self::inner::answer as pick;pub fn run()->i64{pick()}}fn main()->i64{outer::run()}",
        ),
        (
            "alias-chain",
            "fn answer()->i64{7}mod outer{mod inner{pub fn answer()->i64{42}}pub use self::inner::answer as first;pub use self::first as pick;pub fn run()->i64{pick()}}fn main()->i64{outer::run()}",
        ),
        (
            "sibling-helper",
            "fn helper()->i64{7}mod outer{fn helper()->i64{42}pub fn run()->i64{helper()}}fn main()->i64{outer::run()}",
        ),
        (
            "function-value",
            "fn helper()->i64{7}mod outer{fn helper()->i64{42}pub fn run()->i64{let pick=helper;pick()}}fn main()->i64{outer::run()}",
        ),
        (
            "function-argument",
            "fn helper()->i64{7}mod outer{fn helper()->i64{42}fn apply(pick:fn()->i64)->i64{pick()}pub fn run()->i64{apply(helper)}}fn main()->i64{outer::run()}",
        ),
        (
            "parameter-shadow",
            "fn selected()->i64{42}mod outer{fn helper()->i64{7}pub fn run(helper:fn()->i64)->i64{helper()}}fn main()->i64{outer::run(selected)}",
        ),
        (
            "self-ignores-local",
            "fn selected()->i64{7}mod outer{fn helper()->i64{42}pub fn run(helper:fn()->i64)->i64{self::helper()}}fn main()->i64{outer::run(selected)}",
        ),
        (
            "closure-capture-shadow",
            "fn selected()->i64{42}mod outer{fn helper()->i64{7}pub fn run(helper:fn()->i64)->i64{let pick=||helper();pick()}}fn main()->i64{outer::run(selected)}",
        ),
    ];
    for (name, source) in cases {
        let path = temp_path(&format!("rust-nested-function-{name}.rs"));
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path(&format!("rust-nested-function-{name}-out"));
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(report.success, "{name} {target:?} {profile:?}: {report:?}");
                assert!(report.external_invocations.is_empty(), "{report:?}");
                assert!(report.degradations.is_empty(), "{report:?}");
                if target == CompileTarget::Jit {
                    assert!(report.execution_started, "{report:?}");
                    assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{report:?}");
                } else {
                    assert!(out.is_file(), "{report:?}");
                    assert_eq!(
                        std::process::Command::new(&out).status().unwrap().code(),
                        Some(42),
                        "{name} {profile:?}"
                    );
                    fs::remove_file(out).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_nested_function_qualification_preserves_privacy_and_conditional_refusal() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (name, source) in [
        (
            "private-function",
            "mod hidden{fn answer()->i64{42}}fn main()->i64{hidden::answer()}",
        ),
        (
            "conditional-function",
            "mod outer{#[cfg(feature=\"optional\")]pub fn answer()->i64{42}pub fn run()->i64{answer()}}fn main()->i64{outer::run()}",
        ),
    ] {
        let path = temp_path(&format!("rust-nested-function-{name}.rs"));
        fs::write(&path, source).unwrap();
        for target in [CompileTarget::Jit, CompileTarget::Native] {
            for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                let out = temp_path(&format!("rust-nested-function-{name}-out"));
                let mut request =
                    default_request(path.clone(), target, Some("main"), Some(out.clone()));
                request.profile = profile;
                let report = compile_owned(&request);
                assert!(!report.success, "{name} {target:?} {profile:?}: {report:?}");
                assert!(!report.execution_started, "{report:?}");
                assert_eq!(report.eval_result, None);
                assert_eq!(report.eval_exit_code, None);
                assert_eq!(report.artifact_path, None);
                assert_eq!(report.executable_path, None);
                assert_eq!(report.abi_path, None);
                assert!(report.external_invocations.is_empty(), "{report:?}");
                if name == "private-function" {
                    assert_eq!(
                        report.reason_code.as_deref(),
                        Some("rust-invalid-associated-type"),
                        "{report:?}"
                    );
                } else {
                    let reason = report.reason.as_deref().unwrap_or("");
                    assert!(reason.contains("cfg predicates"), "{report:?}");
                    if target == CompileTarget::Native {
                        assert_eq!(
                            report.reason_code.as_deref(),
                            Some("native-lowering-failed"),
                            "{report:?}"
                        );
                        assert!(
                            reason.starts_with(
                                "unsupported Rust associated type in native callable `"
                            ),
                            "{report:?}"
                        );
                    } else {
                        assert_eq!(
                            report.reason_code.as_deref(),
                            Some("jit-failed"),
                            "{report:?}"
                        );
                        assert!(
                            reason.starts_with("jit-lowering-failed: source-type-proof:"),
                            "{report:?}"
                        );
                    }
                }
                assert!(!out.exists(), "{report:?}");
            }
        }
        fs::remove_file(path).unwrap();
    }
}
