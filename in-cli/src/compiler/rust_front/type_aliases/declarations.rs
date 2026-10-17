//! Validate parsed alias declarations independently of executable lowering.
use super::*;
use syn::visit::{self, Visit};

fn node<T>(value: &T) -> usize {
    std::ptr::from_ref(value) as usize
}
fn active(attrs: &[syn::Attribute]) -> bool {
    attrs_presence(attrs) == Presence::Active
}
fn expression_active(expr: &syn::Expr) -> bool {
    let attrs = match expr {
        syn::Expr::Array(expr) => &expr.attrs,
        syn::Expr::Assign(expr) => &expr.attrs,
        syn::Expr::Async(expr) => &expr.attrs,
        syn::Expr::Await(expr) => &expr.attrs,
        syn::Expr::Binary(expr) => &expr.attrs,
        syn::Expr::Block(expr) => &expr.attrs,
        syn::Expr::Break(expr) => &expr.attrs,
        syn::Expr::Call(expr) => &expr.attrs,
        syn::Expr::Cast(expr) => &expr.attrs,
        syn::Expr::Closure(expr) => &expr.attrs,
        syn::Expr::Const(expr) => &expr.attrs,
        syn::Expr::Continue(expr) => &expr.attrs,
        syn::Expr::Field(expr) => &expr.attrs,
        syn::Expr::ForLoop(expr) => &expr.attrs,
        syn::Expr::Group(expr) => &expr.attrs,
        syn::Expr::If(expr) => &expr.attrs,
        syn::Expr::Index(expr) => &expr.attrs,
        syn::Expr::Infer(expr) => &expr.attrs,
        syn::Expr::Let(expr) => &expr.attrs,
        syn::Expr::Lit(expr) => &expr.attrs,
        syn::Expr::Loop(expr) => &expr.attrs,
        syn::Expr::Macro(expr) => &expr.attrs,
        syn::Expr::Match(expr) => &expr.attrs,
        syn::Expr::MethodCall(expr) => &expr.attrs,
        syn::Expr::Paren(expr) => &expr.attrs,
        syn::Expr::Path(expr) => &expr.attrs,
        syn::Expr::Range(expr) => &expr.attrs,
        syn::Expr::RawAddr(expr) => &expr.attrs,
        syn::Expr::Reference(expr) => &expr.attrs,
        syn::Expr::Repeat(expr) => &expr.attrs,
        syn::Expr::Return(expr) => &expr.attrs,
        syn::Expr::Struct(expr) => &expr.attrs,
        syn::Expr::Try(expr) => &expr.attrs,
        syn::Expr::TryBlock(expr) => &expr.attrs,
        syn::Expr::Tuple(expr) => &expr.attrs,
        syn::Expr::Unary(expr) => &expr.attrs,
        syn::Expr::Unsafe(expr) => &expr.attrs,
        syn::Expr::While(expr) => &expr.attrs,
        syn::Expr::Yield(expr) => &expr.attrs,
        _ => return true,
    };
    active(attrs)
}
fn impl_item_active(item: &syn::ImplItem) -> bool {
    match item {
        syn::ImplItem::Const(item) => active(&item.attrs),
        syn::ImplItem::Fn(item) => active(&item.attrs),
        syn::ImplItem::Type(item) => active(&item.attrs),
        syn::ImplItem::Macro(item) => active(&item.attrs),
        _ => true,
    }
}
fn trait_item_active(item: &syn::TraitItem) -> bool {
    match item {
        syn::TraitItem::Const(item) => active(&item.attrs),
        syn::TraitItem::Fn(item) => active(&item.attrs),
        syn::TraitItem::Type(item) => active(&item.attrs),
        syn::TraitItem::Macro(item) => active(&item.attrs),
        _ => true,
    }
}

fn pattern_active(pattern: &syn::Pat) -> bool {
    let attrs = match pattern {
        syn::Pat::Const(pattern) => &pattern.attrs,
        syn::Pat::Ident(pattern) => &pattern.attrs,
        syn::Pat::Lit(pattern) => &pattern.attrs,
        syn::Pat::Macro(pattern) => &pattern.attrs,
        syn::Pat::Or(pattern) => &pattern.attrs,
        syn::Pat::Paren(pattern) => &pattern.attrs,
        syn::Pat::Path(pattern) => &pattern.attrs,
        syn::Pat::Range(pattern) => &pattern.attrs,
        syn::Pat::Reference(pattern) => &pattern.attrs,
        syn::Pat::Rest(pattern) => &pattern.attrs,
        syn::Pat::Slice(pattern) => &pattern.attrs,
        syn::Pat::Struct(pattern) => &pattern.attrs,
        syn::Pat::Tuple(pattern) => &pattern.attrs,
        syn::Pat::TupleStruct(pattern) => &pattern.attrs,
        syn::Pat::Type(pattern) => &pattern.attrs,
        syn::Pat::Wild(pattern) => &pattern.attrs,
        _ => return true,
    };
    active(attrs)
}
// Both phases traverse precisely the same active parsed syntax; macro tokens remain opaque.
macro_rules! presence_visitors {
    () => {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            if item_presence(item) == Presence::Active {
                visit::visit_item(self, item);
            }
        }
        fn visit_expr(&mut self, expr: &'ast syn::Expr) {
            if expression_active(expr) {
                visit::visit_expr(self, expr);
            }
        }
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if active(&local.attrs) {
                visit::visit_local(self, local);
            }
        }
        fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
            if impl_item_active(item) {
                visit::visit_impl_item(self, item);
            }
        }
        fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
            if trait_item_active(item) {
                visit::visit_trait_item(self, item);
            }
        }
        fn visit_pat(&mut self, pattern: &'ast syn::Pat) {
            if pattern_active(pattern) {
                visit::visit_pat(self, pattern);
            }
        }
        fn visit_field(&mut self, field: &'ast syn::Field) {
            if active(&field.attrs) {
                visit::visit_field(self, field);
            }
        }
        fn visit_field_pat(&mut self, field: &'ast syn::FieldPat) {
            if active(&field.attrs) {
                visit::visit_field_pat(self, field);
            }
        }
        fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
            if active(&field.attrs) {
                visit::visit_field_value(self, field);
            }
        }
        fn visit_variant(&mut self, variant: &'ast syn::Variant) {
            if active(&variant.attrs) {
                visit::visit_variant(self, variant);
            }
        }
        fn visit_fn_arg(&mut self, argument: &'ast syn::FnArg) {
            let attrs = match argument {
                syn::FnArg::Receiver(argument) => &argument.attrs,
                syn::FnArg::Typed(argument) => &argument.attrs,
            };
            if active(attrs) {
                visit::visit_fn_arg(self, argument);
            }
        }
        fn visit_generic_param(&mut self, parameter: &'ast syn::GenericParam) {
            let attrs = match parameter {
                syn::GenericParam::Lifetime(parameter) => &parameter.attrs,
                syn::GenericParam::Type(parameter) => &parameter.attrs,
                syn::GenericParam::Const(parameter) => &parameter.attrs,
            };
            if active(attrs) {
                visit::visit_generic_param(self, parameter);
            }
        }
        fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
            let attrs = match item {
                syn::ForeignItem::Fn(item) => &item.attrs,
                syn::ForeignItem::Static(item) => &item.attrs,
                syn::ForeignItem::Type(item) => &item.attrs,
                syn::ForeignItem::Macro(item) => &item.attrs,
                _ => return,
            };
            if active(attrs) {
                visit::visit_foreign_item(self, item);
            }
        }
        fn visit_arm(&mut self, arm: &'ast syn::Arm) {
            if active(&arm.attrs) {
                visit::visit_arm(self, arm);
            }
        }
    };
}

struct Prepared {
    catalog: Catalog,
    module: Vec<String>,
    blocks: HashMap<usize, Rc<Scope>>,
    modules: HashMap<usize, Vec<String>>,
    next_module: usize,
}
impl Prepared {
    fn scope<'a>(
        &mut self,
        items: impl Iterator<Item = &'a syn::Item> + Clone,
        block: bool,
    ) -> Rc<Scope> {
        let mut scope = Scope::items(items.clone(), (!block).then_some(self.module.as_slice()));
        for item in items {
            let syn::Item::Mod(module) = item else {
                continue;
            };
            if item_presence(item) != Presence::Active || module.content.is_none() {
                continue;
            }
            let mut path = self.module.clone();
            path.push(if block {
                self.next_module += 1;
                format!("#lexical-{}:{}", self.next_module, module.ident)
            } else {
                module.ident.to_string()
            });
            self.modules.insert(node(module), path.clone());
            fn replace(binding: &mut Binding, path: &[String]) {
                match binding {
                    Binding::Visible(binding, _) => replace(binding, path),
                    Binding::External | Binding::Module(_) => {
                        *binding = Binding::Module(path.to_vec())
                    }
                    _ => {}
                }
            }
            if let Some(binding) = scope.bindings.get_mut(&module.ident.to_string()) {
                replace(binding, &path);
            }
        }
        Rc::new(scope)
    }
}
impl<'ast> Visit<'ast> for Prepared {
    presence_visitors!();
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let Some((_, items)) = &module.content else {
            return;
        };
        let Some(path) = self.modules.get(&node(module)).cloned() else {
            return;
        };
        let outer = std::mem::replace(&mut self.module, path);
        let scope = self.scope(items.iter(), false);
        self.catalog.modules.insert(self.module.clone(), scope);
        visit::visit_item_mod(self, module);
        self.module = outer;
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let scope = self.scope(
            block.stmts.iter().filter_map(|stmt| match stmt {
                syn::Stmt::Item(item) => Some(item),
                _ => None,
            }),
            true,
        );
        self.blocks.insert(node(block), scope);
        visit::visit_block(self, block);
    }
}

struct Validator<'a> {
    frame: Frame,
    blocks: &'a HashMap<usize, Rc<Scope>>,
    modules: &'a HashMap<usize, Vec<String>>,
    failures: Vec<Failure>,
}
impl Validator<'_> {
    fn aliases(&mut self, scope: &Scope) {
        let _scope = self.frame.enter();
        for alias in &scope.aliases {
            if let Err(failure @ Failure::Invalid(_)) =
                source_type_owned(&syn::Path::from(alias.clone()))
            {
                self.failures.push(failure);
            }
        }
    }
    fn scoped(&mut self, frame: Frame, visit: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.frame, frame);
        visit(self);
        self.frame = outer;
    }
}
macro_rules! generic_visitor {
    ($method:ident, $ty:ty, $($field:ident).+) => {
        fn $method(&mut self, item: &'ast $ty) {
            let frame = self.frame.generics(&item.$($field).+);
            self.scoped(frame, |visitor| visit::$method(visitor, item));
        }
    };
}
impl<'ast> Visit<'ast> for Validator<'_> {
    presence_visitors!();
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let Some(path) = self.modules.get(&node(module)) else {
            return;
        };
        let frame = Frame {
            catalog: self.frame.catalog.clone(),
            module: path.clone(),
            locals: vec![],
        };
        self.scoped(frame, |visitor| {
            if let Some(scope) = visitor
                .frame
                .catalog
                .modules
                .get(&visitor.frame.module)
                .cloned()
            {
                visitor.aliases(&scope);
            }
            visit::visit_item_mod(visitor, module);
        });
    }
    fn visit_block(&mut self, block: &'ast syn::Block) {
        let Some(scope) = self.blocks.get(&node(block)).cloned() else {
            return;
        };
        let mut frame = self.frame.clone();
        frame.locals.push(scope.clone());
        self.scoped(frame, |visitor| {
            visitor.aliases(&scope);
            visit::visit_block(visitor, block);
        });
    }
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        let mut frame = self.frame.clone();
        // A nested item cannot capture enclosing generic parameters; keep their names deferred.
        for scope in &mut frame.locals {
            if scope
                .bindings
                .values()
                .any(|binding| matches!(binding, Binding::Generic))
            {
                let mut masked = scope.as_ref().clone();
                for binding in masked.bindings.values_mut() {
                    if matches!(binding, Binding::Generic) {
                        *binding = Binding::Conditional;
                    }
                }
                *scope = Rc::new(masked);
            }
        }
        frame = frame.generics(&function.sig.generics);
        self.scoped(frame, |visitor| visit::visit_item_fn(visitor, function));
    }
    generic_visitor!(visit_item_impl, syn::ItemImpl, generics);
    generic_visitor!(visit_item_trait, syn::ItemTrait, generics);
    generic_visitor!(visit_item_struct, syn::ItemStruct, generics);
    generic_visitor!(visit_item_enum, syn::ItemEnum, generics);
    generic_visitor!(visit_item_union, syn::ItemUnion, generics);
    generic_visitor!(visit_item_type, syn::ItemType, generics);
    generic_visitor!(visit_impl_item_fn, syn::ImplItemFn, sig.generics);
    generic_visitor!(visit_trait_item_fn, syn::TraitItemFn, sig.generics);
}

pub(super) fn validate(file: &syn::File, catalog: Rc<Catalog>) -> Vec<Failure> {
    if !active(&file.attrs) || !Frame::root(catalog.clone()).source_is_active() {
        return vec![];
    }
    let mut prepared = Prepared {
        catalog: catalog.as_ref().clone(),
        module: vec![],
        blocks: HashMap::new(),
        modules: HashMap::new(),
        next_module: 0,
    };
    let root = prepared.scope(file.items.iter(), false);
    prepared.catalog.modules.insert(vec![], root.clone());
    prepared.visit_file(file);
    let mut validator = Validator {
        frame: Frame::root(Rc::new(prepared.catalog)),
        blocks: &prepared.blocks,
        modules: &prepared.modules,
        failures: vec![],
    };
    validator.aliases(&root);
    validator.visit_file(file);
    validator.failures
}

#[cfg(test)]
mod tests {
    use super::*;
    fn failures(source: &str) -> Vec<Failure> {
        let file = syn::parse_file(source).unwrap();
        validate(&file, Catalog::new(&file))
    }
    #[test]
    fn unused_alias_declarations_retain_definite_errors_in_all_parsed_bodies() {
        for source in [
            "type A=B;type B=A;fn main()->i64{42}",
            "type Byte=u8;type Byte=u16;",
            "fn unused(){type A=B;type B=A;}",
            "fn outer(){fn unused(){type A=B;type B=A;}}",
            "fn outer(){let closure=||{type A=B;type B=A;};}",
            "fn outer(){let value=unknown({type A=B;type B=A;42});}",
            "const VALUE:i64={type A=B;type B=A;42};",
            "struct Row;impl Row{fn unused(){type A=B;type B=A;}}",
            "trait Row{fn unused(){type A=B;type B=A;}}",
            "mod parent{fn outer(){mod child{type A=B;type B=A;}}}",
            "fn outer(){mod child{type A=self::B;type B=A;}}",
            "fn outer(){mod child{pub type A=u8;pub type A=u16;}type B=child::A;}",
            "fn value(){}type A=(value,);",
            "struct Row<T>{x:T}type A=Row;",
        ] {
            let errors = failures(source);
            assert!(!errors.is_empty(), "{source}");
            assert!(
                errors.iter().all(Failure::is_invalid),
                "{source}: {errors:?}"
            );
        }
    }
    #[test]
    fn alias_scopes_preserve_forward_names_and_lexical_module_identity() {
        for source in [
            "type A=B;type B=u8;",
            "type A=u16;fn outer(){type B=A;type A=u8;}",
            "fn outer(){{type A=u8;}{type A=u16;}}",
            "fn outer(){{mod same{pub type A=u8;}type A=same::A;}{mod same{pub type A=u16;}type A=same::A;}}",
            "type A=u8;fn outer(){type A=u16;mod child{type X=super::A;type Y=self::X;}}",
            "type A=u8;mod parent{type A=u16;fn outer(){mod child{type X=super::A;mod grandchild{type Y=super::X;}}}}",
            "fn value(){}type value=u8;type A=value;",
            "mod parent{pub(super) type A=u8;fn outer(){mod child{type X=super::A;}}}",
            "fn outer(){mod child{type A=u8;}type A=u16;}",
        ] {
            assert!(failures(source).is_empty(), "{source}");
        }
        // Sibling blocks do not share a same-named module's bindings.
        let source = "fn outer(){{mod same{pub type A=u8;}type X=same::A;}{mod same{pub fn A(){}}type X=same::A;}}";
        assert!(!failures(source).is_empty());
    }
    #[test]
    fn alias_validation_defers_cfg_generic_and_opaque_syntax() {
        for source in [
            "#[cfg(any())]type A=A;",
            "#[cfg(feature=\"unknown\")]type A=A;",
            "#[cfg(feature=\"unknown\")]mod child{type A=A;}",
            "#[cfg(feature=\"unknown\")]fn unused(){type A=A;}",
            "fn outer(){#[cfg(any())]fn unused(){type A=A;}}",
            "fn outer(){#[cfg(feature=\"unknown\")]let unused={type A=A;42};}",
            "fn outer(){let unused=#[cfg(any())]{type A=A;42};}",
            "struct Row;impl Row{#[cfg(any())]fn unused(){type A=A;}}",
            "trait Row{#[cfg(feature=\"unknown\")]fn unused(){type A=A;}}",
            "struct Row{#[cfg(any())]field:[u8;{type A=A;1}]}",
            "enum Row{#[cfg(any())]Variant([u8;{type A=A;1}])}",
            "type A<T>=A<T>;",
            "fn outer<T>(){type A=T;fn nested(){type A=T;}}",
            "type A=external::Missing;",
            "opaque!{type A=A;}",
            "fn outer(){opaque!{type A=A;}}",
        ] {
            assert!(failures(source).is_empty(), "{source}");
        }
        assert!(!failures("#[cfg(feature=\"unknown\")]type T=T;type A=A;").is_empty());
    }
    #[test]
    fn generic_parameter_cfg_never_invents_arity_or_duplicate_errors() {
        for source in [
            "struct Row<#[cfg(any())] T>{}type Alias=Row;",
            "fn f<#[cfg(any())] T,T>(){type A=T;}",
            "type T=u8;fn f<#[cfg(any())] T>(){type A=T;}",
            "struct Row<#[cfg(feature=\"unknown\")] T>{}type Alias=Row;",
            "fn f<#[cfg(feature=\"unknown\")] T,T>(){type A=T;}",
        ] {
            assert!(failures(source).is_empty(), "{source}");
        }
        for source in [
            "struct Row<T>{}type Alias=Row;",
            "fn f<T,T>(){type A=T;}",
            "fn f<T,#[cfg(feature=\"unknown\")] T,T>(){type A=T;}",
        ] {
            assert!(!failures(source).is_empty(), "{source}");
        }
    }
    #[test]
    fn source_catalog_retains_conditional_ancestors_and_file_inner_cfg() {
        let file = syn::parse_file("type A=A;").unwrap();
        for presence in [Presence::Conditional, Presence::Inactive] {
            let catalog = Catalog::module_source_with_presence(&file, presence);
            assert!(!Frame::root(catalog.clone()).source_is_active());
            assert!(validate(&file, catalog).is_empty());
        }
        let catalog = Catalog::module_source_with_presence(&file, Presence::Active);
        assert!(Frame::root(catalog.clone()).source_is_active());
        assert!(!validate(&file, catalog).is_empty());
        for source in [
            "#![cfg(any())]type A=A;",
            "#![cfg(feature=\"unknown\")]type A=A;",
        ] {
            let file = syn::parse_file(source).unwrap();
            let catalog = Catalog::module_source(&file);
            assert!(!Frame::root(catalog.clone()).source_is_active());
            assert!(validate(&file, catalog).is_empty());
        }
        let file = syn::parse_file("#![cfg(all())]type A=A;").unwrap();
        assert!(!validate(&file, Catalog::new(&file)).is_empty());
        let file = syn::parse_file("#[cfg(feature=\"unknown\")]mod child{type A=A;}").unwrap();
        let frame = Frame::root(Catalog::new(&file));
        assert!(frame.source_is_active());
        assert!(!frame.module("child").source_is_active());
        assert!(!frame.module("missing").source_is_active());
    }
    #[test]
    fn alias_validation_respects_external_catalog_root_ownership() {
        let file = syn::parse_file("type A=crate::Unknown;type B=B;").unwrap();
        let catalog = Catalog::module_source(&file);
        let errors = validate(&file, catalog);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].is_invalid());
        let file = syn::parse_file("type A=crate::Unknown;type B=self::A;").unwrap();
        assert!(validate(&file, Catalog::module_source(&file)).is_empty());
    }
}
