//! Polyglot Core IR fronts using **Tree-sitter**: full grammar-backed parses → [`UnifiedModule`]
//! with bounded declaration and body extraction where each language extractor is wired.

mod c_family;
mod crystal;
#[cfg(feature = "parse-extended")]
mod csharp;
#[cfg(feature = "parse-extended")]
mod dart;
#[cfg(feature = "parse-extended")]
mod elixir;
#[cfg(feature = "parse-extended")]
mod erlang;
mod extract;
#[cfg(feature = "parse-extended")]
mod fsharp;
mod go;
#[cfg(feature = "parse-extended")]
mod haskell;
mod holyc;
mod java;
mod js;
#[cfg(feature = "parse-extended")]
mod julia;
#[cfg(feature = "parse-extended")]
mod kotlin;
mod lolcat;
mod lua;
mod nim;
#[cfg(feature = "parse-extended")]
mod ocaml;
mod perl;
#[cfg(feature = "parse-extended")]
mod php;
mod python;
#[cfg(feature = "parse-extended")]
mod r_lang;
mod ruby;
mod rust;
#[cfg(feature = "parse-extended")]
mod scala;
mod swift;
mod ts;
#[cfg(feature = "parse-extended")]
mod v_lang;
mod zig;

pub use extract::{parse_polyglot_file, parse_zig_artifact, parse_zig_artifact_source};
