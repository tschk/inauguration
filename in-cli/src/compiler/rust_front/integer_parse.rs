//! Owned integer parsing, lowered through the normal Rust/Core IR pipeline.
use super::*;

pub(super) const U64_PARSE: &str = "#rust-parse-u64";
pub(crate) const STR_BYTE: &str = "#rust-str-byte";

pub(super) fn unmaterialized_result() -> Expr {
    Expr::Ident(
        crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
            "storing an owned parse Result requires a tagged Result value; consume it with ?, return, or an immediate error match".into(),
        )
        .symbol(),
    )
}

pub(super) fn retain_helpers(module: &mut UnifiedModule) -> Result<(), String> {
    let mut needed = false;
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        needed |= matches!(expr, Expr::Ident(name) if name == U64_PARSE);
    });
    if !needed {
        return Ok(());
    }
    // Keep the algorithm in owned code rather than importing rustc's Result or
    // ParseIntError layout. Errors use the existing fallible-call error slot;
    // they can be propagated or mapped without inventing a foreign ABI.
    let mut helper = parse_rust_source(
        r#"
        fn parse_u64(text: &str, radix: u32) -> Result<u64, i64> {
            if radix < 2 || radix > 36 { __in_parse_panic(); }
            let length = text.len();
            let mut index: usize = 0;
            if length == 0 { return Err(0i64); }
            if __in_str_byte(text, 0) == 43 { index = 1; }
            if index == length { return Err(1i64); }
            let mut value: u64 = 0;
            let base = radix as u64;
            while index < length {
                let byte = __in_str_byte(text, index);
                let digit: u64;
                if byte >= 48 && byte <= 57 { digit = (byte - 48) as u64; }
                else if byte >= 65 && byte <= 90 { digit = (byte - 55) as u64; }
                else if byte >= 97 && byte <= 122 { digit = (byte - 87) as u64; }
                else { return Err(1i64); }
                if digit >= base { return Err(1i64); }
                if value > (u64::MAX - digit) / base { return Err(2i64); }
                value = value * base + digit;
                index += 1;
            }
            Ok(value)
        }
        "#,
    )?;
    for declaration in &mut helper.decls {
        if let Decl::Function { name, .. } = declaration {
            *name = U64_PARSE.into();
        }
    }
    crate::core_opt::map_decl_exprs_mut(&mut helper.decls, &mut |expr| {
        if let Expr::Ident(name) = expr {
            match name.as_str() {
                "__in_str_byte" => *name = STR_BYTE.into(),
                "__in_parse_panic" => *name = crate::core_ir::COMPILER_UNREACHABLE.into(),
                _ => {}
            }
        }
    });
    module.decls.extend(helper.decls);
    Ok(())
}

pub(super) fn decimal_call(
    method: &syn::ExprMethodCall,
    locals: &HashMap<String, String>,
) -> Option<Expr> {
    if method.method != "parse" || !method.args.is_empty() {
        return None;
    }
    let arguments = method.turbofish.as_ref()?;
    let syn::GenericArgument::Type(typ) = arguments.args.first()? else {
        return None;
    };
    if arguments.args.len() != 1
        || generic_argument_type(typ).ok()? != Typ::Named("#rust-integer-u64".into())
    {
        return None;
    }
    // Method spelling alone is not proof of FromStr: user-defined receivers
    // may have an unrelated parse method. Only select an owned string receiver.
    let string = match method.receiver.as_ref() {
        syn::Expr::Lit(value) => matches!(value.lit, syn::Lit::Str(_)),
        syn::Expr::Path(path) => path.path.get_ident().is_some_and(|name| {
            locals
                .get(&name.to_string())
                .is_some_and(|typ| typ == "String")
                && type_aliases::source_signature_type(&syn::parse_quote!(String))
                    .ok()
                    .zip(
                        type_aliases::source_signature_type(&syn::parse_quote!(
                            ::std::string::String
                        ))
                        .ok(),
                    )
                    .is_some_and(|(source, standard)| source == standard)
        }),
        _ => false,
    };
    string.then(|| Expr::Call {
        callee: Box::new(Expr::Ident(U64_PARSE.into())),
        args: vec![
            lower_expr_with_types(&method.receiver, &mut locals.clone()),
            rust_integer_literal(10, "u32"),
        ],
    })
}

pub(super) fn map_error(
    method: &syn::ExprMethodCall,
    locals: &mut HashMap<String, String>,
) -> Option<Expr> {
    if method.method != "map_err" || method.turbofish.is_some() || method.args.len() != 1 {
        return None;
    }
    let syn::Expr::Closure(mapper) = method.args.first()? else {
        return None;
    };
    // ParseIntError remains opaque: never expose its private runtime code as a
    // Rust integer. Mappers which inspect it need an owned error-value type.
    if mapper.inputs.len() != 1
        || mapper.asyncness.is_some()
        || !matches!(mapper.inputs.first(), Some(syn::Pat::Wild(_)))
    {
        return None;
    }
    struct Returns(bool);
    impl<'ast> syn::visit::Visit<'ast> for Returns {
        fn visit_expr_return(&mut self, _: &'ast syn::ExprReturn) {
            self.0 = true;
        }
    }
    let mut returns = Returns(false);
    syn::visit::Visit::visit_expr(&mut returns, &mapper.body);
    if returns.0 {
        // An inlined mapper must not turn a closure return into a caller return.
        return None;
    }
    let receiver = lower_fallible_expr(&method.receiver, locals);
    if !matches!(&receiver, Expr::Call {callee,..} if matches!(callee.as_ref(), Expr::Ident(name) if name == U64_PARSE))
    {
        return None;
    }
    let mut inner = locals.clone();
    let result = "#rust-parsed-value".to_string();
    Some(Expr::Block {
        body: vec![
            Stmt::Declare(result.clone(), Typ::Named("#rust-integer-u64".into())),
            Stmt::Try {
                body: vec![Stmt::Assign(result.clone(), receiver)],
                catches: vec![CatchArm {
                    pattern: "_".into(),
                    body: vec![Stmt::Throw(lower_expr_with_types(&mapper.body, &mut inner))],
                }],
            },
        ],
        result: Some(Box::new(Expr::Ident(result))),
    })
}
