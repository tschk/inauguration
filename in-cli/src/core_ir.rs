//! Cross-frontend core AST (v0). Bodies may be empty until a frontend fills statements.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;

thread_local! {
    static INTERRUPT_FNS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

pub fn register_interrupt_fn(name: &str) {
    INTERRUPT_FNS.with(|fns| fns.borrow_mut().insert(name.to_string()));
}

pub fn is_interrupt_fn(name: &str) -> bool {
    INTERRUPT_FNS.with(|fns| fns.borrow().contains(name))
}

/// Source position span.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub file: String,
}

impl Span {
    #[must_use]
    pub fn new(line: u32, col: u32, file: &str) -> Self {
        Self {
            line,
            col,
            file: file.to_string(),
        }
    }
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            line: 0,
            col: 0,
            file: String::new(),
        }
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file.is_empty() {
            write!(f, "{}:{}", self.line, self.col)
        } else {
            write!(f, "{}:{}:{}", self.file, self.line, self.col)
        }
    }
}

/// Source position attached to a Core IR node.
pub type NodeSpan = Option<Span>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FloatVal(pub f64);

impl PartialEq for FloatVal {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for FloatVal {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Typ {
    Int,
    String,
    Bool,
    Float,
    Void,
    Array(Box<Typ>),
    Vector(Box<Typ>),
    Named(String),
    Generic(String),
}

impl Typ {
    /// Normalize language-specific named types to canonical Core IR primitives.
    pub fn canonical(&self) -> Typ {
        match self {
            Typ::Named(name) => match name.trim() {
                "Int" | "int" | "i64" | "i32" | "i16" | "i8" | "u64" | "u32" | "u16" | "u8" => {
                    Typ::Int
                }
                "String" | "string" | "str" => Typ::String,
                "Bool" | "bool" => Typ::Bool,
                "Float" | "float" | "Double" | "double" | "f64" | "f32" => Typ::Float,
                "Void" | "void" | "Unit" | "unit" | "()" => Typ::Void,
                _ => self.clone(),
            },
            Typ::Array(item) => Typ::Array(Box::new(item.canonical())),
            Typ::Vector(item) => Typ::Vector(Box::new(item.canonical())),
            _ => self.clone(),
        }
    }

    pub fn is_any(&self) -> bool {
        matches!(self, Typ::Named(name) if name == "Any")
    }

    /// Two types are compatible if canonical forms are equal or either is Any.
    pub fn compatible_with(&self, other: &Typ) -> bool {
        let a = self.canonical();
        let b = other.canonical();
        a == b || a.is_any() || b.is_any()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    IntLit(i64),
    FloatLit(FloatVal),
    StringLit(String),
    BoolLit(bool),
    Ident(String),
    Unary {
        op: String,
        expr: Box<Expr>,
    },
    Binary {
        op: String,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    StructInit {
        name: String,
        fields: Vec<(String, Expr)>,
    },
    Field {
        base: Box<Expr>,
        name: String,
    },
    ArrayLit(Vec<Expr>),
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Closure {
        params: Vec<(String, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
        captures: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Let(String, Option<Typ>, Expr),
    Assign(String, Expr),
    IndexAssign {
        base: Expr,
        index: Expr,
        value: Expr,
    },
    /// Assign to a struct field: `s.x = val`.
    FieldAssign {
        base: Expr,
        name: String,
        value: Expr,
    },
    Return(Option<Expr>),
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    Loop {
        kind: LoopKind,
        cond: Option<Expr>,
        body: Vec<Stmt>,
    },
    Match {
        scrutinee: Expr,
        arms: Vec<MatchArm>,
    },
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        catches: Vec<CatchArm>,
    },
    /// Return from the current function when the runtime error flag is set.
    Propagate,
    /// Evaluated for side effects (e.g. `.in` expression statements).
    Expr(Expr),
    /// Break out of the current loop.
    Break,
    /// Continue the current loop.
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopKind {
    For { binding: String },
    While,
    Infinite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPattern {
    IntPat(i64),
    StringPat(String),
    BoolPat(bool),
    WildPat,
    IdentPat(String),
    RestPat,
    TuplePat(Vec<MatchPattern>),
    StructPat {
        name: String,
        fields: Vec<(String, MatchPattern)>,
    },
    ArrayPat(Vec<MatchPattern>),
}

fn trim_match_pat(s: &str) -> &str {
    s.trim()
}

fn split_match_pat_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    for (i, c) in inner.char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                let arg = trim_match_pat(&inner[start..i]);
                if !arg.is_empty() {
                    out.push(arg.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = trim_match_pat(&inner[start..]);
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

impl MatchPattern {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = trim_match_pat(s).trim_end_matches(':').trim();
        let s = s.strip_prefix("case ").unwrap_or(s).trim();
        if s.is_empty() {
            return Err(".in: empty pattern".into());
        }
        if s == "_" || s == "-" || s == "else" || s == "default" {
            return Ok(MatchPattern::WildPat);
        }
        if s == ".." {
            return Ok(MatchPattern::RestPat);
        }
        if s == "true" {
            return Ok(MatchPattern::BoolPat(true));
        }
        if s == "false" {
            return Ok(MatchPattern::BoolPat(false));
        }
        if let Ok(n) = s.parse::<i64>() {
            return Ok(MatchPattern::IntPat(n));
        }
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            return Ok(MatchPattern::StringPat(s[1..s.len() - 1].to_string()));
        }
        if s.starts_with('(') && s.ends_with(')') {
            let inner = &s[1..s.len() - 1];
            let parts = split_match_pat_args(inner);
            let pats: Result<Vec<_>, _> = parts.iter().map(|p| MatchPattern::parse(p)).collect();
            return Ok(MatchPattern::TuplePat(pats?));
        }
        if s.starts_with('[') && s.ends_with(']') {
            let inner = &s[1..s.len() - 1];
            let parts = split_match_pat_args(inner);
            let pats: Result<Vec<_>, _> = parts.iter().map(|p| MatchPattern::parse(p)).collect();
            return Ok(MatchPattern::ArrayPat(pats?));
        }
        if let Some(open) = s.find('{')
            && s.ends_with('}')
        {
            let name = trim_match_pat(&s[..open]);
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                let inner = &s[open + 1..s.len() - 1];
                let field_strs = split_match_pat_args(inner);
                let mut fields = Vec::new();
                for f in field_strs {
                    if let Some((field_name, field_pat)) = f.split_once(':') {
                        let fn_trim = trim_match_pat(field_name);
                        let fp_trim = trim_match_pat(field_pat);
                        if fn_trim.is_empty() {
                            return Err(format!(".in: empty field name in struct pattern `{s}`"));
                        }
                        fields.push((fn_trim.to_string(), MatchPattern::parse(fp_trim)?));
                    } else {
                        let fn_trim = trim_match_pat(&f);
                        if fn_trim.is_empty() {
                            return Err(format!(".in: empty field name in struct pattern `{s}`"));
                        }
                        fields.push((
                            fn_trim.to_string(),
                            MatchPattern::IdentPat(fn_trim.to_string()),
                        ));
                    }
                }
                return Ok(MatchPattern::StructPat {
                    name: name.to_string(),
                    fields,
                });
            }
        }
        // Wildcard discard keeps a lone `_`; all other names are kebab-case.
        if s == "_"
            || (!s.is_empty()
                && s.chars().enumerate().all(|(i, c)| match (i, c) {
                    (0, ch) => ch.is_ascii_alphabetic(),
                    (_, ch) => ch.is_ascii_alphanumeric() || ch == '-',
                }))
        {
            return Ok(MatchPattern::IdentPat(s.to_string()));
        }
        Err(format!(".in: unknown pattern `{s}`"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: String,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchArm {
    pub pattern: String,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Pub,
    Private,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<(String, Typ)>,
    pub ret: Typ,
}

/// Single-module view produced by language fronts before lowering to textual SIL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedModule {
    pub identity: CoreModuleIdentity,
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreModuleIdentity {
    pub package: Option<String>,
    pub module: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleIdentityReport {
    pub package: Option<String>,
    pub module: Option<String>,
    pub requested_module_id: String,
    pub effective_module_id: String,
}

impl UnifiedModule {
    #[must_use]
    pub fn new(decls: Vec<Decl>) -> Self {
        Self {
            identity: CoreModuleIdentity::default(),
            decls,
        }
    }

    #[must_use]
    pub fn with_identity(decls: Vec<Decl>, identity: CoreModuleIdentity) -> Self {
        Self { identity, decls }
    }

    #[must_use]
    pub fn effective_module_id<'a>(&'a self, requested: &'a str) -> &'a str {
        if requested != "App" {
            return requested;
        }
        self.identity
            .module
            .as_deref()
            .or(self.identity.package.as_deref())
            .unwrap_or(requested)
    }

    #[must_use]
    pub fn identity_report(&self, requested: &str) -> ModuleIdentityReport {
        ModuleIdentityReport {
            package: self.identity.package.clone(),
            module: self.identity.module.clone(),
            requested_module_id: requested.to_string(),
            effective_module_id: self.effective_module_id(requested).to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentImport {
    pub name: String,
    pub interface: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentExport {
    pub name: String,
    pub interface: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentCapability {
    pub name: String,
    pub capability_type: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decl {
    Struct {
        name: String,
        fields: Vec<(String, Typ)>,
        type_params: Vec<String>,
    },
    Function {
        name: String,
        params: Vec<(String, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
        type_params: Vec<String>,
    },
    Class {
        name: String,
        fields: Vec<(String, Typ)>,
        methods: Vec<Decl>,
        visibility: Visibility,
        extends: Option<String>,
        implements: Vec<String>,
        type_params: Vec<String>,
    },
    Interface {
        name: String,
        methods: Vec<MethodSig>,
        visibility: Visibility,
        type_params: Vec<String>,
    },
    Component {
        name: String,
        target: String,
        deterministic: bool,
        checkpoint: String,
        imports: Vec<ComponentImport>,
        exports: Vec<ComponentExport>,
        capabilities: Vec<ComponentCapability>,
    },
    /// A global variable or constant declaration.
    /// `mutable` is true for `var`, false for `const`.
    Global {
        name: String,
        typ: Typ,
        init: Option<Box<Expr>>,
        mutable: bool,
    },
}

/// Visit every immediate child expression of `e`, calling `f` for each.
/// Useful for building recursive AST walkers without duplicating the variant match.
pub fn for_each_expr_child(e: &Expr, f: &mut impl FnMut(&Expr)) {
    match e {
        Expr::Call { callee, args, .. } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Expr::Unary { expr, .. } => f(expr),
        Expr::Field { base, .. } => f(base),
        Expr::Index { base, index, .. } => {
            f(base);
            f(index);
        }
        Expr::StructInit { fields, .. } => {
            for (_, fe) in fields {
                f(fe);
            }
        }
        Expr::ArrayLit(items) => {
            for i in items {
                f(i);
            }
        }
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_)
        | Expr::Closure { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Span ───────────────────────────────────────────────────────────

    #[test]
    fn span_new() {
        let s = Span::new(10, 5, "test.in");
        assert_eq!(s.line, 10);
        assert_eq!(s.col, 5);
        assert_eq!(s.file, "test.in");
    }

    #[test]
    fn span_unknown() {
        let s = Span::unknown();
        assert_eq!(s.line, 0);
        assert_eq!(s.col, 0);
        assert!(s.file.is_empty());
    }

    #[test]
    fn span_display_with_file() {
        let s = Span::new(10, 5, "test.in");
        assert_eq!(format!("{s}"), "test.in:10:5");
    }

    #[test]
    fn span_display_without_file() {
        let s = Span::new(10, 5, "");
        assert_eq!(format!("{s}"), "10:5");
    }

    #[test]
    fn span_default() {
        let s = Span::default();
        assert_eq!(s.line, 0);
        assert_eq!(s.col, 0);
        assert!(s.file.is_empty());
    }

    // ─── FloatVal ──────────────────────────────────────────────────────

    #[test]
    fn float_val_equality() {
        assert_eq!(FloatVal(1.0), FloatVal(1.0));
        assert_ne!(FloatVal(1.0), FloatVal(2.0));
    }

    #[test]
    fn float_val_nan_equality() {
        let nan1 = FloatVal(f64::NAN);
        let nan2 = FloatVal(f64::NAN);
        assert_eq!(nan1, nan2);
    }

    // ─── Typ ───────────────────────────────────────────────────────────

    #[test]
    fn typ_array_nesting() {
        let t = Typ::Array(Box::new(Typ::Int));
        assert_eq!(t, Typ::Array(Box::new(Typ::Int)));
    }

    #[test]
    fn typ_named() {
        let t = Typ::Named("MyStruct".to_string());
        if let Typ::Named(n) = &t {
            assert_eq!(n, "MyStruct");
        } else {
            panic!("expected Named");
        }
    }

    #[test]
    fn typ_generic() {
        let t = Typ::Generic("T".to_string());
        if let Typ::Generic(n) = &t {
            assert_eq!(n, "T");
        } else {
            panic!("expected Generic");
        }
    }

    // ─── MatchPattern ──────────────────────────────────────────────────

    #[test]
    fn match_pattern_wild() {
        assert_eq!(MatchPattern::parse("_").unwrap(), MatchPattern::WildPat);
        assert_eq!(MatchPattern::parse("-").unwrap(), MatchPattern::WildPat);
        assert_eq!(MatchPattern::parse("else").unwrap(), MatchPattern::WildPat);
        assert_eq!(
            MatchPattern::parse("default").unwrap(),
            MatchPattern::WildPat
        );
    }

    #[test]
    fn match_pattern_rest() {
        assert_eq!(MatchPattern::parse("..").unwrap(), MatchPattern::RestPat);
    }

    #[test]
    fn match_pattern_bool() {
        assert_eq!(
            MatchPattern::parse("true").unwrap(),
            MatchPattern::BoolPat(true)
        );
        assert_eq!(
            MatchPattern::parse("false").unwrap(),
            MatchPattern::BoolPat(false)
        );
    }

    #[test]
    fn match_pattern_int() {
        assert_eq!(MatchPattern::parse("42").unwrap(), MatchPattern::IntPat(42));
        assert_eq!(MatchPattern::parse("-1").unwrap(), MatchPattern::IntPat(-1));
    }

    #[test]
    fn match_pattern_string() {
        assert_eq!(
            MatchPattern::parse("\"hello\"").unwrap(),
            MatchPattern::StringPat("hello".to_string())
        );
    }

    #[test]
    fn match_pattern_ident() {
        assert_eq!(
            MatchPattern::parse("x").unwrap(),
            MatchPattern::IdentPat("x".to_string())
        );
    }

    #[test]
    fn match_pattern_tuple() {
        let pat = MatchPattern::parse("(1, 2)").unwrap();
        assert_eq!(
            pat,
            MatchPattern::TuplePat(vec![MatchPattern::IntPat(1), MatchPattern::IntPat(2)])
        );
    }

    #[test]
    fn match_pattern_array() {
        let pat = MatchPattern::parse("[1, 2]").unwrap();
        assert_eq!(
            pat,
            MatchPattern::ArrayPat(vec![MatchPattern::IntPat(1), MatchPattern::IntPat(2)])
        );
    }

    #[test]
    fn match_pattern_struct() {
        let pat = MatchPattern::parse("Point{x: 1, y: 2}").unwrap();
        if let MatchPattern::StructPat { name, fields } = pat {
            assert_eq!(name, "Point");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0], ("x".to_string(), MatchPattern::IntPat(1)));
            assert_eq!(fields[1], ("y".to_string(), MatchPattern::IntPat(2)));
        } else {
            panic!("expected StructPat");
        }
    }

    #[test]
    fn match_pattern_struct_shorthand() {
        let pat = MatchPattern::parse("Point{x, y}").unwrap();
        if let MatchPattern::StructPat { name, fields } = pat {
            assert_eq!(name, "Point");
            assert_eq!(
                fields[0],
                ("x".to_string(), MatchPattern::IdentPat("x".to_string()))
            );
        } else {
            panic!("expected StructPat");
        }
    }

    #[test]
    fn match_pattern_case_prefix() {
        assert_eq!(
            MatchPattern::parse("case 42").unwrap(),
            MatchPattern::IntPat(42)
        );
    }

    #[test]
    fn match_pattern_trailing_colon() {
        assert_eq!(
            MatchPattern::parse("42:").unwrap(),
            MatchPattern::IntPat(42)
        );
    }

    #[test]
    fn match_pattern_empty_error() {
        assert!(MatchPattern::parse("").is_err());
    }

    // ─── split_match_pat_args ──────────────────────────────────────────

    #[test]
    fn split_simple_args() {
        let args = split_match_pat_args("1, 2, 3");
        assert_eq!(args, vec!["1", "2", "3"]);
    }

    #[test]
    fn split_nested_args() {
        let args = split_match_pat_args("(1, 2), 3");
        assert_eq!(args, vec!["(1, 2)", "3"]);
    }

    #[test]
    fn split_empty() {
        let args = split_match_pat_args("");
        assert!(args.is_empty());
    }

    // ─── UnifiedModule ──────────────────────────────────────────────────

    #[test]
    fn unified_module_new() {
        let m = UnifiedModule::new(vec![]);
        assert!(m.decls.is_empty());
        assert_eq!(m.identity, CoreModuleIdentity::default());
    }

    #[test]
    fn unified_module_with_identity() {
        let id = CoreModuleIdentity {
            package: Some("my_pkg".to_string()),
            module: Some("my_mod".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id.clone());
        assert_eq!(m.identity, id);
    }

    #[test]
    fn effective_module_id_uses_requested_when_not_app() {
        let m = UnifiedModule::new(vec![]);
        assert_eq!(m.effective_module_id("Custom"), "Custom");
    }

    #[test]
    fn effective_module_id_falls_back_to_module() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: Some("mod_name".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id);
        assert_eq!(m.effective_module_id("App"), "mod_name");
    }

    #[test]
    fn effective_module_id_falls_back_to_package() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: None,
        };
        let m = UnifiedModule::with_identity(vec![], id);
        assert_eq!(m.effective_module_id("App"), "pkg");
    }

    #[test]
    fn effective_module_id_falls_back_to_app() {
        let m = UnifiedModule::new(vec![]);
        assert_eq!(m.effective_module_id("App"), "App");
    }

    #[test]
    fn identity_report() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: Some("mod_name".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id);
        let report = m.identity_report("App");
        assert_eq!(report.requested_module_id, "App");
        assert_eq!(report.effective_module_id, "mod_name");
        assert_eq!(report.package, Some("pkg".to_string()));
    }

    // ─── Interrupt FNs ──────────────────────────────────────────────────

    #[test]
    fn interrupt_fn_registration() {
        register_interrupt_fn("my_isr");
        assert!(is_interrupt_fn("my_isr"));
        assert!(!is_interrupt_fn("not_registered"));
    }

    // ─── Visibility / Import / MethodSig ───────────────────────────────

    #[test]
    fn visibility_variants() {
        let v = Visibility::Pub;
        assert_eq!(v, Visibility::Pub);
        assert_ne!(v, Visibility::Private);
        assert_ne!(v, Visibility::Internal);
    }

    #[test]
    fn import_with_alias() {
        let imp = Import {
            path: "std.io".to_string(),
            alias: Some("io".to_string()),
        };
        assert_eq!(imp.path, "std.io");
        assert_eq!(imp.alias.unwrap(), "io");
    }

    #[test]
    fn method_sig_round_trip() {
        let sig = MethodSig {
            name: "foo".to_string(),
            params: vec![("x".to_string(), Typ::Int)],
            ret: Typ::Void,
        };
        assert_eq!(sig.name, "foo");
        assert_eq!(sig.params.len(), 1);
    }
}
