//! Core IR → C code emitter.
//!
//! Vlang-style C backend: emit portable C99, let GCC/Clang handle optimization.
//! Compile with: zig cc -c -target x86_64-freestanding -ffreestanding -O2 kernel.c

use crate::core_ir::{Decl, Expr, LoopKind, Stmt, Typ, UnifiedModule};
use crate::typecheck::{Facts, TypeChecker};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[path = "c_backend_scan.rs"]
mod scan;
use scan::{check_module_limits, module_needs_invec};

fn ctype(typ: &Typ) -> String {
    match typ.canonical() {
        Typ::Int => "int64_t".into(),
        Typ::Bool => "uint8_t".into(),
        Typ::Float => "double".into(),
        Typ::String => "const char*".into(),
        Typ::Void => "void".into(),
        Typ::Array(inner) => format!("{}*", ctype(&inner)),
        // Matches native lower: Vec = { ptr, len, cap } (8-byte fields).
        Typ::Vector(_) => "struct InVec".into(),
        Typ::Named(name) if name == "Vec" || name == "InVec" => "struct InVec".into(),
        Typ::Named(name) => format!("struct {}", sanitize_ident(&name)),
        Typ::Generic(_) => "int64_t".into(),
    }
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn escape_c_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn sanitize_comment(s: &str) -> String {
    s.replace("*/", "* /").replace("/*", "/ *")
}

/// Emit a complete C translation unit from Core IR.
pub fn emit_c_module(module: &UnifiedModule) -> Result<String, String> {
    let mut prepared = crate::lower_core::prepared_native_module(module);
    check_module_limits(prepared.as_ref())?;
    // Hoisted declarations require unique lexical names, even on the borrowed fast path.
    crate::core_bindings::normalize_decls(&mut prepared.to_mut().decls);
    let module = prepared.as_ref();
    crate::core_ir_verifier::check_nonvoid_completion(module)?;

    let mut names = HashSet::new();
    for decl in &module.decls {
        match decl {
            Decl::Function { name, .. } => {
                names.insert(name.clone());
            }
            Decl::Class { methods, .. } => {
                for m in methods {
                    if let Decl::Function { name, .. } = m {
                        names.insert(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let checker = TypeChecker::new();
    let facts = Rc::new(checker.collect_decl_facts(&module.decls));
    let mut ctx = Ctx::new(Rc::new(names), facts);
    ctx.throwing = Rc::new(crate::core_effects::fallible_functions(
        module.decls.iter().filter_map(|decl| {
            if let Decl::Function {
                name,
                params,
                ret,
                body,
                ..
            } = decl
            {
                Some((name.as_str(), body.as_slice(), ret, params.as_slice()))
            } else {
                None
            }
        }),
    ));
    ctx.types = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    fn reserve(decls: &[Decl], names: &mut HashSet<String>) {
        for decl in decls {
            match decl {
                Decl::Function {
                    name, params, body, ..
                } => {
                    names.insert(name.clone());
                    names.extend(params.iter().map(|(name, _)| name.clone()));
                    crate::core_bindings::reserve_body(body, names);
                }
                Decl::Global { name, init, .. } => {
                    names.insert(name.clone());
                    if let Some(init) = init {
                        crate::core_bindings::reserve_expr(init, names);
                    }
                }
                Decl::Class { methods, .. } => reserve(methods, names),
                _ => {}
            }
        }
    }
    let mut reserved = HashSet::new();
    reserve(&module.decls, &mut reserved);
    ctx.reserved
        .borrow_mut()
        .extend(reserved.iter().map(|name| sanitize_ident(name)));
    ctx.global_names = Rc::new(
        module
            .decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Global { name, .. } => Some(sanitize_ident(name)),
                _ => None,
            })
            .collect(),
    );

    ctx.line("/* Generated by Inauguration C backend */");
    ctx.line("#include <stdint.h>");
    ctx.line("#include <stddef.h>");
    ctx.line(
        r#"
static inline int64_t in_float_saturate(double value, unsigned bits, int is_signed) {
    if (value != value) return 0;
    if (is_signed) {
        int64_t minimum = bits == 64 ? INT64_MIN : -(INT64_C(1) << (bits - 1));
        int64_t maximum = bits == 64 ? INT64_MAX : (INT64_C(1) << (bits - 1)) - 1;
        if (value <= (double)minimum) return minimum;
        if (value >= (double)maximum) return maximum;
        return (int64_t)value;
    }
    uint64_t maximum = bits == 64 ? UINT64_MAX : (UINT64_C(1) << bits) - 1;
    uint64_t raw = value <= 0.0 ? 0 : value >= (double)maximum ? maximum : (uint64_t)value;
    union { uint64_t bits; int64_t word; } result = { .bits = raw };
    return result.word;
}
"#,
    );

    ctx.line("");
    if module_needs_invec(module) {
        // Shared with native lower (ptr/len/cap, 8-byte fields).
        ctx.line("typedef struct InVec {");
        ctx.line("  int64_t *ptr;");
        ctx.line("  int64_t len;");
        ctx.line("  int64_t cap;");
        ctx.line("} InVec;");
        ctx.line("");
    }

    if let Some(pkg) = &module.identity.package {
        ctx.push(&format!("/* package: {} */", sanitize_comment(pkg)));
    }
    if let Some(mod_name) = &module.identity.module {
        ctx.push(&format!("/* module: {} */", sanitize_comment(mod_name)));
    }
    if module.identity.package.is_some() || module.identity.module.is_some() {
        ctx.line("");
    }

    for decl in &module.decls {
        if let Decl::Struct { name, fields, .. } = decl {
            ctx.push(&format!("struct {} {{", sanitize_ident(name)));
            ctx.indent();
            if fields.is_empty() {
                ctx.push("char _pad;");
            } else {
                for (fname, ftype) in fields {
                    ctx.push(&format!("{} {};", ctype(ftype), sanitize_ident(fname)));
                }
            }
            ctx.dedent();
            ctx.line("};");
            ctx.line("");
        }
    }

    // Classes lower as structs + free methods (self first param).
    for decl in &module.decls {
        if let Decl::Class {
            name,
            fields,
            methods,
            ..
        } = decl
        {
            ctx.push(&format!("struct {} {{", sanitize_ident(name)));
            ctx.indent();
            if fields.is_empty() {
                ctx.push("char _pad;");
            } else {
                for (fname, ftype) in fields {
                    ctx.push(&format!("{} {};", ctype(ftype), sanitize_ident(fname)));
                }
            }
            ctx.dedent();
            ctx.line("};");
            ctx.line("");
            for m in methods {
                if let Decl::Function {
                    name: mname,
                    params,
                    ret,
                    body,
                    ..
                } = m
                {
                    let mut full_params = vec![("self".to_string(), Typ::Named(name.clone()))];
                    full_params.extend(params.iter().cloned());
                    emit_fn_def(
                        &mut ctx,
                        &format!("{}_{}", sanitize_ident(name), sanitize_ident(mname)),
                        &full_params,
                        ret,
                        body,
                    );
                }
            }
        }
    }

    for decl in &module.decls {
        if let Decl::Interface { name, methods, .. } = decl {
            ctx.push(&format!("/* interface {} */", sanitize_ident(name)));
            for m in methods {
                let ps: Vec<String> = m
                    .params
                    .iter()
                    .map(|(n, t)| format!("{} {}", ctype(t), sanitize_ident(n)))
                    .collect();
                ctx.push(&format!(
                    "/* {} {}({}); */",
                    ctype(&m.ret),
                    sanitize_ident(&m.name),
                    ps.join(", ")
                ));
            }
            ctx.line("");
        }
    }

    let has_globals = module
        .decls
        .iter()
        .any(|d| matches!(d, Decl::Global { .. }));
    if has_globals {
        for decl in &module.decls {
            if let Decl::Global {
                name,
                typ,
                init,
                constant_value: false,
                ..
            } = decl
            {
                let init_s = match init {
                    Some(e) => {
                        ctx.global_initializer = true;
                        let value = crate::core_opt::map_expr(
                            e.as_ref().clone(),
                            &mut crate::core_opt::fold_expr,
                        );
                        let value = ctx.expr_str(&value);
                        ctx.global_initializer = false;
                        format!(" = {}", value)
                    }
                    None => String::new(),
                };
                ctx.push(&format!(
                    "volatile {} {}{};",
                    ctype(typ),
                    sanitize_ident(name),
                    init_s
                ));
            }
        }
        ctx.line("");
    }

    for decl in &module.decls {
        if let Decl::Function {
            name, params, ret, ..
        } = decl
        {
            let ps: Vec<String> = params
                .iter()
                .map(|(n, t)| format!("{} {}", ctype(t), sanitize_ident(n)))
                .collect();
            ctx.push(&format!(
                "{} {}({});",
                ctype(ret),
                sanitize_ident(name),
                ps.join(", ")
            ));
        }
    }
    if module
        .decls
        .iter()
        .any(|d| matches!(d, Decl::Function { .. }))
    {
        ctx.line("");
    }

    for decl in &module.decls {
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            emit_fn_def(&mut ctx, name, params, ret, body);
        }
    }

    for decl in &module.decls {
        if let Decl::Component {
            name,
            target,
            deterministic,
            ..
        } = decl
        {
            ctx.push(&format!(
                "/* component: {} target:{} deterministic:{} */",
                name, target, deterministic
            ));
        }
    }

    if let Some(error) = ctx.error.borrow_mut().take() {
        return Err(error);
    }
    Ok(ctx.out)
}

fn word_failure_return(ret: &Typ) -> Option<&'static str> {
    match ret.canonical() {
        Typ::Int | Typ::Bool => Some("return 1;"),
        Typ::Void => Some("return;"),
        _ => None,
    }
}

struct Ctx {
    out: String,
    facts: Rc<Facts>,
    throwing: Rc<HashSet<String>>,
    reserved: Rc<RefCell<HashSet<String>>>,
    types: HashMap<String, Typ>,
    error: Rc<RefCell<Option<String>>>,
    indent_lvl: u32,
    func_names: Rc<HashSet<String>>,
    global_names: Rc<HashSet<String>>,
    /// Hoisted closures emitted before the current function.
    hoisted: Vec<String>,
    tmp: u32,
    ret_is_void: bool,
    word_failure_return: Option<&'static str>,
    global_initializer: bool,
}

impl Ctx {
    fn new(func_names: Rc<HashSet<String>>, facts: Rc<Facts>) -> Self {
        Self {
            out: String::with_capacity(4096),
            facts,
            throwing: Rc::new(HashSet::new()),
            reserved: Rc::new(RefCell::new(HashSet::new())),
            types: HashMap::new(),
            error: Rc::new(RefCell::new(None)),
            indent_lvl: 0,
            func_names,
            global_names: Rc::new(HashSet::new()),
            hoisted: Vec::new(),
            tmp: 0,
            ret_is_void: false,
            word_failure_return: None,
            global_initializer: false,
        }
    }

    fn fail(&self, message: String) {
        let mut error = self.error.borrow_mut();
        if error.is_none() {
            *error = Some(message);
        }
    }

    fn capture_type(&self, name: &str) -> Typ {
        self.types.get(name).cloned().unwrap_or_else(|| {
            self.fail(format!("c-backend: cannot infer capture `{name}`"));
            Typ::Int
        })
    }

    fn fresh(&mut self, prefix: &str) -> String {
        loop {
            let id = self.tmp;
            self.tmp += 1;
            let candidate = format!("__{}_{}", prefix, id);
            if self.reserved.borrow_mut().insert(candidate.clone()) {
                return candidate;
            }
        }
    }

    fn string_equality(&mut self, left: &str, right: &str, unequal: bool) -> String {
        if self.global_initializer {
            self.fail(
                "c-backend: String equality in a global initializer requires constant folding"
                    .into(),
            );
            return "0".into();
        }
        let a = self.fresh("str_left");
        let b = self.fresh("str_right");
        let equal = self.fresh("str_equal");
        let result = if unequal {
            format!("!{equal}")
        } else {
            equal.clone()
        };
        // Keep operands sequenced inside the surrounding lazy expression.
        // shortcut: C strings use NUL termination; embedded NUL needs a length-bearing ABI.
        format!(
            "__extension__({{ const unsigned char *{a} = (const unsigned char*)({left}); const unsigned char *{b} = (const unsigned char*)({right}); int {equal} = 0; if ({a} && {b}) {{ while (*{a} && *{a} == *{b}) {{ ++{a}; ++{b}; }} {equal} = *{a} == *{b}; }} {result}; }})"
        )
    }

    fn pattern_condition(
        &mut self,
        scrutinee: &str,
        pattern: &str,
        typ: &str,
    ) -> (String, Vec<String>) {
        if typ == "const char*" {
            if let Ok(crate::core_ir::MatchPattern::StringPat(value)) =
                crate::core_ir::MatchPattern::parse(pattern)
            {
                return (
                    self.string_equality(
                        scrutinee,
                        &format!("\"{}\"", escape_c_string(&value)),
                        false,
                    ),
                    vec![],
                );
            }
        }
        match_arm_cond_and_binds(scrutinee, pattern, typ)
    }

    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn push(&mut self, s: &str) {
        for _ in 0..self.indent_lvl {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn indent(&mut self) {
        self.indent_lvl += 1;
    }

    fn dedent(&mut self) {
        self.indent_lvl = self.indent_lvl.saturating_sub(1);
    }

    fn is_func(&self, name: &str) -> bool {
        self.func_names.contains(name)
    }

    fn expr_str(&mut self, expr: &Expr) -> String {
        match expr {
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => format!(
                "({} ? {} : {})",
                self.expr_str(cond),
                self.expr_str(then_expr),
                self.expr_str(else_expr)
            ),
            Expr::MatchValue { .. } => unreachable!("match values rejected by C backend scan"),
            Expr::Block { .. } => unreachable!("block values rejected by C backend scan"),
            Expr::UnitLit => unreachable!("unit values refused by C backend scan"),
            Expr::IntLit(n) => format!("INT64_C({})", n),
            Expr::FloatLit(f) => {
                let bits = f.0;
                if bits.is_finite() {
                    format!("{:?}", bits)
                } else if bits.is_nan() {
                    "(0.0/0.0)".into()
                } else if bits.is_sign_negative() {
                    "(-1.0/0.0)".into()
                } else {
                    "(1.0/0.0)".into()
                }
            }
            Expr::BoolLit(b) => {
                if *b {
                    "1".into()
                } else {
                    "0".into()
                }
            }
            Expr::StringLit(s) => format!("\"{}\"", escape_c_string(s)),
            Expr::Ident(name) => {
                if self.is_func(name) {
                    format!("(uint64_t)({})", sanitize_ident(name))
                } else {
                    sanitize_ident(name)
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let (l, r) = (self.expr_str(lhs), self.expr_str(rhs));
                if matches!(op.as_str(), "eq" | "==" | "neq" | "!=") {
                    let checker = TypeChecker::new();
                    if checker
                        .expr_type(lhs, &self.facts, &self.types)
                        .map(|t| t.canonical())
                        == Some(Typ::String)
                        && checker
                            .expr_type(rhs, &self.facts, &self.types)
                            .map(|t| t.canonical())
                            == Some(Typ::String)
                    {
                        return self.string_equality(&l, &r, matches!(op.as_str(), "neq" | "!="));
                    }
                }
                match op.as_str() {
                    "land" | "&&" => format!("({} && {})", l, r),
                    "lor" | "||" => format!("({} || {})", l, r),
                    "eq" | "==" => format!("({} == {})", l, r),
                    "neq" | "!=" => format!("({} != {})", l, r),
                    "lt" | "<" => format!("({} < {})", l, r),
                    "gt" | ">" => format!("({} > {})", l, r),
                    "le" | "<=" => format!("({} <= {})", l, r),
                    "ge" | ">=" => format!("({} >= {})", l, r),
                    "add" | "+" => format!("({} + {})", l, r),
                    "sub" | "-" => format!("({} - {})", l, r),
                    "mul" | "*" => format!("({} * {})", l, r),
                    "udiv" | "urem" | "lshr" => {
                        if self.global_initializer {
                            self.fail("c-backend: unsigned operation in a global initializer requires constant folding".into());
                            return "0".into();
                        }
                        let failure = if matches!(op.as_str(), "udiv" | "urem") {
                            let Some(failure) = self.word_failure_return else {
                                self.fail("c-backend: checked unsigned division requires an Int, Bool, or Void function return".into());
                                return "0".into();
                            };
                            Some(failure)
                        } else {
                            None
                        };
                        // The C projection uses a 64-bit Int carrier. Capture
                        // both operands in source order before unsigned math.
                        let left = self.fresh("unsigned_left");
                        let right = self.fresh("unsigned_right");
                        let operator = match op.as_str() {
                            "udiv" => "/",
                            "urem" => "%",
                            _ => ">>",
                        };
                        let right_value = if op == "lshr" {
                            format!("({right} & 63)")
                        } else {
                            right.clone()
                        };
                        let guard = failure
                            .map(|failure| format!("if ({right} == 0) {{ {failure} }} "))
                            .unwrap_or_default();
                        format!(
                            "({{ uint64_t {left} = (uint64_t)({l}); uint64_t {right} = (uint64_t)({r}); {guard}(int64_t)({left} {operator} {right_value}); }})"
                        )
                    }
                    "div" | "/" => format!("({} / {})", l, r),
                    "mod" | "%" => format!("({} % {})", l, r),
                    "band" | "&" => format!("({} & {})", l, r),
                    "bor" | "|" => format!("({} | {})", l, r),
                    "xor" | "^" => format!("({} ^ {})", l, r),
                    "shl" | "<<" => format!("({} << {})", l, r),
                    "shr" | ">>" => format!("({} >> {})", l, r),
                    _ => format!("({} /*op:{}*/ {})", l, sanitize_comment(op), r),
                }
            }
            Expr::Unary { op, expr } => {
                if crate::core_ir::integer_cast_spec(op).is_some() {
                    let value =
                        crate::core_ir::integer_cast_expr(expr.as_ref().clone(), op).unwrap();
                    return format!("((int64_t)({}))", self.expr_str(&value));
                }
                let e = self.expr_str(expr);
                if let Some((bits, signed)) = crate::core_ir::saturating_float_cast_spec(op) {
                    return format!("in_float_saturate({e}, {bits}, {})", u8::from(signed));
                }

                match op.as_str() {
                    "bool-to-int" | "float-to-int" => format!("((int64_t)({e}))"),
                    "int-to-float" | "float-cast-f64" => format!("((double)({e}))"),
                    "float-cast-f32" => format!("((double)((float)({e})))"),
                    "neg" | "-" => format!("(-{})", e),
                    "not" | "!" => format!("(!{})", e),
                    "bnot" | "~" => format!("(~{})", e),
                    _ => format!("(/*op:{}*/{})", sanitize_comment(op), e),
                }
            }
            Expr::Call { callee, args } => {
                // method call: recv.method(args) → method(recv, args…)
                if let Expr::Field { base, name } = &**callee {
                    let mut as_ = vec![self.expr_str(base)];
                    as_.extend(args.iter().map(|a| self.expr_str(a)));
                    return format!("{}({})", sanitize_ident(name), as_.join(", "));
                }
                if let Expr::Ident(name) = &**callee {
                    if self.types.contains_key(name) && !self.is_func(name) {
                        self.fail(format!(
                            "c-backend: indirect local call `{name}` requires a function signature"
                        ));
                        return "0".into();
                    }
                    self.intrinsic_or_call(name, args)
                } else {
                    self.fail(
                        "c-backend: indirect expression call requires a function signature".into(),
                    );
                    "0".into()
                }
            }
            Expr::Field { base, name } => {
                format!("({}).{}", self.expr_str(base), sanitize_ident(name))
            }
            Expr::StructInit { name, fields } => {
                let fs: Vec<String> = fields
                    .iter()
                    .map(|(fn_, fe)| format!(".{} = {}", sanitize_ident(fn_), self.expr_str(fe)))
                    .collect();
                format!("((struct {}){{{}}})", sanitize_ident(name), fs.join(", "))
            }
            Expr::Index { base, index } => {
                format!("({})[{}]", self.expr_str(base), self.expr_str(index))
            }
            Expr::ArrayLit(items) => {
                let els: Vec<String> = items.iter().map(|e| self.expr_str(e)).collect();
                let typ = TypeChecker::new().expr_type(expr, &self.facts, &self.types);
                let element = match typ.map(|typ| typ.canonical()) {
                    // Empty sequence inference carries Void; preserve the existing empty carrier.
                    Some(Typ::Array(item)) if item.canonical() == Typ::Void => "int64_t".into(),
                    Some(Typ::Array(item)) => ctype(&item),
                    _ => {
                        self.fail("c-backend: cannot infer array element type".into());
                        "int64_t".into()
                    }
                };
                // Compound literal; caller must keep storage live.
                format!(
                    "(({element}[]){{{}}})",
                    if els.is_empty() {
                        "0".into()
                    } else {
                        els.join(", ")
                    }
                )
            }
            Expr::Closure {
                params,
                ret,
                body,
                captures,
            } => {
                let cname = self.fresh("closure");
                let env_name = format!("{}_env", cname);
                let mut helper = String::new();
                // Captures use file-scope static env so value stays a uint64_t fn ptr.
                // ponytail: not re-entrant / not thread-safe; heap env if that matters.
                if !captures.is_empty() {
                    helper.push_str(&format!("struct {} {{\n", env_name));
                    for cap in captures {
                        helper.push_str(&format!(
                            "  {} {};\n",
                            ctype(&self.capture_type(cap)),
                            sanitize_ident(cap)
                        ));
                    }
                    helper.push_str(&format!("}};\nstatic struct {} {};\n", env_name, env_name));
                }
                let ps: Vec<String> = params
                    .iter()
                    .map(|(n, t)| format!("{} {}", ctype(t), sanitize_ident(n)))
                    .collect();
                helper.push_str(&format!(
                    "static {} {}({}) {{\n",
                    ctype(ret),
                    cname,
                    ps.join(", ")
                ));
                let mut nested = Ctx::new(self.func_names.clone(), self.facts.clone());
                nested.throwing = self.throwing.clone();
                nested.error = self.error.clone();
                nested.reserved = self.reserved.clone();
                nested.tmp = self.tmp;
                nested.types = self.types.clone();
                nested.types.extend(params.iter().cloned());
                nested.global_names = self.global_names.clone();
                nested.ret_is_void = matches!(ret.canonical(), Typ::Void);
                nested.word_failure_return = word_failure_return(ret);
                nested.indent_lvl = 1;
                for cap in captures {
                    let n = sanitize_ident(cap);
                    nested.push(&format!(
                        "{} {} = {}.{};",
                        ctype(&self.capture_type(cap)),
                        n,
                        env_name,
                        n
                    ));
                }
                let mut locals = Vec::new();
                let mut known = self.global_names.as_ref().clone();
                known.extend(params.iter().map(|(name, _)| sanitize_ident(name)));
                let mut env = nested.types.clone();
                if let Err(error) = collect_locals(
                    body,
                    &mut locals,
                    &known,
                    &nested.facts,
                    &mut env,
                    &mut nested.types,
                ) {
                    nested.fail(error);
                }
                nested.types.extend(env);
                for (n, ty) in &locals {
                    if captures.iter().any(|c| sanitize_ident(c) == *n) {
                        continue;
                    }
                    nested.push(&format!("{} {};", ty, n));
                }
                for s in body {
                    emit_stmt(&mut nested, s);
                }
                if nested.ret_is_void && !body.last().is_some_and(|s| matches!(s, Stmt::Return(_)))
                {
                    nested.push("return;");
                }
                helper.push_str(&nested.out);
                helper.push_str("}\n");
                self.tmp = nested.tmp;
                self.hoisted.extend(nested.hoisted);
                self.hoisted.push(helper);
                if captures.is_empty() {
                    format!("(uint64_t)({})", cname)
                } else {
                    let fills: Vec<String> = captures
                        .iter()
                        .map(|cap| {
                            let n = sanitize_ident(cap);
                            format!("{}.{} = {}", env_name, n, n)
                        })
                        .collect();
                    format!("({{ {}; (uint64_t)({}); }})", fills.join("; "), cname)
                }
            }
        }
    }

    fn raw_native_invoke(&mut self, name: &str, args: &[Expr]) -> String {
        if self.global_initializer {
            self.fail("c-backend: raw native call in a global initializer requires runtime initialization".into());
            return "0".into();
        }
        let expected = match name {
            "invoke-native" | "invoke_native" => 1,
            "invoke-native1" | "invoke_native1" => 2,
            _ => 3,
        };
        if args.len() != expected {
            self.fail(format!(
                "c-backend: `{name}` requires {expected} Int arguments"
            ));
            return "0".into();
        }
        if let Expr::Ident(target) = &args[0]
            && !self.types.contains_key(target)
            && let Some((params, ret)) = self.facts.function_signature(target)
        {
            if self.throwing.contains(target) {
                self.fail(format!("c-backend: raw native target `{target}` may throw"));
                return "0".into();
            }
            if let Some(error) = crate::core_ir_verifier::invoke_signature_error(name, params, ret)
            {
                self.fail(format!("c-backend: {error}"));
                return "0".into();
            }
        }
        let checker = TypeChecker::new();
        if args.iter().any(|arg| {
            checker
                .expr_type(arg, &self.facts, &self.types)
                .map(|typ| typ.canonical())
                != Some(Typ::Int)
        }) {
            self.fail(format!("c-backend: `{name}` requires Int arguments"));
            return "0".into();
        }
        let mut staged = Vec::with_capacity(args.len());
        let mut out = String::from("__extension__({ ");
        for arg in args {
            let slot = self.fresh("native_arg");
            let value = self.expr_str(arg);
            out.push_str(&format!("int64_t {slot} = {value}; "));
            staged.push(slot);
        }
        out.push_str(&format!("if ({} == 0) __builtin_trap(); ", staged[0]));
        let params = if expected == 1 {
            "void".into()
        } else {
            vec!["int64_t"; expected - 1].join(", ")
        };
        out.push_str(&format!(
            "((int64_t(*)({params}))(uintptr_t){} )({}); }})",
            staged[0],
            staged[1..].join(", ")
        ));
        out
    }

    fn intrinsic_or_call(&mut self, name: &str, args: &[Expr]) -> String {
        if name == crate::core_ir::COMPILER_UNREACHABLE {
            if !args.is_empty() || self.is_func(name) {
                self.fail(
                    "c-backend: private unreachable requires zero args and cannot be declared"
                        .into(),
                );
                return "0".into();
            }
            return "__builtin_trap()".into();
        }
        if crate::core_ir::compiler_print_callee(name).is_some() {
            self.fail("c-backend: private print requires a hosted runtime".into());
            return "0".into();
        }
        if crate::core_ir_verifier::is_native_invoke(name) && !self.is_func(name) {
            return self.raw_native_invoke(name, args);
        }
        if matches!(name, "array_len" | "array-len")
            && !self.is_func(name)
            && args.first().is_some_and(|arg| {
                matches!(
                    TypeChecker::new().expr_type(arg, &self.facts, &self.types),
                    Some(Typ::Array(_))
                )
            })
        {
            self.fail("c-backend: array length requires a length-bearing iterable".into());
            return "0".into();
        }
        let evaled: Vec<String> = args.iter().map(|e| self.expr_str(e)).collect();
        let a = |i: usize| -> String { evaled.get(i).cloned().unwrap_or_else(|| "0".into()) };
        match name {
            "outb" => {
                let p = a(0);
                let v = a(1);
                format!(
                    "__extension__({{ __asm__ volatile(\"outb %0, %1\" : : \"a\"((uint8_t)({v})) , \"Nd\"((uint16_t)({p})) : \"memory\"); }})"
                )
            }
            "inb" => {
                let p = a(0);
                format!(
                    "__extension__({{ uint8_t _v; __asm__ volatile(\"inb %1, %0\" : \"=a\"(_v) : \"Nd\"((uint16_t)({p})) : \"memory\"); _v; }})"
                )
            }
            "outl" => {
                let p = a(0);
                let v = a(1);
                format!(
                    "__extension__({{ __asm__ volatile(\"outl %0, %1\" : : \"a\"((uint32_t)({v})) , \"Nd\"((uint16_t)({p})) : \"memory\"); }})"
                )
            }
            "inl" => {
                let p = a(0);
                format!(
                    "__extension__({{ uint32_t _v; __asm__ volatile(\"inl %1, %0\" : \"=a\"(_v) : \"Nd\"((uint16_t)({p})) : \"memory\"); _v; }})"
                )
            }
            "hlt" => "__extension__({ __asm__ volatile(\"hlt\" : : : \"memory\"); })".into(),
            "cli" => "__extension__({ __asm__ volatile(\"cli\" : : : \"memory\"); })".into(),
            "sti" => "__extension__({ __asm__ volatile(\"sti\" : : : \"memory\"); })".into(),
            "invlpg" => format!(
                "__extension__({{ __asm__ volatile(\"invlpg (%0)\" : : \"r\"({}) : \"memory\"); }})",
                a(0)
            ),
            "lidt" => format!(
                "__extension__({{ __asm__ volatile(\"lidt (%0)\" : : \"r\"({}) : \"memory\"); }})",
                a(0)
            ),
            "read_cr2" => {
                "__extension__({ uint64_t _v; __asm__ volatile(\"mov %%cr2, %0\" : \"=r\"(_v)); _v; })"
                    .into()
            }
            "load8" => format!("(*(volatile uint8_t*)({}))", a(0)),
            "load16" => format!("(*(volatile uint16_t*)({}))", a(0)),
            "load32" => format!("(*(volatile uint32_t*)({}))", a(0)),
            "load64" => format!("(*(volatile uint64_t*)({}))", a(0)),
            "store8" => format!(
                "__extension__({{ *(volatile uint8_t*)({}) = (uint8_t)({}); }})",
                a(0),
                a(1)
            ),
            "store16" => format!(
                "__extension__({{ *(volatile uint16_t*)({}) = (uint16_t)({}); }})",
                a(0),
                a(1)
            ),
            "store32" => format!(
                "__extension__({{ *(volatile uint32_t*)({}) = (uint32_t)({}); }})",
                a(0),
                a(1)
            ),
            "store64" => format!(
                "__extension__({{ *(volatile uint64_t*)({}) = (uint64_t)({}); }})",
                a(0),
                a(1)
            ),
            "invoke" if args.len() == 1 => {
                format!("((uint64_t(*)(uint64_t))({}))({})", a(0), a(0))
            }
            "invoke" if args.len() == 2 => {
                format!("((uint64_t(*)(uint64_t))({}))({})", a(0), a(1))
            }
            "invoke" if args.len() == 3 => {
                format!(
                    "((uint64_t(*)(uint64_t,uint64_t))({}))({}, {})",
                    a(0),
                    a(1),
                    a(2)
                )
            }
            "invoke1" if args.len() == 2 => {
                format!("((uint64_t(*)(uint64_t))({}))({})", a(0), a(1))
            }
            "invoke2" if args.len() == 3 => {
                format!(
                    "((uint64_t(*)(uint64_t,uint64_t))({}))({}, {})",
                    a(0),
                    a(1),
                    a(2)
                )
            }
            _ => {
                let as_: Vec<String> = args.iter().map(|a| self.expr_str(a)).collect();
                format!("{}({})", sanitize_ident(name), as_.join(", "))
            }
        }
    }
}

fn emit_fn_def(ctx: &mut Ctx, name: &str, params: &[(String, Typ)], ret: &Typ, body: &[Stmt]) {
    let ps: Vec<String> = params
        .iter()
        .map(|(n, t)| format!("{} {}", ctype(t), sanitize_ident(n)))
        .collect();
    let sec = if name == "kernel_entry" {
        " __attribute__((section(\".text.entry\")))"
    } else {
        ""
    };
    let attr = format!("__attribute__((used)){}", sec);
    let sig = format!(
        "{} {} {}({})",
        attr,
        ctype(ret),
        sanitize_ident(name),
        ps.join(", ")
    );

    // Build body into a nested buffer so hoisted closures land before the fn.
    let mut body_ctx = Ctx::new(ctx.func_names.clone(), ctx.facts.clone());
    body_ctx.throwing = ctx.throwing.clone();
    body_ctx.error = ctx.error.clone();
    body_ctx.reserved = ctx.reserved.clone();
    body_ctx.types = ctx.types.clone();
    body_ctx.types.extend(params.iter().cloned());
    body_ctx.global_names = ctx.global_names.clone();
    body_ctx.tmp = ctx.tmp;
    body_ctx.ret_is_void = matches!(ret.canonical(), Typ::Void);
    body_ctx.word_failure_return = word_failure_return(ret);
    body_ctx.indent_lvl = 1;

    let mut locals: Vec<(String, String)> = Vec::new();
    let mut known = ctx.global_names.as_ref().clone();
    known.extend(params.iter().map(|(name, _)| sanitize_ident(name)));
    let mut env = body_ctx.types.clone();
    if let Err(error) = collect_locals(
        body,
        &mut locals,
        &known,
        &body_ctx.facts,
        &mut env,
        &mut body_ctx.types,
    ) {
        body_ctx.fail(error);
    }
    body_ctx.types.extend(env);
    for (n, ty) in &locals {
        body_ctx.push(&format!("{} {};", ty, n));
    }
    // runtime error flag for throw/try/propagate (function-local, simple model)
    body_ctx.push("int __in_err = 0;");
    body_ctx.push("int64_t __in_err_val = 0;");

    for stmt in body {
        emit_stmt(&mut body_ctx, stmt);
    }
    if body_ctx.ret_is_void && !body.last().is_some_and(|s| matches!(s, Stmt::Return(_))) {
        body_ctx.push("return;");
    }

    ctx.tmp = body_ctx.tmp;
    for h in body_ctx.hoisted.drain(..) {
        ctx.out.push_str(&h);
        ctx.out.push('\n');
    }

    ctx.push(&format!("{} {{", sig));
    ctx.out.push_str(&body_ctx.out);
    ctx.line("}");
    ctx.line("");
}

fn emit_stmt(ctx: &mut Ctx, stmt: &Stmt) {
    match stmt {
        Stmt::Let(name, _, expr) => {
            let rhs = ctx.expr_str(expr);
            ctx.push(&format!("{} = {};", sanitize_ident(name), rhs));
        }
        Stmt::Assign(name, expr) => {
            let rhs = ctx.expr_str(expr);
            ctx.push(&format!("{} = {};", sanitize_ident(name), rhs));
        }
        Stmt::IndexAssign { base, index, value } => {
            let b = ctx.expr_str(base);
            let i = ctx.expr_str(index);
            let v = ctx.expr_str(value);
            ctx.push(&format!("({})[{}] = {};", b, i, v));
        }
        Stmt::FieldAssign { base, name, value } => {
            let b = ctx.expr_str(base);
            let v = ctx.expr_str(value);
            ctx.push(&format!("({}).{} = {};", b, sanitize_ident(name), v));
        }
        Stmt::Return(e) => match e {
            Some(x) => {
                let v = ctx.expr_str(x);
                ctx.push(&format!("return {};", v));
            }
            None => ctx.push("return;"),
        },
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            let c = ctx.expr_str(cond);
            ctx.push(&format!("if ({}) {{", c));
            ctx.indent();
            for s in then_body {
                emit_stmt(ctx, s);
            }
            ctx.dedent();
            if !else_body.is_empty() {
                ctx.line("} else {");
                ctx.indent();
                for s in else_body {
                    emit_stmt(ctx, s);
                }
                ctx.dedent();
            }
            ctx.line("}");
        }
        Stmt::Loop { kind, cond, body } => {
            match kind {
                LoopKind::For { binding } => {
                    // cond = iterable. Prefer InVec {ptr,len} layout from native lower.
                    let iter = cond
                        .as_ref()
                        .map(|c| ctx.expr_str(c))
                        .unwrap_or_else(|| "((struct InVec){0})".into());
                    let idx = ctx.fresh("i");
                    let len = ctx.fresh("len");
                    let ptr = ctx.fresh("ptr");
                    let vec = ctx.fresh("vec");
                    let bind = sanitize_ident(binding.trim().trim_start_matches("& "));
                    ctx.push(&format!("struct InVec {} = {};", vec, iter));
                    let item_type = ctx
                        .types
                        .get(binding.trim().trim_start_matches("& "))
                        .map(ctype)
                        .unwrap_or_else(|| "int64_t".into());
                    ctx.push(&format!(
                        "{item_type}* {} = ({item_type}*)({}.ptr);",
                        ptr, vec
                    ));
                    ctx.push(&format!("int64_t {} = {}.len;", len, vec));
                    ctx.push(&format!(
                        "for (int64_t {} = 0; {} < {}; {}++) {{",
                        idx, idx, len, idx
                    ));
                    ctx.indent();
                    if bind != "_" && !bind.is_empty() {
                        ctx.push(&format!("{} = {}[{}];", bind, ptr, idx));
                    }
                    for s in body {
                        emit_stmt(ctx, s);
                    }
                    ctx.dedent();
                    ctx.line("}");
                }
                LoopKind::While | LoopKind::Infinite => {
                    match cond {
                        Some(c) => {
                            let cstr = ctx.expr_str(c);
                            ctx.push(&format!("while ({}) {{", cstr));
                        }
                        None => ctx.line("while (1) {"),
                    }
                    ctx.indent();
                    for s in body {
                        emit_stmt(ctx, s);
                    }
                    ctx.dedent();
                    ctx.line("}");
                }
            }
        }
        Stmt::Match { scrutinee, arms } => {
            // Simple if/else chain; patterns are source text (Core IR stores pattern strings).
            let scr_tmp = ctx.fresh("scr");
            let scr = ctx.expr_str(scrutinee);
            let typ = TypeChecker::new().expr_type(scrutinee, &ctx.facts, &ctx.types);
            let scr_type = typ.as_ref().map(ctype).unwrap_or_else(|| {
                ctx.fail("c-backend: cannot infer match scrutinee type".into());
                "int64_t".into()
            });
            ctx.push(&format!("{} {} = {};", scr_type, scr_tmp, scr));
            let mut first = true;
            for arm in arms {
                let (cond, binds) = ctx.pattern_condition(&scr_tmp, &arm.pattern, &scr_type);
                if first {
                    ctx.push(&format!("if ({}) {{", cond));
                    first = false;
                } else {
                    ctx.push(&format!("}} else if ({}) {{", cond));
                }
                ctx.indent();
                for b in binds {
                    ctx.push(&b);
                }
                for s in &arm.body {
                    emit_stmt(ctx, s);
                }
                ctx.dedent();
            }
            if !first {
                ctx.line("}");
            }
        }
        Stmt::Throw(e) => {
            let val = ctx.expr_str(e);
            ctx.push(&format!("__in_err = 1; __in_err_val = (int64_t)({});", val));
            if ctx.ret_is_void {
                ctx.push("return;");
            } else {
                ctx.push("return 0;");
            }
        }
        Stmt::Try { body, catches } => {
            ctx.line("{ /* try */");
            ctx.indent();
            ctx.push("int __in_err_save = __in_err;");
            ctx.push("__in_err = 0;");
            for s in body {
                emit_stmt(ctx, s);
            }
            if !catches.is_empty() {
                ctx.push("if (__in_err) {");
                ctx.indent();
                for (i, c) in catches.iter().enumerate() {
                    let bind = c.pattern.trim();
                    let (cond, binds) = ctx.pattern_condition("__in_err_val", bind, "int64_t");
                    if i == 0 {
                        ctx.push(&format!("if ({}) {{", cond));
                    } else {
                        ctx.push(&format!("}} else if ({}) {{", cond));
                    }
                    ctx.indent();
                    for b in binds {
                        ctx.push(&b);
                    }
                    for s in &c.body {
                        emit_stmt(ctx, s);
                    }
                    ctx.push("__in_err = 0;");
                    ctx.dedent();
                }
                ctx.line("}"); // last else-if arm
                ctx.dedent();
                ctx.line("}");
            }
            ctx.push("if (!__in_err) { __in_err = __in_err_save; }");
            ctx.dedent();
            ctx.line("}");
        }
        Stmt::Propagate => {
            ctx.push("if (__in_err) {");
            ctx.indent();
            if ctx.ret_is_void {
                ctx.push("return;");
            } else {
                ctx.push("return __in_err_val;");
            }
            ctx.dedent();
            ctx.line("}");
        }
        Stmt::Expr(e) => {
            let v = ctx.expr_str(e);
            ctx.push(&format!("{};", v));
        }
        Stmt::Break => {
            ctx.line("break;");
        }
        Stmt::Continue => {
            ctx.line("continue;");
        }
    }
}

fn is_simple_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Returns (C condition, binding statements) for a match arm pattern string.
fn match_arm_cond_and_binds(scrutinee: &str, pattern: &str, typ: &str) -> (String, Vec<String>) {
    let p = pattern.trim().trim_end_matches(':').trim();
    if p == "_" || p == "-" || p == "else" || p == "default" || p == ".." {
        return ("1".into(), vec![]);
    }
    if p == "true" {
        return (format!("({})", scrutinee), vec![]);
    }
    if p == "false" {
        return (format!("(!({}))", scrutinee), vec![]);
    }
    if let Ok(n) = p.parse::<i64>() {
        return (format!("(({}) == INT64_C({}))", scrutinee, n), vec![]);
    }
    // Ident binding: always matches, bind name = scrutinee.
    if is_simple_ident(p) {
        let name = sanitize_ident(p);
        return (
            "1".into(),
            vec![format!("{} {} = {};", typ, name, scrutinee)],
        );
    }
    // Struct / other patterns: always true (lossy).
    ("1".into(), vec![])
}

fn collect_locals(
    stmts: &[Stmt],
    locals: &mut Vec<(String, String)>,
    known: &HashSet<String>,
    facts: &Facts,
    env: &mut HashMap<String, Typ>,
    all_types: &mut HashMap<String, Typ>,
) -> Result<(), String> {
    let checker = TypeChecker::new();
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, _, value) | Stmt::Assign(name, value) => {
                let annotation = match stmt {
                    Stmt::Let(_, annotation, _) => annotation.as_ref(),
                    _ => env.get(name),
                };
                // Closures use the existing C function-address carrier ABI.
                let typ = annotation
                    .cloned()
                    .or_else(|| checker.expr_type(value, facts, env))
                    .or_else(|| matches!(value, Expr::Closure { .. }).then_some(Typ::Int))
                    .ok_or_else(|| format!("c-backend: cannot infer local `{name}`"))?;
                let n = sanitize_ident(name);
                if !known.contains(&n) && !locals.iter().any(|(existing, _)| existing == &n) {
                    locals.push((n, ctype(&typ)));
                }
                all_types.insert(name.clone(), typ.clone());
                env.insert(name.clone(), typ);
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_locals(then_body, locals, known, facts, &mut env.clone(), all_types)?;
                collect_locals(else_body, locals, known, facts, &mut env.clone(), all_types)?;
            }
            Stmt::Loop { kind, cond, body } => {
                let mut inner = env.clone();
                if let LoopKind::For { binding } = kind {
                    let typ = cond
                        .as_ref()
                        .and_then(|expr| checker.expr_type(expr, facts, env));
                    let item = match typ.map(|typ| typ.canonical()) {
                        Some(Typ::Vector(item)) => *item,
                        Some(Typ::Array(_)) => {
                            return Err(
                                "c-backend: array iteration requires a length-bearing iterable"
                                    .into(),
                            );
                        }
                        Some(Typ::Named(name)) if name == "Vec" || name == "InVec" => Typ::Int,
                        _ => return Err("c-backend: cannot infer iterator element type".into()),
                    };
                    let name = binding.trim().trim_start_matches("& ").to_string();
                    let emitted = sanitize_ident(&name);
                    if emitted != "_"
                        && !emitted.is_empty()
                        && !locals.iter().any(|(n, _)| n == &emitted)
                    {
                        locals.push((emitted, ctype(&item)));
                    }
                    all_types.insert(name.clone(), item.clone());
                    inner.insert(name, item);
                }
                collect_locals(body, locals, known, facts, &mut inner, all_types)?;
            }
            Stmt::Match { scrutinee, arms } => {
                let typ = checker.expr_type(scrutinee, facts, env);
                for arm in arms {
                    let mut inner = env.clone();
                    if let Some(typ) = &typ {
                        let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern)?;
                        let bindings =
                            pattern.typed_bindings(typ, &|name| facts.pattern_fields(name))?;
                        all_types.extend(bindings.iter().cloned());
                        inner.extend(bindings);
                    }
                    collect_locals(&arm.body, locals, known, facts, &mut inner, all_types)?;
                }
            }
            Stmt::Try { body, catches } => {
                collect_locals(body, locals, known, facts, &mut env.clone(), all_types)?;
                for catch in catches {
                    let mut inner = env.clone();
                    let pattern = crate::core_ir::MatchPattern::parse(&catch.pattern)?;
                    let bindings =
                        pattern.typed_bindings(&Typ::Int, &|name| facts.pattern_fields(name))?;
                    all_types.extend(bindings.iter().cloned());
                    inner.extend(bindings);
                    collect_locals(&catch.body, locals, known, facts, &mut inner, all_types)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "c_backend_tests.rs"]
mod tests;
