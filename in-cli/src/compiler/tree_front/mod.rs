//! Polyglot Core IR fronts using **Tree-sitter**: full grammar-backed parses → [`UnifiedModule`]
//! with bounded declaration and body extraction where each language extractor is wired.

mod c_family;
pub(crate) use c_family::c_typ_from_decl_specifier_text as normalize_c_type;
mod crystal;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod csharp;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod dart;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod elixir;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod erlang;
mod extract;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod fsharp;
mod go;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod haskell;
mod holyc;
mod java;
mod js;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod julia;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod kotlin;
mod lolcat;
mod lua;
mod nim;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod ocaml;
mod perl;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod php;
mod python;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod r_lang;
mod ruby;
mod rust;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod scala;
mod swift;
mod ts;
#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
mod v_lang;
mod zig;

pub use extract::{parse_polyglot_file, parse_zig_artifact, parse_zig_artifact_source};
