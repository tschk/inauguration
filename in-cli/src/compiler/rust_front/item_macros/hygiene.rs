//! Preserve expansion-local value bindings while restoring item and type names.
use super::super::type_aliases::{Presence, attrs_presence, item_presence};
use proc_macro2::{Group, Ident, Span, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{HashMap, HashSet};
use syn::parse::Parser;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;

pub(super) struct Hygiene {
    prefix: String,
    pub(super) next: usize,
    originals: HashMap<String, Ident>,
    self_spans: HashMap<usize, Span>,
    self_contexts: HashMap<String, usize>,
    source_files: HashSet<String>,
}
impl Hygiene {
    pub(super) fn new(source: &str, tokens: TokenStream) -> Self {
        let mut prefix = "__in_macro_context_".to_string();
        while source.contains(&prefix) {
            prefix.push('_');
        }
        let mut source_files = HashSet::new();
        let mut pending = vec![tokens];
        while let Some(tokens) = pending.pop() {
            for token in tokens {
                source_files.insert(token.span().file());
                if let TokenTree::Group(group) = token {
                    pending.push(group.stream());
                }
            }
        }
        Self {
            prefix,
            next: 1,
            originals: HashMap::new(),
            self_spans: HashMap::new(),
            self_contexts: HashMap::new(),
            source_files,
        }
    }
    pub(super) fn original(&self, name: &Ident) -> Ident {
        self.originals
            .get(&name.to_string())
            .unwrap_or(name)
            .clone()
    }
    pub(super) fn context_span(&mut self, context: usize) -> Result<Span, String> {
        if let Some(span) = self.self_spans.get(&context) {
            return Ok(*span);
        }
        let tokens = "self"
            .parse::<TokenStream>()
            .map_err(|error| error.to_string())?;
        let span = tokens
            .into_iter()
            .next()
            .ok_or("missing source context marker")?
            .span();
        let file = span.file();
        if file == "<unspecified>"
            || self.source_files.contains(&file)
            || self.self_contexts.contains_key(&file)
        {
            return Err("source macro receiver contexts require distinct source spans".into());
        }
        self.self_contexts.insert(file, context);
        self.self_spans.insert(context, span);
        Ok(span)
    }
    fn self_context(&self, span: Span) -> usize {
        self.self_contexts.get(&span.file()).copied().unwrap_or(0)
    }
    pub(super) fn literal(&mut self, name: &Ident, context: usize) -> Result<Ident, String> {
        let spelling = name.to_string();
        if spelling == "self" {
            let mut result = name.clone();
            result.set_span(self.context_span(context)?);
            return Ok(result);
        }
        if self.originals.contains_key(&spelling)
            || matches!(
                spelling.as_str(),
                "as" | "async"
                    | "await"
                    | "break"
                    | "const"
                    | "continue"
                    | "crate"
                    | "dyn"
                    | "else"
                    | "enum"
                    | "extern"
                    | "false"
                    | "fn"
                    | "for"
                    | "if"
                    | "impl"
                    | "in"
                    | "let"
                    | "loop"
                    | "match"
                    | "mod"
                    | "move"
                    | "mut"
                    | "pub"
                    | "ref"
                    | "return"
                    | "self"
                    | "Self"
                    | "static"
                    | "struct"
                    | "super"
                    | "trait"
                    | "true"
                    | "type"
                    | "unsafe"
                    | "use"
                    | "where"
                    | "while"
                    | "yield"
                    | "try"
                    | "gen"
                    | "macro_rules"
                    | "_"
            )
        {
            return Ok(name.clone());
        }
        let identifier = format!(
            "{}{context}_{}",
            self.prefix,
            spelling.strip_prefix("r#").unwrap_or(&spelling)
        );
        self.originals.insert(identifier.clone(), name.clone());
        Ok(Ident::new(&identifier, name.span()))
    }
    pub(super) fn attributes(&self, attributes: &mut [syn::Attribute]) {
        for attribute in attributes {
            Clean {
                hygiene: self,
                bindings: Bindings::default(),
                uncaptured: HashSet::new(),
                receiver: None,
                failure: None,
            }
            .visit_attribute_mut(attribute);
        }
    }
    pub(super) fn normalize(&self, file: &mut syn::File) -> Result<(), String> {
        let mut clean = Clean {
            hygiene: self,
            bindings: Bindings::default(),
            uncaptured: HashSet::new(),
            receiver: None,
            failure: None,
        };
        clean.visit_file_mut(file);
        clean.failure.map_or(Ok(()), Err)
    }
}

#[derive(Default)]
struct Bindings {
    values: HashSet<String>,
}
impl Bindings {
    fn arguments(signature: &syn::Signature) -> Self {
        let mut bindings = Self::default();
        for input in &signature.inputs {
            if let syn::FnArg::Typed(input) = input {
                bindings.visit_pat(&input.pat);
            }
        }
        bindings
    }
    fn condition(&mut self, value: &syn::Expr) {
        match value {
            syn::Expr::Let(value) => self.visit_pat(&value.pat),
            syn::Expr::Binary(value) if matches!(value.op, syn::BinOp::And(_)) => {
                self.condition(&value.left);
                self.condition(&value.right);
            }
            syn::Expr::Paren(value) => self.condition(&value.expr),
            syn::Expr::Group(value) => self.condition(&value.expr),
            _ => {}
        }
    }
}
impl<'ast> Visit<'ast> for Bindings {
    fn visit_pat_ident(&mut self, value: &'ast syn::PatIdent) {
        self.values.insert(value.ident.to_string());
        syn::visit::visit_pat_ident(self, value);
    }
}
struct Clean<'a> {
    hygiene: &'a Hygiene,
    bindings: Bindings,
    uncaptured: HashSet<String>,
    receiver: Option<usize>,
    failure: Option<String>,
}
impl Clean<'_> {
    fn protected(&self, name: &Ident, value: bool) -> bool {
        self.hygiene.originals.contains_key(&name.to_string())
            && value
            && (self.bindings.values.contains(&name.to_string())
                || self
                    .bindings
                    .values
                    .contains(&self.hygiene.original(name).to_string()))
    }
    fn tokens(&self, tokens: TokenStream, protect_values: bool) -> TokenStream {
        tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Ident(name) if !self.protected(&name, protect_values) => {
                    TokenTree::Ident(self.hygiene.original(&name))
                }
                TokenTree::Group(group) => TokenTree::Group(Group::new(
                    group.delimiter(),
                    self.tokens(group.stream(), protect_values),
                )),
                token => token,
            })
            .collect()
    }
    fn scope(&mut self, additions: Bindings, visit: impl FnOnce(&mut Self)) {
        let mut values = Vec::new();
        let uncaptured = self.uncaptured.clone();
        for value in additions.values {
            self.uncaptured.remove(&value);
            if self.bindings.values.insert(value.clone()) {
                values.push(value);
            }
        }
        visit(self);
        for value in values {
            self.bindings.values.remove(&value);
        }
        self.uncaptured = uncaptured;
    }
    fn function(&mut self, signature: &syn::Signature, visit: impl FnOnce(&mut Self)) {
        // Retain outer names so an illegal fn-item capture cannot become a global lookup.
        let uncaptured = std::mem::replace(&mut self.uncaptured, self.bindings.values.clone());
        let receiver = self.receiver;
        self.receiver = signature
            .receiver()
            .map(|receiver| self.hygiene.self_context(receiver.self_token.span));
        self.scope(Bindings::arguments(signature), visit);
        self.receiver = receiver;
        self.uncaptured = uncaptured;
    }
    fn condition(&mut self, value: &mut syn::Expr) {
        match value {
            syn::Expr::Binary(value) if matches!(value.op, syn::BinOp::And(_)) => {
                self.condition(&mut value.left);
                let mut bindings = Bindings::default();
                bindings.condition(&value.left);
                self.scope(bindings, |this| this.condition(&mut value.right));
            }
            syn::Expr::Paren(value) => self.condition(&mut value.expr),
            syn::Expr::Group(value) => self.condition(&mut value.expr),
            _ => self.visit_expr_mut(value),
        }
    }
    fn macro_receivers(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Ident(name) if name == "self" => {
                    let colon = |token: Option<&TokenTree>| {
                        token.is_some_and(|token| {
                            matches!(token,
                        TokenTree::Punct(punctuation) if punctuation.as_char() == ':')
                        })
                    };
                    let module_path = colon(tokens.get(at + 1)) && colon(tokens.get(at + 2));
                    if !module_path && self.receiver != Some(self.hygiene.self_context(name.span()))
                    {
                        self.failure = Some(
                            "source macro value self has no receiver in the same source context"
                                .into(),
                        );
                    }
                }
                TokenTree::Group(group) => self.macro_receivers(group.stream()),
                _ => {}
            }
        }
    }
    fn format_captures(
        &mut self,
        arguments: &mut syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
    ) -> Result<(), String> {
        let Some(syn::Expr::Lit(first)) = arguments.first() else {
            return Ok(());
        };
        let syn::Lit::Str(template) = &first.lit else {
            return Ok(());
        };
        let context = self.hygiene.self_context(template.span());
        if context == 0
            && self.receiver.is_none_or(|context| context == 0)
            && !self
                .bindings
                .values
                .iter()
                .any(|name| self.hygiene.originals.contains_key(name))
        {
            return Ok(());
        }
        let span = template.span();
        let text = template.value();
        let mut characters = text.chars().peekable();
        let mut captures = Vec::new();
        while let Some(character) = characters.next() {
            if character != '{' {
                continue;
            }
            if characters.next_if_eq(&'{').is_some() {
                continue;
            }
            let field: String = characters
                .by_ref()
                .take_while(|character| *character != '}')
                .collect();
            let (name, spec) = field.split_once(':').unwrap_or((&field, ""));
            if !name.is_empty() && !name.chars().all(|character| character.is_ascii_digit()) {
                captures.push(name.to_string());
            }
            for (end, _) in spec.match_indices('$') {
                let name: String = spec[..end]
                    .chars()
                    .rev()
                    .take_while(|character| character.is_alphanumeric() || *character == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let name = name.trim_start_matches('0');
                if !name.is_empty() && !name.chars().all(|character| character.is_ascii_digit()) {
                    captures.push(name.to_string());
                }
            }
        }
        let mut explicit: HashSet<_> = arguments
            .iter()
            .filter_map(|argument| {
                let syn::Expr::Assign(argument) = argument else {
                    return None;
                };
                let syn::Expr::Path(name) = argument.left.as_ref() else {
                    return None;
                };
                name.path
                    .get_ident()
                    .map(|name| name.to_string().trim_start_matches("r#").to_string())
            })
            .collect();
        for name in captures {
            let mut reference = syn::parse_str::<syn::ExprPath>(&name).map_err(|_| {
                "source macro format capture requires an owned identifier".to_string()
            })?;
            if reference.qself.is_some() || reference.path.get_ident().is_none() {
                return Err("source macro format capture requires an owned identifier".into());
            }
            let mut field = reference.path.segments[0].ident.clone();
            field.set_span(span);
            let name = field.to_string().trim_start_matches("r#").to_string();
            if !explicit.insert(name.clone()) {
                continue;
            }
            let protected = format!("{}{context}_{name}", self.hygiene.prefix);
            let binding = if context != 0
                && (self.bindings.values.contains(&protected)
                    || self.bindings.values.contains(&name))
            {
                Ident::new(&protected, span)
            } else {
                field.clone()
            };
            reference.path.segments[0].ident = binding;
            let mut reference = syn::Expr::Path(reference);
            self.visit_expr_mut(&mut reference);
            arguments.push(
                syn::parse2(quote::quote!(#field = #reference))
                    .map_err(|error| format!("source macro format capture: {error}"))?,
            );
        }
        Ok(())
    }
}
impl VisitMut for Clean<'_> {
    fn visit_item_mut(&mut self, value: &mut syn::Item) {
        let attrs = match value {
            syn::Item::Const(item) => Some(&mut item.attrs),
            syn::Item::Enum(item) => Some(&mut item.attrs),
            syn::Item::ExternCrate(item) => Some(&mut item.attrs),
            syn::Item::Fn(item) => Some(&mut item.attrs),
            syn::Item::ForeignMod(item) => Some(&mut item.attrs),
            syn::Item::Impl(item) => Some(&mut item.attrs),
            syn::Item::Macro(item) => Some(&mut item.attrs),
            syn::Item::Mod(item) => Some(&mut item.attrs),
            syn::Item::Static(item) => Some(&mut item.attrs),
            syn::Item::Struct(item) => Some(&mut item.attrs),
            syn::Item::Trait(item) => Some(&mut item.attrs),
            syn::Item::TraitAlias(item) => Some(&mut item.attrs),
            syn::Item::Type(item) => Some(&mut item.attrs),
            syn::Item::Union(item) => Some(&mut item.attrs),
            syn::Item::Use(item) => Some(&mut item.attrs),
            _ => None,
        };
        if let Some(attrs) = attrs {
            self.hygiene.attributes(attrs);
        }
        if item_presence(value) == Presence::Inactive {
            return;
        }
        syn::visit_mut::visit_item_mut(self, value);
    }
    fn visit_ident_mut(&mut self, name: &mut Ident) {
        *name = self.hygiene.original(name);
    }
    fn visit_attribute_mut(&mut self, value: &mut syn::Attribute) {
        syn::visit_mut::visit_attribute_mut(self, value);
        if let syn::Meta::List(list) = &mut value.meta {
            list.tokens = self.tokens(list.tokens.clone(), false);
        }
    }
    fn visit_macro_mut(&mut self, value: &mut syn::Macro) {
        self.visit_path_mut(&mut value.path);
        let formatting = value.path.get_ident().is_some_and(|name| {
            matches!(name.to_string().as_str(), "format" | "println" | "eprintln")
        });
        if value.path.get_ident().is_some_and(|name| {
            matches!(
                name.to_string().as_str(),
                "vec" | "env" | "println" | "eprintln" | "format"
            )
        }) && let Ok(mut arguments) =
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(value.tokens.clone())
        {
            for (index, argument) in arguments.iter_mut().enumerate() {
                if formatting
                    && index > 0
                    && let syn::Expr::Assign(argument) = argument
                    && let syn::Expr::Path(name) = argument.left.as_mut()
                    && name.qself.is_none()
                    && name.path.get_ident().is_some()
                {
                    // A named format argument's left side is a field name, not a value.
                    self.visit_path_mut(&mut name.path);
                    self.visit_expr_mut(&mut argument.right);
                } else {
                    self.visit_expr_mut(argument);
                }
            }
            if formatting && let Err(error) = self.format_captures(&mut arguments) {
                self.failure = Some(error);
            }
            value.tokens = arguments.to_token_stream();
            return;
        }
        if !value.path.is_ident("macro_rules") {
            self.macro_receivers(value.tokens.clone());
        }
        value.tokens = self.tokens(value.tokens.clone(), true);
    }
    fn visit_label_mut(&mut self, _: &mut syn::Label) {
        // Labels are local bindings; type lifetimes are not.
    }
    fn visit_expr_break_mut(&mut self, value: &mut syn::ExprBreak) {
        let label = value.label.take();
        syn::visit_mut::visit_expr_break_mut(self, value);
        value.label = label;
    }
    fn visit_expr_continue_mut(&mut self, value: &mut syn::ExprContinue) {
        let label = value.label.take();
        syn::visit_mut::visit_expr_continue_mut(self, value);
        value.label = label;
    }
    fn visit_pat_ident_mut(&mut self, value: &mut syn::PatIdent) {
        let name = value.ident.clone();
        syn::visit_mut::visit_pat_ident_mut(self, value);
        value.ident = name;
    }
    fn visit_expr_path_mut(&mut self, value: &mut syn::ExprPath) {
        if value.qself.is_none()
            && value
                .path
                .get_ident()
                .is_some_and(|name| self.uncaptured.contains(&name.to_string()))
        {
            self.failure = Some("source macro fn item cannot capture outer value bindings".into());
        }
        if value.path.is_ident("self")
            && value.qself.is_none()
            && self.receiver
                != Some(
                    self.hygiene
                        .self_context(value.path.segments[0].ident.span()),
                )
        {
            self.failure =
                Some("source macro value self has no receiver in the same source context".into());
        }
        let head = value
            .path
            .segments
            .first()
            .map(|segment| segment.ident.clone())
            .filter(|name| {
                value.path.segments.len() == 1
                    && value.qself.is_none()
                    && self.protected(name, true)
            });
        syn::visit_mut::visit_expr_path_mut(self, value);
        if let Some(head) = head {
            value
                .path
                .segments
                .first_mut()
                .expect("path had a head")
                .ident = head;
        }
    }
    fn visit_field_value_mut(&mut self, value: &mut syn::FieldValue) {
        syn::visit_mut::visit_field_value_mut(self, value);
        if value.colon_token.is_none()
            && let (syn::Member::Named(member), syn::Expr::Path(path)) =
                (&value.member, &value.expr)
            && !path.path.is_ident(member)
        {
            value.colon_token = Some(Default::default());
        }
    }
    fn visit_field_pat_mut(&mut self, value: &mut syn::FieldPat) {
        syn::visit_mut::visit_field_pat_mut(self, value);
        if value.colon_token.is_none()
            && let (syn::Member::Named(member), syn::Pat::Ident(pattern)) =
                (&value.member, value.pat.as_ref())
            && pattern.ident != *member
        {
            value.colon_token = Some(Default::default());
        }
    }
    fn visit_item_fn_mut(&mut self, value: &mut syn::ItemFn) {
        self.function(&value.sig.clone(), |this| {
            syn::visit_mut::visit_item_fn_mut(this, value)
        });
    }
    fn visit_impl_item_fn_mut(&mut self, value: &mut syn::ImplItemFn) {
        self.hygiene.attributes(&mut value.attrs);
        if attrs_presence(&value.attrs) == Presence::Inactive {
            return;
        }
        self.function(&value.sig.clone(), |this| {
            syn::visit_mut::visit_impl_item_fn_mut(this, value)
        });
    }
    fn visit_trait_item_fn_mut(&mut self, value: &mut syn::TraitItemFn) {
        self.hygiene.attributes(&mut value.attrs);
        if attrs_presence(&value.attrs) == Presence::Inactive {
            return;
        }
        self.function(&value.sig.clone(), |this| {
            syn::visit_mut::visit_trait_item_fn_mut(this, value)
        });
    }
    fn visit_block_mut(&mut self, value: &mut syn::Block) {
        let previous = self.bindings.values.clone();
        let uncaptured = self.uncaptured.clone();
        for statement in &mut value.stmts {
            // A let's initializer and else block precede the new binding's scope.
            self.visit_stmt_mut(statement);
            if let syn::Stmt::Local(local) = statement {
                let mut bindings = Bindings::default();
                bindings.visit_pat(&local.pat);
                for name in bindings.values {
                    self.uncaptured.remove(&name);
                    self.bindings.values.insert(name);
                }
            }
        }
        self.bindings.values = previous;
        self.uncaptured = uncaptured;
    }
    fn visit_arm_mut(&mut self, value: &mut syn::Arm) {
        let mut bindings = Bindings::default();
        bindings.visit_pat(&value.pat);
        self.scope(bindings, |this| syn::visit_mut::visit_arm_mut(this, value));
    }
    fn visit_expr_closure_mut(&mut self, value: &mut syn::ExprClosure) {
        let mut bindings = Bindings::default();
        for input in &value.inputs {
            bindings.visit_pat(input);
        }
        self.scope(bindings, |this| {
            syn::visit_mut::visit_expr_closure_mut(this, value)
        });
    }
    fn visit_expr_for_loop_mut(&mut self, value: &mut syn::ExprForLoop) {
        for attribute in &mut value.attrs {
            self.visit_attribute_mut(attribute);
        }
        self.visit_expr_mut(&mut value.expr);
        self.visit_pat_mut(&mut value.pat);
        let mut bindings = Bindings::default();
        bindings.visit_pat(&value.pat);
        self.scope(bindings, |this| this.visit_block_mut(&mut value.body));
    }
    fn visit_expr_if_mut(&mut self, value: &mut syn::ExprIf) {
        for attribute in &mut value.attrs {
            self.visit_attribute_mut(attribute);
        }
        self.condition(&mut value.cond);
        let mut bindings = Bindings::default();
        bindings.condition(&value.cond);
        self.scope(bindings, |this| {
            this.visit_block_mut(&mut value.then_branch)
        });
        if let Some((_, branch)) = &mut value.else_branch {
            self.visit_expr_mut(branch);
        }
    }
    fn visit_expr_while_mut(&mut self, value: &mut syn::ExprWhile) {
        for attribute in &mut value.attrs {
            self.visit_attribute_mut(attribute);
        }
        self.condition(&mut value.cond);
        let mut bindings = Bindings::default();
        bindings.condition(&value.cond);
        self.scope(bindings, |this| this.visit_block_mut(&mut value.body));
    }
}
