//! Source-owned primitive alias proofs. Nominal layout/alias lowering stays with the frontend.
use super::{imports, rust_integer_path};
use crate::core_ir::{Typ, type_proofs::ResolutionFailure};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

mod declarations;
mod enum_variants;
mod generic_aliases;
mod source_signatures;
mod trait_statics;
pub(super) fn validate_declarations(
    file: &syn::File,
    catalog: Rc<Catalog>,
) -> Vec<ResolutionFailure> {
    declarations::validate(file, catalog)
}

struct SourceCfg {
    features: HashSet<String>,
    target: Option<HashSet<String>>,
}
thread_local! {
    static SOURCE_CFG: RefCell<Option<SourceCfg>> = const { RefCell::new(None) };
}

pub(super) fn with_cargo_cfg<R>(
    features: &[String],
    target: Option<&[String]>,
    parse: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<SourceCfg>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SOURCE_CFG.with(|current| *current.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SOURCE_CFG.with(|current| {
        current.replace(Some(SourceCfg {
            features: features.iter().cloned().collect(),
            target: target.map(|cfg| cfg.iter().cloned().collect()),
        }))
    }));
    parse()
}

/// Source-only cfg truth. Unknown flags/features never select a host configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    Active,
    Inactive,
    Conditional,
}
impl Presence {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Inactive, _) | (_, Self::Inactive) => Self::Inactive,
            (Self::Active, Self::Active) => Self::Active,
            _ => Self::Conditional,
        }
    }
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Active, _) | (_, Self::Active) => Self::Active,
            (Self::Inactive, Self::Inactive) => Self::Inactive,
            _ => Self::Conditional,
        }
    }
}
fn meta_arguments(list: &syn::MetaList) -> Option<Vec<syn::Meta>> {
    list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
        .ok()
        .map(|values| values.into_iter().collect())
}
fn cfg_predicate(meta: &syn::Meta) -> Presence {
    // The Rust frontend compiles ordinary source, not a rustc test harness.
    // This is the source compilation mode, independent of this compiler's own
    // cfg(test), which is enabled when running its regression suite.
    if matches!(meta, syn::Meta::Path(path) if path.is_ident("test")) {
        return Presence::Inactive;
    }
    if let syn::Meta::NameValue(value) = meta
        && value.path.is_ident("feature")
        && let syn::Expr::Lit(literal) = &value.value
        && let syn::Lit::Str(feature) = &literal.lit
    {
        return SOURCE_CFG.with(|current| match current.borrow().as_ref() {
            Some(cfg) if cfg.features.contains(&feature.value()) => Presence::Active,
            Some(_) => Presence::Inactive,
            None => Presence::Conditional,
        });
    }
    if let Some(key) = meta.path().get_ident().map(ToString::to_string)
        && (key == "unix" || key == "windows" || key.starts_with("target_"))
    {
        let predicate = match meta {
            syn::Meta::Path(_) => Some(key),
            syn::Meta::NameValue(value) => match &value.value {
                syn::Expr::Lit(literal) => match &literal.lit {
                    syn::Lit::Str(value) => Some(format!("{key}={:?}", value.value())),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if let Some(predicate) = predicate {
            return SOURCE_CFG.with(|current| {
                match current
                    .borrow()
                    .as_ref()
                    .and_then(|cfg| cfg.target.as_ref())
                {
                    Some(target) if target.contains(&predicate) => Presence::Active,
                    Some(_) => Presence::Inactive,
                    None => Presence::Conditional,
                }
            });
        }
    }
    let syn::Meta::List(list) = meta else {
        return Presence::Conditional;
    };
    let Some(arguments) = meta_arguments(list) else {
        return Presence::Conditional;
    };
    if list.path.is_ident("all") {
        arguments.iter().fold(Presence::Active, |value, argument| {
            value.and(cfg_predicate(argument))
        })
    } else if list.path.is_ident("any") {
        arguments
            .iter()
            .fold(Presence::Inactive, |value, argument| {
                value.or(cfg_predicate(argument))
            })
    } else if list.path.is_ident("not") && arguments.len() == 1 {
        match cfg_predicate(&arguments[0]) {
            Presence::Active => Presence::Inactive,
            Presence::Inactive => Presence::Active,
            Presence::Conditional => Presence::Conditional,
        }
    } else {
        Presence::Conditional
    }
}
fn meta_presence(meta: &syn::Meta) -> Presence {
    let syn::Meta::List(list) = meta else {
        return if meta.path().is_ident("cfg") || meta.path().is_ident("cfg_attr") {
            Presence::Conditional
        } else {
            Presence::Active
        };
    };
    if list.path.is_ident("cfg") {
        return meta_arguments(list)
            .filter(|arguments| arguments.len() == 1)
            .map_or(Presence::Conditional, |arguments| {
                cfg_predicate(&arguments[0])
            });
    }
    if !list.path.is_ident("cfg_attr") {
        return Presence::Active;
    }
    let Some(arguments) = meta_arguments(list).filter(|arguments| !arguments.is_empty()) else {
        return Presence::Conditional;
    };
    let applied = arguments[1..].iter().fold(Presence::Active, |value, meta| {
        value.and(meta_presence(meta))
    });
    match cfg_predicate(&arguments[0]) {
        Presence::Active => applied,
        Presence::Inactive => Presence::Active,
        Presence::Conditional if applied == Presence::Active => Presence::Active,
        Presence::Conditional => Presence::Conditional,
    }
}
pub(super) fn item_presence(item: &syn::Item) -> Presence {
    let attrs = match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => return Presence::Active,
    };
    attrs_presence(attrs)
}
pub(super) fn attrs_presence(attrs: &[syn::Attribute]) -> Presence {
    attrs.iter().fold(Presence::Active, |value, attr| {
        value.and(meta_presence(&attr.meta))
    })
}
fn generic_presence(parameter: &syn::GenericParam) -> Presence {
    let attrs = match parameter {
        syn::GenericParam::Lifetime(parameter) => &parameter.attrs,
        syn::GenericParam::Type(parameter) => &parameter.attrs,
        syn::GenericParam::Const(parameter) => &parameter.attrs,
    };
    attrs_presence(attrs)
}

// Lifetimes may elide, and conditional parameters may be absent. Only active,
// required type/const parameters establish a definite minimum arity.
fn generic_arity(generics: &syn::Generics) -> (usize, usize) {
    let mut minimum = 0;
    let mut maximum = 0;
    for parameter in &generics.params {
        let presence = generic_presence(parameter);
        if presence == Presence::Inactive {
            continue;
        }
        maximum += 1;
        if presence == Presence::Active
            && match parameter {
                syn::GenericParam::Type(parameter) => parameter.default.is_none(),
                syn::GenericParam::Const(parameter) => parameter.default.is_none(),
                syn::GenericParam::Lifetime(_) => false,
            }
        {
            minimum += 1;
        }
    }
    (minimum, maximum)
}

type Failure = ResolutionFailure;
#[derive(Clone)]
enum Binding {
    Visible(Box<Binding>, syn::Visibility),
    Alias(Rc<(syn::Generics, syn::Type)>),
    Parameter(Rc<Resolved>),
    Import(Vec<String>, bool),
    Module(Vec<String>),
    External,
    Nominal(syn::Generics),
    Generic,
    Value,
    Constant(Rc<(syn::Type, syn::Expr)>),
    Conditional,
    UnknownImport,
    Duplicate,
}
#[derive(Clone, Default)]
struct Scope {
    aliases: Vec<syn::Ident>,
    parameters: bool,
    lifetimes: HashMap<String, Presence>,
    bindings: HashMap<String, Binding>,
    imports: HashMap<String, Vec<Binding>>,
    values: HashMap<String, Binding>,
    enums: HashMap<String, Rc<syn::ItemEnum>>,
    active_types: HashSet<String>,
    active_values: HashSet<String>,
    glob: bool,
    glob_sources: Vec<(Vec<String>, bool, Presence, syn::Visibility)>,
    conditional: bool,
    no_implicit_prelude: bool,
}
impl Scope {
    fn glob_sources(tree: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                Self::glob_sources(&path.tree, prefix, out);
                prefix.pop();
            }
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    Self::glob_sources(tree, prefix, out);
                }
            }
            syn::UseTree::Glob(_) => out.push(prefix.clone()),
            _ => {}
        }
    }

    fn insert(&mut self, name: String, value: Binding) {
        let duplicate =
            !matches!(&value, Binding::Conditional) && !self.active_types.insert(name.clone());
        self.bindings
            .entry(name)
            .and_modify(|previous| {
                *previous = if duplicate || matches!(previous, Binding::Duplicate) {
                    Binding::Duplicate
                } else if matches!(previous, Binding::Conditional)
                    || matches!(&value, Binding::Conditional)
                {
                    Binding::Conditional
                } else {
                    Binding::Duplicate
                };
            })
            .or_insert(value);
    }
    fn items<'a>(items: impl Iterator<Item = &'a syn::Item>, module: Option<&[String]>) -> Self {
        let mut scope = Self::default();
        for item in items {
            let presence = item_presence(item);
            if presence == Presence::Inactive {
                continue;
            }
            if presence == Presence::Active
                && let syn::Item::Type(alias) = item
            {
                scope.aliases.push(alias.ident.clone());
            }
            if presence == Presence::Active
                && let syn::Item::Enum(item) = item
            {
                scope
                    .enums
                    .insert(item.ident.to_string(), Rc::new(item.clone()));
            }
            let named = match item {
                syn::Item::Type(item) => Some((
                    item.ident.to_string(),
                    Binding::Alias(Rc::new((item.generics.clone(), (*item.ty).clone()))),
                )),
                syn::Item::Mod(item) => {
                    let binding = match (module, &item.content) {
                        (Some(parent), Some(_)) => {
                            let mut path = parent.to_vec();
                            path.push(item.ident.to_string());
                            Binding::Module(path)
                        }
                        _ => Binding::External,
                    };
                    Some((item.ident.to_string(), binding))
                }
                syn::Item::Struct(item) => Some((
                    item.ident.to_string(),
                    Binding::Nominal(item.generics.clone()),
                )),
                syn::Item::Enum(item) => Some((
                    item.ident.to_string(),
                    Binding::Nominal(item.generics.clone()),
                )),
                syn::Item::Union(item) => Some((
                    item.ident.to_string(),
                    Binding::Nominal(item.generics.clone()),
                )),
                syn::Item::Trait(item) => Some((
                    item.ident.to_string(),
                    Binding::Nominal(item.generics.clone()),
                )),
                syn::Item::TraitAlias(item) => Some((
                    item.ident.to_string(),
                    Binding::Nominal(item.generics.clone()),
                )),
                syn::Item::Fn(item) => Some((item.sig.ident.to_string(), Binding::Value)),
                syn::Item::Const(item) => Some((
                    item.ident.to_string(),
                    Binding::Constant(Rc::new(((*item.ty).clone(), (*item.expr).clone()))),
                )),
                syn::Item::Static(item) => Some((item.ident.to_string(), Binding::Value)),
                syn::Item::Use(item) => {
                    let mut leaves = HashMap::new();
                    scope.glob |= imports::import_leaves(
                        &item.tree,
                        &[],
                        item.leading_colon.is_some(),
                        &mut leaves,
                    );
                    let mut globs = Vec::new();
                    Self::glob_sources(&item.tree, &mut Vec::new(), &mut globs);
                    scope.glob_sources.extend(globs.into_iter().map(|path| {
                        (
                            path,
                            item.leading_colon.is_some(),
                            presence,
                            item.vis.clone(),
                        )
                    }));
                    for (name, target) in leaves {
                        let binding = target.map_or(Binding::UnknownImport, |(path, absolute)| {
                            Binding::Import(path, absolute)
                        });
                        scope.imports.entry(name).or_default().push(
                            if presence == Presence::Conditional {
                                Binding::Conditional
                            } else {
                                Binding::Visible(Box::new(binding), item.vis.clone())
                            },
                        );
                    }
                    None
                }
                syn::Item::ExternCrate(item) => {
                    let name = item
                        .rename
                        .as_ref()
                        .map(|(_, name)| name)
                        .unwrap_or(&item.ident)
                        .to_string();
                    if item.ident == name && matches!(name.as_str(), "std" | "core") {
                        None
                    } else {
                        Some((name, Binding::External))
                    }
                }
                _ => None,
            };
            if let Some((name, binding)) = named {
                let visibility = match item {
                    syn::Item::Type(item) => &item.vis,
                    syn::Item::Mod(item) => &item.vis,
                    syn::Item::Struct(item) => &item.vis,
                    syn::Item::Enum(item) => &item.vis,
                    syn::Item::Union(item) => &item.vis,
                    syn::Item::Trait(item) => &item.vis,
                    syn::Item::TraitAlias(item) => &item.vis,
                    syn::Item::ExternCrate(item) => &item.vis,
                    syn::Item::Fn(item) => &item.vis,
                    syn::Item::Const(item) => &item.vis,
                    syn::Item::Static(item) => &item.vis,
                    _ => continue,
                };
                let value_only = matches!(&binding, Binding::Value | Binding::Constant(..));
                let binding = if presence == Presence::Conditional {
                    Binding::Conditional
                } else {
                    Binding::Visible(Box::new(binding), visibility.clone())
                };
                if value_only {
                    let duplicate =
                        presence == Presence::Active && !scope.active_values.insert(name.clone());
                    scope
                        .values
                        .entry(name)
                        .and_modify(|previous| {
                            *previous = if duplicate || matches!(previous, Binding::Duplicate) {
                                Binding::Duplicate
                            } else {
                                Binding::Conditional
                            };
                        })
                        .or_insert(binding);
                } else {
                    scope.insert(name, binding);
                }
            }
        }
        scope
    }
}

#[derive(Clone)]
pub(super) struct Catalog {
    owns_crate_root: bool,
    modules: HashMap<Vec<String>, Rc<Scope>>,
    trait_statics: trait_statics::Index,
}
impl Catalog {
    pub(super) fn new(file: &syn::File) -> Rc<Self> {
        Self::with_crate_root(file, true, Presence::Active)
    }
    pub(super) fn module_source(file: &syn::File) -> Rc<Self> {
        Self::module_source_with_presence(file, Presence::Active)
    }
    pub(super) fn module_source_with_presence(file: &syn::File, presence: Presence) -> Rc<Self> {
        Self::with_crate_root(file, false, presence)
    }
    fn with_crate_root(file: &syn::File, owns_crate_root: bool, presence: Presence) -> Rc<Self> {
        fn collect(
            items: &[syn::Item],
            module: Vec<String>,
            modules: &mut HashMap<Vec<String>, Rc<Scope>>,
            conditional: bool,
            no_implicit_prelude: bool,
            trait_statics: &mut trait_statics::Index,
        ) {
            trait_statics.collect(items, &module, conditional);
            let mut scope = Scope::items(items.iter(), Some(&module));
            scope.conditional = conditional;
            scope.no_implicit_prelude = no_implicit_prelude;
            modules.insert(module.clone(), Rc::new(scope));
            for source_item in items {
                let presence = item_presence(source_item);
                if let syn::Item::Mod(item) = source_item
                    && presence != Presence::Inactive
                    && let Some((_, items)) = &item.content
                {
                    let mut child = module.clone();
                    child.push(item.ident.to_string());
                    collect(
                        items,
                        child,
                        modules,
                        conditional || presence == Presence::Conditional,
                        no_implicit_prelude
                            || item
                                .attrs
                                .iter()
                                .any(|attr| attr.path().is_ident("no_implicit_prelude")),
                        trait_statics,
                    );
                }
            }
        }
        let mut modules = HashMap::new();
        let mut trait_statics = trait_statics::Index::default();
        collect(
            &file.items,
            vec![],
            &mut modules,
            presence.and(attrs_presence(&file.attrs)) != Presence::Active,
            file.attrs
                .iter()
                .any(|attr| attr.path().is_ident("no_implicit_prelude")),
            &mut trait_statics,
        );
        Rc::new(Self {
            modules,
            owns_crate_root,
            trait_statics,
        })
    }
}
#[derive(Clone)]
pub(super) struct Frame {
    catalog: Rc<Catalog>,
    module: Vec<String>,
    locals: Vec<Rc<Scope>>,
}
impl Frame {
    pub(super) fn root(catalog: Rc<Catalog>) -> Self {
        Self {
            catalog,
            module: vec![],
            locals: vec![],
        }
    }
    pub(super) fn source_is_active(&self) -> bool {
        self.catalog
            .modules
            .get(&self.module)
            .is_some_and(|scope| !scope.conditional)
    }
    pub(super) fn implicit_prelude(&self) -> bool {
        self.catalog
            .modules
            .get(&self.module)
            .is_some_and(|scope| !scope.no_implicit_prelude)
    }
    pub(super) fn module(&self, name: &str) -> Self {
        let mut module = self.module.clone();
        module.push(name.into());
        Self {
            catalog: self.catalog.clone(),
            module,
            locals: vec![],
        }
    }
    pub(super) fn block(&self, block: &syn::Block) -> Self {
        let scope = Scope::items(
            block.stmts.iter().filter_map(|stmt| {
                if let syn::Stmt::Item(item) = stmt {
                    Some(item)
                } else {
                    None
                }
            }),
            None,
        );
        let mut frame = self.clone();
        frame.locals.push(Rc::new(scope));
        frame
    }
    pub(super) fn generics(&self, generics: &syn::Generics) -> Self {
        let mut scope = Scope::default();
        for parameter in generics.type_params() {
            let binding = match attrs_presence(&parameter.attrs) {
                Presence::Inactive => continue,
                Presence::Conditional => Binding::Conditional,
                Presence::Active => Binding::Generic,
            };
            scope.insert(parameter.ident.to_string(), binding);
        }
        for parameter in generics.lifetimes() {
            scope.lifetimes.insert(
                parameter.lifetime.ident.to_string(),
                attrs_presence(&parameter.attrs),
            );
        }
        let mut frame = self.clone();
        frame.locals.push(Rc::new(scope));
        frame
    }
    fn owns_reference_lifetime(&self, lifetime: &syn::Lifetime) -> bool {
        matches!(lifetime.ident.to_string().as_str(), "static" | "_")
            || self
                .locals
                .iter()
                .rev()
                .find_map(|scope| scope.lifetimes.get(&lifetime.ident.to_string()))
                == Some(&Presence::Active)
    }
    pub(super) fn enter(&self) -> Guard {
        Guard(CURRENT.with(|value| value.replace(Some(self.clone()))))
    }
    fn root_frame(&self) -> Self {
        Self::root(self.catalog.clone())
    }
}
thread_local! { static CURRENT: RefCell<Option<Frame>> = const { RefCell::new(None) }; }
pub(super) struct Guard(Option<Frame>);
impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|value| {
            value.replace(self.0.take());
        });
    }
}
pub(super) fn current() -> Option<Frame> {
    CURRENT.with(|value| value.borrow().clone())
}

/// Variant value lookup is separate from type lookup: enum globs export no types.
pub(super) fn enum_variant(path: &syn::Path) -> Option<(String, syn::Variant, i64)> {
    enum_variants::resolve(path).ok().flatten()
}

pub(super) fn enum_pattern_variant(
    path: &syn::Path,
) -> Result<Option<(String, syn::Variant, i64)>, Failure> {
    enum_variants::resolve(path)
}

fn qualified_enum_variant(path: &syn::Path) -> Option<(String, syn::Variant, i64)> {
    let mut owner = path.clone();
    let member = owner.segments.pop()?.into_value();
    if owner.segments.is_empty() {
        return None;
    }
    owner.segments.pop_punct();
    let typ = syn::Type::Path(syn::TypePath {
        qself: None,
        path: owner.clone(),
    });
    let Typ::Named(identity) = source_signature_type(&typ).ok()? else {
        return None;
    };
    let mut identity = crate::core_ir::source_types::nominal_identity(&identity)?
        .split("::")
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let name = identity.pop()?;
    let frame = current()?;
    let item = frame.catalog.modules.get(&identity)?.enums.get(&name)?;
    let mut tag = 0i64;
    for variant in &item.variants {
        match attrs_presence(&variant.attrs) {
            Presence::Inactive => continue,
            Presence::Conditional => return None,
            Presence::Active => {}
        }
        if let Some((_, value)) = &variant.discriminant {
            tag = crate::core_ir::parse_int_literal(&super::compact_rust_tokens(value))?;
        }
        if variant.ident == member.ident {
            identity.push(name);
            return Some((identity.join("::"), variant.clone(), tag));
        }
        tag = tag.checked_add(1)?;
    }
    None
}

pub(super) fn suspend() -> Guard {
    Guard(CURRENT.with(|value| value.replace(None)))
}
pub(super) fn block_scope(block: &syn::Block) -> Option<Guard> {
    current().map(|frame| frame.block(block).enter())
}
pub(super) fn generic_scope(generics: &syn::Generics) -> Option<Guard> {
    current().map(|frame| frame.generics(generics).enter())
}

#[derive(Clone)]
enum Extent {
    Public,
    Module(Vec<String>),
}
impl Extent {
    fn permits(&self, access: &[String]) -> bool {
        match self {
            Self::Public => true,
            Self::Module(owner) => access.starts_with(owner),
        }
    }
    fn includes(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Public, _) => true,
            (Self::Module(owner), Self::Module(other)) => other.starts_with(owner),
            _ => false,
        }
    }
}
#[derive(Clone)]
struct NominalApplication {
    identity: Option<Vec<String>>,
    type_extent: Extent,
    generics: syn::Generics,
    arguments: usize,
    unsupported_arguments: bool,
    infer_arguments: bool,
}
#[derive(Clone)]
enum NominalObligation {
    Projection,
    Application(NominalApplication),
    Failure(Failure),
}
fn structural_failure(value: Resolved) -> Option<Failure> {
    match value.visibility().0 {
        Resolved::Nominal(obligations) => {
            obligations.into_iter().fold(None, |failure, obligation| {
                let result = match obligation {
                    NominalObligation::Application(application) => {
                        nominal_application(&application)
                    }
                    NominalObligation::Failure(failure) => Err(failure),
                    NominalObligation::Projection => Err(projection_failure()),
                };
                crate::core_ir::type_proofs::prefer_failure(failure, result.err())
            })
        }
        Resolved::Type(_) => None,
        Resolved::Value | Resolved::Constant(_) => Some(Failure::Invalid(
            "source alias names a value instead of a type".into(),
        )),
        _ => Some(unsupported("source alias type is unresolved")),
    }
}
#[derive(Clone)]
struct SourceConstant {
    syntax: Rc<(syn::Type, syn::Expr)>,
    declared: Frame,
    key: Key,
}
#[derive(Clone)]
enum Resolved {
    Dependencies(Box<Resolved>, generic_aliases::Dependencies),
    Visible(Box<Resolved>, Extent),
    Type(Typ),
    Module(Vec<String>),
    Nominal(Vec<NominalObligation>),
    Value,
    Constant(SourceConstant),
    Missing,
}
impl Resolved {
    fn visibility(self) -> (Self, Extent) {
        match self {
            Self::Visible(value, extent) => {
                let (value, _) = value.visibility();
                (value, extent)
            }
            Self::Dependencies(value, _) => value.visibility(),
            value => (value, Extent::Public),
        }
    }
    fn with_visibility(self, extent: Extent) -> Self {
        match self {
            Self::Dependencies(value, dependencies) => {
                Self::Dependencies(Box::new(value.with_visibility(extent)), dependencies)
            }
            Self::Visible(value, _) => value.with_visibility(extent),
            value => Self::Visible(Box::new(value), extent),
        }
    }
}
type Key = (Vec<String>, usize, String);
fn unsupported(message: impl Into<String>) -> Failure {
    Failure::Unsupported(message.into())
}
fn projection_failure() -> Failure {
    unsupported("generic associated projection is unresolved")
}

// Resolution cursors move through modules; access remains the module using the path.
pub(super) fn source_visibility_permits(
    visibility: &str,
    declared: &[String],
    caller: &[String],
) -> Result<bool, Failure> {
    let visibility = syn::parse_str::<syn::Visibility>(visibility)
        .map_err(|_| unsupported("unresolved source function visibility"))?;
    Ok(visibility_extent(&visibility, declared)?.permits(caller))
}

fn visibility_extent(visibility: &syn::Visibility, declared: &[String]) -> Result<Extent, Failure> {
    let owner = match visibility {
        syn::Visibility::Public(_) => return Ok(Extent::Public),
        syn::Visibility::Inherited => declared.to_vec(),
        syn::Visibility::Restricted(restricted) => {
            let parts: Vec<_> = restricted
                .path
                .segments
                .iter()
                .map(|part| part.ident.to_string())
                .collect();
            match parts.first().map(String::as_str) {
                Some("crate") => parts[1..].to_vec(),
                Some("self") => {
                    let mut owner = declared.to_vec();
                    owner.extend_from_slice(&parts[1..]);
                    owner
                }
                Some("super") => {
                    let mut owner = declared.to_vec();
                    let mut index = 0;
                    while parts.get(index).is_some_and(|part| part == "super") {
                        if owner.pop().is_none() {
                            return Err(unsupported("restricted visibility escapes source module"));
                        }
                        index += 1;
                    }
                    owner.extend_from_slice(&parts[index..]);
                    owner
                }
                _ => {
                    return Err(unsupported(
                        "restricted visibility requires source module resolution",
                    ));
                }
            }
        }
    };
    if !declared.starts_with(&owner) {
        return Err(unsupported(
            "restricted visibility is not an enclosing source module",
        ));
    }
    Ok(Extent::Module(owner))
}

#[derive(Clone, Copy)]
struct LookupContext<'a> {
    arguments: Option<(&'a syn::PathArguments, &'a Frame)>,
    caller_active: Option<&'a HashSet<Key>>,
    declaration: bool,
    alias_target: bool,
    source_qualified: bool,
    ordinary_value_owner: bool,
    constant_value: bool,
    trait_position: bool,
    nominal_arguments: usize,
    unsupported_nominal_arguments: bool,
}
impl LookupContext<'_> {
    fn qualified(self) -> Self {
        Self {
            source_qualified: true,
            ..self
        }
    }
}
fn path_type(
    path: &syn::Path,
    frame: &Frame,
    active: &mut HashSet<Key>,
    alias_target: bool,
) -> Result<Resolved, Failure> {
    path_type_with_mode(path, frame, active, alias_target, false, false, false)
}
fn path_type_with_mode(
    path: &syn::Path,
    frame: &Frame,
    active: &mut HashSet<Key>,
    alias_target: bool,
    declaration: bool,
    trait_position: bool,
    ordinary_value_owner: bool,
) -> Result<Resolved, Failure> {
    let _query = generic_aliases::query()?;
    let caller_active = active.clone();
    if path
        .segments
        .iter()
        .take(path.segments.len().saturating_sub(1))
        .any(|segment| has_generic_arguments(&segment.arguments))
    {
        return Err(unsupported(
            "source namespace prefix arguments require associated type resolution",
        ));
    }
    let parts: Vec<_> = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect();
    let generic = path
        .segments
        .iter()
        .any(|part| has_generic_arguments(&part.arguments));
    resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        generic,
        frame,
        &frame.module,
        active,
        LookupContext {
            arguments: path.segments.last().map(|part| (&part.arguments, frame)),
            caller_active: Some(&caller_active),
            declaration,
            alias_target,
            source_qualified: false,
            ordinary_value_owner,
            constant_value: false,
            trait_position,
            nominal_arguments: path
                .segments
                .last()
                .map_or(0, |part| match &part.arguments {
                    syn::PathArguments::AngleBracketed(args) => args.args.len(),
                    _ => 0,
                }),
            unsupported_nominal_arguments: path.segments.last().is_some_and(|part| {
                match &part.arguments {
                    syn::PathArguments::None => false,
                    syn::PathArguments::AngleBracketed(args) => args
                        .args
                        .iter()
                        .any(|arg| !matches!(arg, syn::GenericArgument::Type(_))),
                    _ => true,
                }
            }),
        },
    )
}
// Imports can bring only a value into scope. Probe their actual source target before
// treating their spelling as a conflicting type declaration or a type shadow.
fn scope_binding(
    scope: &Scope,
    name: &str,
    declared: &Frame,
    depth: usize,
    active: &mut HashSet<Key>,
    constant_value: bool,
) -> Result<(Option<Binding>, Option<Binding>), Failure> {
    let mut typ = scope.bindings.get(name).cloned();
    let mut value = scope.values.get(name).cloned();
    if matches!(typ, Some(Binding::Duplicate)) {
        return Ok((typ, value));
    }
    if let Some(imports) = scope.imports.get(name) {
        for import in imports {
            let Binding::Visible(binding, visibility) = import else {
                return Err(unsupported("import presence or namespace is unresolved"));
            };
            let Binding::Import(path, absolute) = binding.as_ref() else {
                return Err(unsupported("import namespace is not proven by source"));
            };
            // A same-named import can name the external crate prelude, which this catalog does not own.
            if !absolute
                && path.first().is_some_and(|root| root == name)
                && !scope.bindings.contains_key(name)
            {
                return Err(unsupported(
                    "import root identity is outside the owned source catalog",
                ));
            }
            let key = (declared.module.clone(), depth, name.to_string());
            if !active.insert(key.clone()) {
                // `mod answer; use answer::answer;` binds a module and a value.
                // Probing the value import's prefix must use the existing module,
                // not recursively probe that same import as a competing type.
                if matches!(&typ, Some(Binding::Visible(binding, _)) if matches!(binding.as_ref(), Binding::Module(_)))
                {
                    continue;
                }
                return Err(Failure::Invalid(format!("cyclic type alias `{name}`")));
            }
            let target = resolve_parts(
                path,
                *absolute,
                false,
                declared,
                &declared.module,
                active,
                LookupContext {
                    arguments: None,
                    caller_active: None,
                    declaration: true,
                    alias_target: true,
                    source_qualified: false,
                    ordinary_value_owner: false,
                    constant_value,
                    trait_position: false,
                    nominal_arguments: 0,
                    unsupported_nominal_arguments: false,
                },
            );
            active.remove(&key);
            let (target, extent) = target?.visibility();
            if !extent.includes(&visibility_extent(visibility, &declared.module)?) {
                return Err(Failure::Invalid(format!(
                    "source import `{name}` widens target visibility"
                )));
            }
            if matches!(target, Resolved::Value | Resolved::Constant(_)) {
                value = Some(import.clone());
            } else if matches!(target, Resolved::Missing) {
                if typ.is_none() {
                    // Preserve the existing ordinary std/core import fallback. An alias RHS
                    // still rejects a missing target after the normal import resolver runs.
                    typ = Some(import.clone());
                } else {
                    return Err(unsupported(format!(
                        "import namespace is unresolved for `{name}`"
                    )));
                }
            } else if typ.is_some() {
                if matches!(typ, Some(Binding::Conditional)) {
                    return Err(unsupported(
                        "source type binding depends on unresolved cfg predicates",
                    ));
                }
                typ = Some(Binding::Duplicate);
            } else {
                typ = Some(import.clone());
            }
        }
    }
    Ok((typ, value))
}

/// Collect declarations, not their runtime carriers: two different source
/// aliases to u32 are still competing glob imports. Cyclic reexports can reach
/// the same declaration repeatedly without creating an ambiguity.
fn glob_candidates(
    scope: &Scope,
    frame: &Frame,
    name: &str,
    access: &[String],
    active: &mut HashSet<Key>,
    seen: &mut HashSet<Vec<String>>,
    candidates: &mut std::collections::BTreeSet<Vec<String>>,
) -> Result<(), Failure> {
    for (path, absolute, presence, visibility) in &scope.glob_sources {
        if !visibility_extent(visibility, &frame.module)?.permits(access) {
            continue;
        }
        if *presence != Presence::Active {
            return Err(unsupported(
                "glob import depends on unresolved cfg predicates",
            ));
        }
        let context = LookupContext {
            arguments: None,
            caller_active: None,
            declaration: true,
            alias_target: false,
            source_qualified: false,
            ordinary_value_owner: false,
            constant_value: false,
            trait_position: false,
            nominal_arguments: 0,
            unsupported_nominal_arguments: false,
        };
        let target = resolve_parts(
            path,
            *absolute,
            false,
            frame,
            &frame.module,
            active,
            context,
        )?
        .visibility()
        .0;
        // An enum glob contributes variants, never type aliases or modules.
        if enum_variants::identity(&target, frame).is_some() {
            continue;
        }
        // Runtime enum owners may have no unapplied storage type. Query their
        // source identity rather than treating a variant-only glob as a module.
        let standard_enum = if matches!(target, Resolved::Missing) {
            let _source = source_signatures::ensure_source_context(frame);
            resolve_parts(
                path,
                *absolute,
                false,
                frame,
                &frame.module,
                active,
                context,
            )?
            .visibility()
            .0
        } else {
            target.clone()
        };
        if matches!(standard_enum,Resolved::Type(Typ::Named(ref name)) if matches!(name.as_str(),"#rust-source-signature:std:option:Option"|"#rust-source-signature:std:result:Result"))
        {
            continue;
        }
        // Ordering is already owned by the atomic frontend. Do not mistake a
        // source module shadowing std/core for that standard enum.
        if matches!(target, Resolved::Missing)
            && matches!(
                path.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                ["std" | "core", "sync", "atomic", "Ordering"]
            )
            && (*absolute
                || matches!(
                    resolve_parts(
                        &path[..1],
                        false,
                        false,
                        frame,
                        &frame.module,
                        active,
                        context
                    )?
                    .visibility()
                    .0,
                    Resolved::Missing
                ))
        {
            continue;
        }
        let Resolved::Module(module) = target else {
            return Err(unsupported(
                "glob source module is not owned by this source",
            ));
        };
        let Some(source) = frame.catalog.modules.get(&module) else {
            return Err(unsupported(
                "glob source module is not owned by this source",
            ));
        };
        if source.conditional {
            return Err(unsupported(
                "glob source depends on unresolved cfg predicates",
            ));
        }
        if !seen.insert(module.clone()) {
            continue;
        }
        let declared = Frame {
            catalog: frame.catalog.clone(),
            module: module.clone(),
            locals: vec![],
        };
        // A private import back through a public reexport is not an export
        // from this module. While its target is being resolved, traversing that
        // same private edge again would manufacture an alias cycle.
        if active.contains(&(module.clone(), 0, name.to_owned()))
            && !source.bindings.contains_key(name)
            && source.imports.get(name).is_some_and(|imports| {
                imports.iter().all(|import| match import {
                    Binding::Visible(_, visibility) => visibility_extent(visibility, &module)
                        .is_ok_and(|extent| !extent.permits(&frame.module)),
                    _ => false,
                })
            })
        {
            continue;
        }
        let (binding, _) = scope_binding(source, name, &declared, 0, active, false)?;
        if let Some(binding) = binding {
            let visible = match binding {
                Binding::Visible(_, visibility) => visibility_extent(&visibility, &module)?,
                Binding::Conditional => {
                    return Err(unsupported(
                        "glob export depends on unresolved cfg predicates",
                    ));
                }
                _ => Extent::Module(module.clone()),
            };
            if visible.permits(&frame.module) {
                candidates.insert(module);
            }
        } else {
            glob_candidates(source, &declared, name, access, active, seen, candidates)?;
        }
    }
    Ok(())
}

fn scope_glob(
    scope: &Scope,
    frame: &Frame,
    parts: &[String],
    generic: bool,
    access: &[String],
    active: &mut HashSet<Key>,
    context: LookupContext<'_>,
) -> Result<Option<Resolved>, Failure> {
    if scope.glob_sources.is_empty()
        || (!context.alias_target
            && !source_signatures::active()
            && !matches!(primitive(parts, generic)?, Resolved::Missing))
    {
        return Ok(None);
    }
    let key = (
        frame.module.clone(),
        frame.locals.len(),
        format!("#glob:{}", parts[0]),
    );
    if !active.insert(key.clone()) {
        return Ok(None);
    }
    let result = (|| {
        let mut candidates = std::collections::BTreeSet::new();
        glob_candidates(
            scope,
            frame,
            &parts[0],
            access,
            active,
            &mut HashSet::new(),
            &mut candidates,
        )?;
        if candidates.len() > 1 {
            return Err(unsupported(format!(
                "ambiguous source glob type `{}`",
                parts[0]
            )));
        }
        let Some(module) = candidates.pop_first() else {
            return Ok(None);
        };
        resolve_parts(
            parts,
            false,
            generic,
            &Frame {
                catalog: frame.catalog.clone(),
                module,
                locals: vec![],
            },
            access,
            active,
            context.qualified(),
        )
        .map(Some)
    })();
    active.remove(&key);
    result
}

fn resolve_parts(
    parts: &[String],
    absolute: bool,
    generic: bool,
    frame: &Frame,
    access: &[String],
    active: &mut HashSet<Key>,
    context: LookupContext,
) -> Result<Resolved, Failure> {
    let _query = generic_aliases::query()?;
    let Some(first) = parts.first() else {
        // `crate`, `self` and `super` can name a module on their own,
        // including the source of a wildcard import.
        return Ok(Resolved::Module(frame.module.clone()));
    };
    if first == "crate" {
        if !frame.catalog.owns_crate_root {
            if context.ordinary_value_owner || context.constant_value {
                return Ok(Resolved::Missing);
            }
            return Err(unsupported(
                "crate root is outside the owned source alias catalog",
            ));
        }
        return resolve_parts(
            &parts[1..],
            false,
            generic,
            &frame.root_frame(),
            access,
            active,
            context.qualified(),
        );
    }
    if first == "self" {
        let mut declared = frame.clone();
        declared.locals.clear();
        return resolve_parts(
            &parts[1..],
            false,
            generic,
            &declared,
            access,
            active,
            context.qualified(),
        );
    }
    if first == "super" {
        let mut parent = frame.clone();
        parent.locals.clear();
        if parent.module.pop().is_none() {
            return Err(unsupported("super alias path escapes the source module"));
        }
        return resolve_parts(
            &parts[1..],
            false,
            generic,
            &parent,
            access,
            active,
            context.qualified(),
        );
    }
    if absolute {
        if !matches!(first.as_str(), "std" | "core" | "alloc") {
            if context.ordinary_value_owner || context.constant_value {
                return Ok(Resolved::Missing);
            }
            return Err(unsupported(
                "external artifact alias identity is not owned by this source",
            ));
        }
        if source_signatures::active()
            && let Some(value) = source_signatures::builtin(parts, context, active)?
        {
            return Ok(value);
        }
        if !source_signatures::active()
            && let Some(value) =
                runtime_map(parts, context, active)?.or(runtime_enum(parts, context, active)?)
        {
            return Ok(value);
        }
        return primitive(parts, generic);
    }
    let module = frame
        .catalog
        .modules
        .get(&frame.module)
        .ok_or_else(|| unsupported("alias module is not owned by this source"))?;
    if module.conditional {
        return Err(unsupported(
            "source module depends on unresolved cfg predicates",
        ));
    }
    let mut selected = None;
    let mut value_only = None;
    for (index, scope) in frame.locals.iter().enumerate().rev() {
        let mut declared = frame.clone();
        declared.locals.truncate(index + 1);
        let (typ, value) = scope_binding(
            scope,
            first,
            &declared,
            index + 1,
            active,
            context.constant_value && parts.len() == 1,
        )?;
        if value_only.is_none() {
            value_only = value.map(|binding| (binding, index + 1));
        }
        if context.constant_value && parts.len() == 1 {
            if let Some(binding) = value_only.clone() {
                selected = Some(binding);
                break;
            }
        } else if let Some(binding) = typ {
            selected = Some((binding, index + 1));
            break;
        } else if let Some(value) =
            scope_glob(scope, &declared, parts, generic, access, active, context)?
        {
            return Ok(value);
        }
    }
    if selected.is_none() {
        let mut declared = frame.clone();
        declared.locals.clear();
        let (typ, value) = scope_binding(
            module,
            first,
            &declared,
            0,
            active,
            context.constant_value && parts.len() == 1,
        )?;
        if context.constant_value && parts.len() == 1 {
            selected = value.map(|binding| (binding, 0));
        } else {
            selected = typ.map(|binding| (binding, 0));
            if value_only.is_none() {
                value_only = value.map(|binding| (binding, 0));
            }
        }
        if selected.is_none()
            && let Some(value) =
                scope_glob(module, &declared, parts, generic, access, active, context)?
        {
            return Ok(value);
        }
    }
    if selected.is_none() && parts.len() == 1 && value_only.is_some() {
        let primitive_type =
            !context.source_qualified && !matches!(primitive(parts, generic)?, Resolved::Missing);
        if !primitive_type {
            selected = value_only;
        }
    }
    if let Some((binding, depth)) = selected {
        let mut declared = frame.clone();
        declared.locals.truncate(depth);
        let (binding, extent) = match binding {
            Binding::Visible(binding, visibility) => {
                let extent = visibility_extent(&visibility, &frame.module)?;
                if !extent.permits(access) {
                    return Err(Failure::Invalid(format!(
                        "private source type binding `{first}`"
                    )));
                }
                (*binding, extent)
            }
            binding => (binding, Extent::Module(frame.module.clone())),
        };
        let carries_visibility = matches!(
            &binding,
            Binding::Alias(..) | Binding::Nominal(..) | Binding::Value | Binding::Constant(..)
        ) || (parts.len() == 1
            && matches!(&binding, Binding::Import(..) | Binding::Module(..)));
        let key = (frame.module.clone(), depth, first.clone());
        let tracked = matches!(&binding, Binding::Import(..));
        if tracked && !active.insert(key.clone()) {
            return Err(Failure::Invalid(format!(
                "cyclic type alias `{}`",
                parts.join("::")
            )));
        }
        let result = match binding {
            Binding::Visible(..) => Err(unsupported("nested source visibility binding")),
            Binding::Conditional => Err(unsupported(
                "source type binding depends on unresolved cfg predicates",
            )),
            Binding::UnknownImport => Err(unsupported("import namespace is not proven by source")),
            Binding::Value => Ok(Resolved::Value),
            Binding::Constant(syntax) if parts.len() == 1 && !generic => {
                Ok(Resolved::Constant(SourceConstant {
                    syntax,
                    declared: declared.clone(),
                    key: key.clone(),
                }))
            }
            Binding::Constant(..) => Ok(Resolved::Missing),
            Binding::Duplicate => Err(Failure::Invalid(format!(
                "duplicate type binding `{first}`"
            ))),
            Binding::Generic if parts.len() == 1 && !generic => {
                Ok(Resolved::Type(Typ::Generic(first.clone())))
            }
            Binding::Generic if parts.len() == 1 => Err(Failure::Invalid(
                "type parameter requires zero generic arguments".into(),
            )),
            Binding::Generic => Err(unsupported("generic type owner requires monomorphization")),
            Binding::Nominal(generics) if parts.len() == 1 && source_signatures::active() => {
                source_signatures::nominal(
                    (depth == 0).then(|| {
                        let mut identity = frame.module.clone();
                        identity.push(first.clone());
                        identity
                    }),
                    &generics,
                    context,
                    active,
                )
            }
            Binding::Nominal(generics) if parts.len() == 1 => {
                Ok(Resolved::Nominal(vec![NominalObligation::Application(
                    NominalApplication {
                        identity: (depth == 0).then(|| {
                            let mut identity = frame.module.clone();
                            identity.push(first.clone());
                            identity
                        }),
                        type_extent: extent.clone(),
                        generics,
                        arguments: context.nominal_arguments,
                        unsupported_arguments: context.unsupported_nominal_arguments,
                        infer_arguments: context.ordinary_value_owner,
                    },
                )]))
            }
            Binding::Nominal(..) => Ok(Resolved::Missing),
            Binding::External if context.ordinary_value_owner || context.constant_value => {
                Ok(Resolved::Missing)
            }
            Binding::External => Err(unsupported(
                "external artifact alias identity is not owned by this source",
            )),
            Binding::Module(module) => {
                if parts.len() == 1 {
                    return Ok(Resolved::Module(module).with_visibility(extent));
                }
                let module = Frame {
                    catalog: frame.catalog.clone(),
                    module,
                    locals: vec![],
                };
                resolve_parts(
                    &parts[1..],
                    false,
                    generic,
                    &module,
                    access,
                    active,
                    context.qualified(),
                )
            }
            Binding::Import(path, rooted) => {
                // The imported prefix is validated where `use` is declared. Caller-selected
                // members of an imported module retain the caller's access rights.
                let target = resolve_parts(
                    &path,
                    rooted,
                    if parts.len() == 1 { generic } else { false },
                    &declared,
                    &declared.module,
                    active,
                    LookupContext {
                        arguments: context.arguments,
                        caller_active: context.caller_active,
                        declaration: context.declaration,
                        alias_target: true,
                        source_qualified: false,
                        ordinary_value_owner: context.ordinary_value_owner && parts.len() == 1,
                        constant_value: context.constant_value && parts.len() == 1,
                        trait_position: context.trait_position,
                        nominal_arguments: context.nominal_arguments,
                        unsupported_nominal_arguments: context.unsupported_nominal_arguments,
                    },
                )?;
                let dependencies = target.dependencies();
                let (target, target_extent) = target.visibility();
                if !target_extent.includes(&extent) {
                    return Err(Failure::Invalid(format!(
                        "source import `{first}` widens target visibility"
                    )));
                }
                if parts.len() > 1 {
                    match target {
                        Resolved::Module(module) => resolve_parts(
                            &parts[1..],
                            false,
                            generic,
                            &Frame {
                                catalog: frame.catalog.clone(),
                                module,
                                locals: vec![],
                            },
                            access,
                            active,
                            context.qualified(),
                        ),
                        Resolved::Missing
                            if matches!(path.first().map(String::as_str), Some("std" | "core")) =>
                        {
                            Ok(Resolved::Missing)
                        }
                        _ => Err(unsupported(
                            "projected imported type requires owned module identity",
                        )),
                    }
                } else {
                    match target {
                        Resolved::Missing
                            if !matches!(
                                path.first().map(String::as_str),
                                Some("std" | "core")
                            ) =>
                        {
                            Err(unsupported(format!("unresolved imported type `{first}`")))
                        }
                        result => Ok(result.with_dependencies(dependencies)),
                    }
                }
            }
            Binding::Parameter(value) => {
                if parts.len() == 1 && generic {
                    Err(Failure::Invalid(
                        "type parameter requires zero generic arguments".into(),
                    ))
                } else if parts.len() == 1 {
                    Ok(value.as_ref().clone())
                } else {
                    Ok(Resolved::Nominal(vec![NominalObligation::Projection])
                        .with_dependencies(value.dependencies()))
                }
            }
            Binding::Alias(_) if context.trait_position => Err(Failure::Invalid(
                "type alias cannot be used in trait position".into(),
            )),
            Binding::Alias(syntax) => generic_aliases::expand(
                &syntax,
                first,
                &key,
                &declared,
                access,
                active,
                context,
                parts.len() == 1,
            ),
        };
        if tracked {
            active.remove(&key);
        }
        return result.map(|value| {
            if carries_visibility {
                value.with_visibility(extent)
            } else {
                value
            }
        });
    }
    if context.source_qualified {
        return Ok(Resolved::Missing);
    }
    let result = if source_signatures::active() {
        match source_signatures::builtin(parts, context, active)? {
            Some(value) => value,
            None => primitive(parts, generic)?,
        }
    } else {
        match runtime_map(parts, context, active)?.or(runtime_enum(parts, context, active)?) {
            Some(value) => value,
            None => primitive(parts, generic)?,
        }
    };
    Ok(result)
}
fn has_generic_arguments(arguments: &syn::PathArguments) -> bool {
    match arguments {
        syn::PathArguments::None => false,
        syn::PathArguments::AngleBracketed(arguments) => !arguments.args.is_empty(),
        syn::PathArguments::Parenthesized(_) => true,
    }
}

fn runtime_map(
    parts: &[String],
    context: LookupContext<'_>,
    active: &mut HashSet<Key>,
) -> Result<Option<Resolved>, Failure> {
    if !matches!(
        parts
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["std" | "alloc", "collections", "BTreeMap"]
            | ["std" | "alloc", "collections", "btree_map", "BTreeMap"]
    ) {
        return Ok(None);
    }
    let Some((arguments, frame)) = context.arguments else {
        return Ok(None);
    };
    if context.ordinary_value_owner && matches!(arguments, syn::PathArguments::None) {
        return Ok(None);
    }
    let _source_context = source_signatures::ensure_source_context(frame);
    let resolved = source_signatures::builtin(parts, context, active)?
        .ok_or_else(|| unsupported("unresolved ordered map type"))?;
    let dependencies = resolved.dependencies();
    let source = source_signatures::exact(resolved)?;
    let storage = super::collections::storage(&source)
        .ok_or_else(|| unsupported("ordered map arguments lack owned storage"))?;
    Ok(Some(
        Resolved::Type(storage).with_dependencies(dependencies),
    ))
}

fn nominal_runtime_type(obligations: &[NominalObligation]) -> Option<Typ> {
    let [NominalObligation::Application(application)] = obligations else {
        return None;
    };
    nominal_application(application).ok()?;
    let typ = Typ::Named(application.identity.as_ref()?.join("::"));
    // A source owner must not become a builtin merely by sharing its spelling.
    (typ.canonical() == typ).then_some(typ)
}

/// Result uses the error slot; Option and Box retain owned storage identities.
/// Resolve aliases after lexical lookup, retaining every source dependency.
fn runtime_enum(
    parts: &[String],
    context: LookupContext<'_>,
    active: &mut HashSet<Key>,
) -> Result<Option<Resolved>, Failure> {
    let (name, arity) = match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["Result"] | ["std" | "core", "result", "Result"] => ("Result", 2),
        ["Option"] | ["std" | "core", "option", "Option"] => ("Option", 1),
        ["Box"] | ["std" | "alloc", "boxed", "Box"] => ("Box", 1),
        _ => return Ok(None),
    };
    let Some((arguments, frame)) = context.arguments else {
        return Ok(None);
    };
    // Value-owner paths such as Result::is_ok infer their type parameters;
    // they are not type annotations missing a pair of arguments.
    if context.ordinary_value_owner && matches!(arguments, syn::PathArguments::None) {
        return Ok(None);
    }
    let syn::PathArguments::AngleBracketed(arguments) = arguments else {
        return Err(Failure::Invalid(format!(
            "{name} requires {arity} type arguments"
        )));
    };
    if arguments.args.len() != arity {
        return Err(Failure::Invalid(format!(
            "{name} requires {arity} type arguments"
        )));
    }
    let mut dependencies = generic_aliases::Dependencies::default();
    let mut payload = None;
    for (index, argument) in arguments.args.iter().enumerate() {
        let syn::GenericArgument::Type(argument) = argument else {
            return Err(Failure::Invalid(format!("{name} requires type arguments")));
        };
        let value = generic_aliases::rhs_type(argument, frame, active)?;
        dependencies.merge(value.dependencies());
        if let Some(failure) = structural_failure(value.clone()) {
            return Err(failure);
        }
        if index == 0 {
            let typ = match value.visibility().0 {
                Resolved::Type(typ) => typ,
                Resolved::Nominal(obligations) => {
                    nominal_runtime_type(&obligations).ok_or_else(|| {
                        unsupported(format!("{name} payload owner is not source-owned"))
                    })?
                }
                _ => {
                    return Err(unsupported(format!(
                        "{name} payload requires an exact runtime type"
                    )));
                }
            };
            payload = Some(typ);
        }
    }
    Ok(payload.map(|typ| {
        Resolved::Type(if name == "Option" {
            crate::core_ir::rust_options::typ(typ)
        } else if name == "Box" {
            crate::core_ir::rust_boxes::typ(typ)
        } else {
            typ
        })
        .with_dependencies(dependencies)
    }))
}

fn primitive(parts: &[String], generic: bool) -> Result<Resolved, Failure> {
    let builtin = match parts {
        [name] => Some(name.as_str()),
        [root, primitive, name]
            if matches!(root.as_str(), "std" | "core") && primitive == "primitive" =>
        {
            Some(name.as_str())
        }
        [root, module, name]
            if matches!(root.as_str(), "std" | "alloc")
                && module == "string"
                && name == "String" =>
        {
            Some("String")
        }
        _ => None,
    };
    let path = syn::parse_str::<syn::Path>(&parts.join("::"))
        .map_err(|_| unsupported("unresolved type path"))?;
    let integer = rust_integer_path(&path);
    if generic
        && (integer.is_some() || matches!(builtin, Some("bool" | "f32" | "f64" | "str" | "String")))
    {
        return Err(Failure::Invalid(
            "primitive type requires zero generic arguments".into(),
        ));
    }
    match builtin {
        Some("String" | "str") => return Ok(Resolved::Type(Typ::String)),
        Some("bool") => return Ok(Resolved::Type(Typ::Bool)),
        Some("f32") => {
            return Ok(Resolved::Type(crate::core_ir::rust_floats::typ(
                crate::core_ir::rust_floats::Kind::F32,
            )));
        }
        Some("f64") => {
            return Ok(Resolved::Type(crate::core_ir::rust_floats::typ(
                crate::core_ir::rust_floats::Kind::F64,
            )));
        }
        _ => {}
    }
    match integer {
        Some("i128" | "u128") => Err(unsupported(
            "primitive alias exceeds supported native integer width",
        )),
        Some(kind) => Ok(Resolved::Type(Typ::Named(format!("#rust-integer-{kind}")))),
        None => Ok(Resolved::Missing),
    }
}

pub(super) fn resolve_in(frame: &Frame, path: &syn::Path) -> Result<Option<Typ>, Failure> {
    match path_type(path, frame, &mut HashSet::new(), false)?
        .visibility()
        .0
    {
        Resolved::Type(typ) => Ok(Some(typ)),
        Resolved::Nominal(obligations) => {
            let failure = obligations
                .iter()
                .filter_map(|obligation| match obligation {
                    NominalObligation::Failure(failure) => Some(failure.clone()),
                    NominalObligation::Projection => Some(projection_failure()),
                    _ => None,
                })
                .fold(None, |failure, next| {
                    crate::core_ir::type_proofs::prefer_failure(failure, Some(next))
                });
            failure.map_or_else(|| Ok(nominal_runtime_type(&obligations)), Err)
        }
        Resolved::Value | Resolved::Constant(_) | Resolved::Missing | Resolved::Module(_) => {
            Ok(None)
        }
        Resolved::Visible(..) | Resolved::Dependencies(..) => {
            Err(unsupported("nested source resolution wrapper"))
        }
    }
}
/// Query exact source type identity before ABI/runtime mapping can erase it.
pub(super) fn source_signature_type(source: &syn::Type) -> Result<Typ, Failure> {
    let frame = current().ok_or_else(|| unsupported("source signature catalog is not active"))?;
    source_signatures::source_type(source, &frame)
}

/// Preserve explicit payload arguments and aliases after lexical ownership lookup.
pub(super) fn box_constructor(path: &syn::Path) -> Option<String> {
    let frame = current()?;
    let _source = source_signatures::ensure_source_context(&frame);
    let parts = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>();
    let context = LookupContext {
        arguments: path.segments.last().and_then(|part| {
            (!matches!(part.arguments, syn::PathArguments::None))
                .then_some((&part.arguments, &frame))
        }),
        caller_active: None,
        declaration: true,
        alias_target: false,
        source_qualified: false,
        ordinary_value_owner: true,
        constant_value: false,
        trait_position: false,
        nominal_arguments: 0,
        unsupported_nominal_arguments: false,
    };
    let result = resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        context.arguments.is_some(),
        &frame,
        &frame.module,
        &mut HashSet::new(),
        context,
    )
    .ok()?;
    let Resolved::Type(source) = result.visibility().0 else {
        return None;
    };
    if matches!(&source, Typ::Named(name) if name == "#rust-source-signature:std:boxed:Box") {
        return Some(crate::core_ir::rust_boxes::NEW.into());
    }
    let Typ::Named(owner) = super::collections::storage(&source)? else {
        return None;
    };
    crate::core_ir::rust_boxes::payload(&owner).map(crate::core_ir::rust_boxes::typed_constructor)
}
pub(super) fn source_callable_signature(
    signature: &syn::Signature,
) -> Result<crate::core_ir::rust_callables::Signature, Failure> {
    if signature.asyncness.is_some() {
        return Err(unsupported(
            "async function item requires an owned future ABI",
        ));
    }
    let _scope = generic_scope(&signature.generics);
    let frame = current().ok_or_else(|| unsupported("source callable catalog is not active"))?;
    source_signatures::declaration(signature, &frame)
}

/// Resolve a source module in the type namespace, independently of value-only imports.
pub(super) fn source_module_path(path: &syn::Path) -> Result<Option<Vec<String>>, Failure> {
    let Some(frame) = current() else {
        return Ok(None);
    };
    if path
        .segments
        .iter()
        .any(|part| has_generic_arguments(&part.arguments))
    {
        return Ok(None);
    }
    let parts = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>();
    // A namespace query does not apply a generic enum/collection constructor.
    // In particular, an absent variant in Option::* must not demand Option<T>.
    match resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        false,
        &frame,
        &frame.module,
        &mut HashSet::new(),
        LookupContext {
            arguments: None,
            caller_active: None,
            declaration: false,
            alias_target: false,
            source_qualified: false,
            ordinary_value_owner: true,
            constant_value: false,
            trait_position: false,
            nominal_arguments: 0,
            unsupported_nominal_arguments: false,
        },
    )?
    .visibility()
    .0
    {
        Resolved::Module(module) => Ok(Some(module)),
        _ => Ok(None),
    }
}

/// Exact current source module identity; an owned crate root is Some(empty).
pub(super) fn current_module_path() -> Option<Vec<String>> {
    current().map(|frame| frame.module)
}

/// Source-owned implicit associated dispatch; unrelated ordinary paths stay untouched.
pub(super) fn resolve_trait_static(path: &syn::Path) -> Option<Result<String, Failure>> {
    let frame = current()?;
    frame.catalog.trait_statics.resolve(path, &frame)
}

pub(super) fn resolve_trait_ufcs(
    qself: &syn::QSelf,
    path: &syn::Path,
) -> Option<Result<String, Failure>> {
    let frame = current()?;
    frame
        .catalog
        .trait_statics
        .resolve_ufcs(qself, path, &frame)
}

/// Validate the type-vs-trait syntactic role through imports and module paths.
/// External/unowned trait identities retain their existing unsupported boundary.
pub(super) fn trait_path_failure(path: &syn::Path) -> Option<Failure> {
    let frame = current()?;
    path_type_with_mode(path, &frame, &mut HashSet::new(), false, false, true, false)
        .err()
        .filter(Failure::is_invalid)
}

/// Ordinary call/value paths may belong to lazily loaded modules outside this catalog.
/// Alias RHS and import target recursion intentionally retain their strict contexts.
pub(super) fn resolve_value_owner(path: &syn::Path) -> Result<Option<Typ>, Failure> {
    let Some(frame) = current() else {
        return Ok(None);
    };
    let parts = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>();
    let generic = path
        .segments
        .iter()
        .any(|part| has_generic_arguments(&part.arguments));
    let caller_active = HashSet::new();
    let result = resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        generic,
        &frame,
        &frame.module,
        &mut HashSet::new(),
        LookupContext {
            arguments: path
                .segments
                .last()
                .map(|segment| (&segment.arguments, &frame)),
            caller_active: Some(&caller_active),
            declaration: false,
            alias_target: false,
            source_qualified: false,
            ordinary_value_owner: true,
            constant_value: false,
            trait_position: false,
            nominal_arguments: 0,
            unsupported_nominal_arguments: false,
        },
    )?;
    match result.visibility().0 {
        Resolved::Type(typ) => Ok(Some(typ)),
        Resolved::Nominal(obligations) => {
            let failure = obligations
                .into_iter()
                .filter_map(|obligation| match obligation {
                    NominalObligation::Failure(failure) => Some(failure),
                    NominalObligation::Projection => Some(projection_failure()),
                    _ => None,
                })
                .fold(None, |failure, next| {
                    crate::core_ir::type_proofs::prefer_failure(failure, Some(next))
                });
            failure.map_or(Ok(None), Err)
        }
        _ => Ok(None),
    }
}

thread_local! {
    static LENGTH_QUERY_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Preserve exact source constant types until the complete count is checked as usize.
/// Namespace ownership and lexical locals are resolved before the existing bounded
/// constant evaluator runs; no source declaration is rewritten by this query.
pub(super) fn usize_length(
    expression: &syn::Expr,
    shadows: &HashSet<String>,
) -> Result<Option<usize>, Failure> {
    use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
    use crate::integer_semantics::IntegerSemantics;
    struct QueryDepth;
    impl Drop for QueryDepth {
        fn drop(&mut self) {
            LENGTH_QUERY_DEPTH.with(|depth| depth.set(depth.get() - 1));
        }
    }
    if LENGTH_QUERY_DEPTH.with(|depth| depth.get() >= 64) {
        return Err(unsupported(
            "fixed-array length source nesting limit exceeded",
        ));
    }
    LENGTH_QUERY_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let _query_depth = QueryDepth;
    struct Constants {
        declarations: Vec<Decl>,
        names: HashMap<Key, String>,
        active: HashSet<Key>,
        fuel: usize,
        unresolved: bool,
    }
    impl Constants {
        fn expression(
            &mut self,
            source: &syn::Expr,
            frame: Option<&Frame>,
            shadows: &HashSet<String>,
            depth: usize,
        ) -> Result<Expr, Failure> {
            if depth >= 64 || self.fuel == 0 {
                return Err(unsupported(
                    "fixed-array length constant resolution limit exceeded",
                ));
            }
            self.fuel -= 1;
            let _scope = frame.map(Frame::enter);
            // Outer runtime bindings never become locals inside a constant initializer.
            let _length_shadows = super::LengthShadowScope::new(&HashMap::new());
            let mut value = super::lower_expr_with_types(source, &mut HashMap::new());
            let mut names = HashSet::new();
            crate::core_bindings::reserve_expr(&value, &mut names);
            crate::core_bindings::normalize_initializer_with_references(
                &mut value,
                &names,
                &HashMap::new(),
            );
            let mut callees = HashSet::new();
            crate::core_opt::map_expr_mut(&mut value, &mut |expr| {
                if let Expr::Call { callee, .. } = expr
                    && let Expr::Ident(name) = callee.as_ref()
                {
                    callees.insert(name.clone());
                }
            });
            self.unresolved |= callees.iter().any(|name| {
                !matches!(
                    name.as_str(),
                    crate::core_ir::INLINE_CONST | crate::core_ir::ARRAY_REPEAT
                )
            });
            let mut failure = None;
            crate::core_opt::map_expr_mut(&mut value, &mut |expr| {
                let Expr::Ident(name) = expr else { return };
                if name.starts_with("in-local-") || callees.contains(name) {
                    return;
                }
                let Ok(path) = syn::parse_str::<syn::ExprPath>(name) else {
                    return;
                };
                let result = self.reference(&path, frame, shadows, depth);
                match result {
                    Ok(Some(name)) => *expr = Expr::Ident(name),
                    Ok(None) => self.unresolved = true,
                    Err(error) => {
                        failure =
                            crate::core_ir::type_proofs::prefer_failure(failure.take(), Some(error))
                    }
                }
            });
            if let Some(failure) = failure {
                return Err(failure);
            }
            Ok(value)
        }
        fn reference(
            &mut self,
            value: &syn::ExprPath,
            frame: Option<&Frame>,
            shadows: &HashSet<String>,
            depth: usize,
        ) -> Result<Option<String>, Failure> {
            if self.fuel == 0 {
                return Err(unsupported(
                    "fixed-array length constant resolution limit exceeded",
                ));
            }
            self.fuel -= 1;
            if value.qself.is_some()
                || value
                    .path
                    .segments
                    .iter()
                    .any(|segment| has_generic_arguments(&segment.arguments))
            {
                return Ok(None);
            }
            if value.path.leading_colon.is_none()
                && value.path.segments.len() == 1
                && shadows.contains(&value.path.segments[0].ident.to_string())
            {
                return Err(Failure::Invalid(
                    "fixed-array length refers to a runtime local value".into(),
                ));
            }
            let Some(frame) = frame else { return Ok(None) };
            let parts = value
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            let resolved = resolve_parts(
                &parts,
                value.path.leading_colon.is_some(),
                false,
                frame,
                &frame.module,
                &mut HashSet::new(),
                LookupContext {
                    arguments: None,
                    caller_active: None,
                    declaration: false,
                    alias_target: false,
                    source_qualified: false,
                    ordinary_value_owner: false,
                    constant_value: true,
                    trait_position: false,
                    nominal_arguments: 0,
                    unsupported_nominal_arguments: false,
                },
            )?
            .visibility()
            .0;
            let Resolved::Constant(constant) = resolved else {
                return if matches!(resolved, Resolved::Value) {
                    Err(Failure::Invalid(
                        "fixed-array length requires a constant value".into(),
                    ))
                } else {
                    Ok(None)
                };
            };
            if let Some(name) = self.names.get(&constant.key) {
                return Ok(Some(name.clone()));
            }
            if !self.active.insert(constant.key.clone()) {
                return Err(Failure::Invalid(
                    "cyclic fixed-array length constant".into(),
                ));
            }
            let result = (|| {
                let _scope = constant.declared.enter();
                let typ = super::map_type(&constant.syntax.0);
                if let Some(failure) = crate::core_ir::type_proofs::failure_in_type(&typ) {
                    return Err(failure);
                }
                let init = self.expression(
                    &constant.syntax.1,
                    Some(&constant.declared),
                    &HashSet::new(),
                    depth + 1,
                )?;
                let name = format!("#array-length-constant-{}", self.declarations.len());
                self.declarations.push(Decl::Global {
                    name: name.clone(),
                    typ,
                    init: Some(Box::new(init)),
                    mutable: false,
                    constant_value: true,
                });
                self.names.insert(constant.key.clone(), name.clone());
                Ok(Some(name))
            })();
            self.active.remove(&constant.key);
            result
        }
    }
    let mut constants = Constants {
        declarations: vec![],
        names: HashMap::new(),
        active: HashSet::new(),
        fuel: 16384,
        unresolved: false,
    };
    let value = constants.expression(expression, current().as_ref(), shadows, 0)?;
    let checker = crate::typecheck::TypeChecker::new();
    let env = constants
        .declarations
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    let facts = checker.collect_decl_facts(&constants.declarations);
    match checker.expr_type(&value, &facts, &env) {
        Some(typ)
            if matches!(
                typ.canonical(),
                Typ::Bool | Typ::Float | Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Void
            ) =>
        {
            return Err(Failure::Invalid(
                "fixed-array length must have usize type".into(),
            ));
        }
        _ => {}
    }
    constants.declarations.push(Decl::Function {
        name: "#array-length".into(),
        params: vec![],
        ret: Typ::Named("#rust-integer-usize".into()),
        body: vec![Stmt::Return(Some(Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INLINE_CONST.into())),
            args: vec![value],
        }))],
        type_params: vec![],
    });
    let constants_unresolved = constants.unresolved;
    let mut module = UnifiedModule::new(constants.declarations);
    let semantics = IntegerSemantics::Word64;
    crate::typecheck::rust_integers::normalize(&checker, &mut module, semantics);
    let proof = checker
        .typed_proof_nodes(&module.decls)
        .into_values()
        .fold(None, |left, right| {
            crate::core_ir::type_proofs::prefer_failure(left, Some(right))
        });
    if let Some(proof) = proof {
        return Err(proof);
    }
    if constants_unresolved {
        return Ok(None);
    }
    for declaration in &module.decls {
        let value = match declaration {
            Decl::Global {
                init: Some(value), ..
            } => value.as_ref(),
            Decl::Function { body, .. } => {
                let [Stmt::Return(Some(value))] = body.as_slice() else {
                    return Ok(None);
                };
                value
            }
            _ => continue,
        };
        let mut value = value.clone();
        crate::core_opt::map_expr_mut(&mut value, &mut |expr| {
            if let Expr::Call { callee, args } = expr
                && matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::INLINE_CONST)
                && let [value] = args.as_slice()
            {
                *expr = value.clone();
            }
        });
        // Evaluation can discard cold branches, but their source types still matter.
        if let Some(failure) = checker.constant_value_type_failure(&value, &facts, &env) {
            return Err(failure);
        }
    }
    // Declarations are topologically ordered by the source binder. Memoize
    // evaluated values before shared folding, avoiding repeated A+A DAG walks.
    let mut values: HashMap<String, Expr> = HashMap::new();
    for declaration in &mut module.decls {
        let Decl::Global {
            name,
            init: Some(init),
            ..
        } = declaration
        else {
            continue;
        };
        crate::core_opt::map_expr_mut(init, &mut |expr| {
            if let Expr::Ident(name) = expr
                && let Some(value) = values.get(name)
            {
                *expr = value.clone();
            }
        });
        let value = match crate::lower_core::evaluate_pure_constant(init, semantics) {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(None),
            Err(crate::lower_core::ConstantEvalError::Arithmetic) => {
                return Err(Failure::Invalid(
                    "fixed-array length constant arithmetic failed".into(),
                ));
            }
        };
        values.insert(name.clone(), value.clone());
        **init = value;
    }
    crate::lower_core::fold_static_initializers_with_semantics(&mut module.decls, semantics);
    crate::lower_core::lower_named_constants_with_semantics(&mut module.decls, semantics);
    let Some(Decl::Function { body, .. }) = module.decls.last() else {
        return Ok(None);
    };
    let [Stmt::Return(Some(value))] = body.as_slice() else {
        return Ok(None);
    };
    match crate::lower_core::evaluate_pure_constant(value, semantics) {
        Ok(Some(Expr::IntLit(length))) => Ok(usize::try_from(length as u64).ok()),
        Ok(_) => Ok(None),
        Err(crate::lower_core::ConstantEvalError::Arithmetic) => Err(Failure::Invalid(
            "fixed-array length constant arithmetic failed".into(),
        )),
    }
}

fn nominal_application(application: &NominalApplication) -> Result<(), Failure> {
    let generics = &application.generics;
    let parameters: Vec<_> = generics
        .params
        .iter()
        .filter(|parameter| generic_presence(parameter) != Presence::Inactive)
        .collect();
    let (minimum, maximum) = generic_arity(generics);
    if application.arguments < minimum || application.arguments > maximum {
        // An expression such as Row::new(value) may infer Row<T>; a type
        // annotation or an alias RHS must still supply its required arguments.
        if application.infer_arguments && application.arguments == 0 {
            return Err(unsupported(
                "nominal value owner requires generic argument inference",
            ));
        }
        return Err(Failure::Invalid(format!(
            "nominal type requires {minimum}..={maximum} type arguments, found {}",
            application.arguments
        )));
    }
    if parameters
        .iter()
        .any(|parameter| generic_presence(parameter) == Presence::Conditional)
    {
        return Err(unsupported(
            "nominal generic parameters depend on unresolved cfg predicates",
        ));
    }
    if application.unsupported_arguments
        || parameters
            .iter()
            .any(|parameter| !matches!(parameter, syn::GenericParam::Type(_)))
    {
        return Err(unsupported(
            "nominal lifetime/const arguments require source type specialization",
        ));
    }
    if maximum != 0 || generics.where_clause.is_some() {
        return Err(unsupported(
            "generic nominal arguments require source type specialization",
        ));
    }
    Ok(())
}

pub(super) fn validate_alias_declaration(path: &syn::Path, frame: &Frame) -> Result<(), Failure> {
    let value = path_type_with_mode(path, frame, &mut HashSet::new(), false, true, false, false)?;
    structural_failure(value).map_or(Ok(()), Err)
}

/// Whether an explicit source type argument has an owned type declaration.
/// Presence, visibility and recursive alias failures remain authoritative.
pub(super) fn source_type_owned(path: &syn::Path) -> Result<bool, Failure> {
    let Some(frame) = current() else {
        return Ok(false);
    };
    match path_type(path, &frame, &mut HashSet::new(), false)?
        .visibility()
        .0
    {
        Resolved::Type(_) => Ok(true),
        Resolved::Nominal(applications) => {
            let mut failure = None;
            for obligation in applications {
                let result = (|| -> Result<(), Failure> {
                    let application = match obligation {
                        NominalObligation::Application(application) => application,
                        NominalObligation::Failure(failure) => return Err(failure),
                        NominalObligation::Projection => return Err(projection_failure()),
                    };
                    nominal_application(&application)
                })();
                failure = crate::core_ir::type_proofs::prefer_failure(failure, result.err());
            }
            failure.map_or(Ok(true), Err)
        }
        _ => Ok(false),
    }
}

pub(super) fn resolve(path: &syn::Path) -> Result<Option<Typ>, Failure> {
    current().map_or(Ok(None), |frame| resolve_in(&frame, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(source: &str) -> Frame {
        Frame::root(Catalog::new(&syn::parse_file(source).unwrap()))
    }
    fn query(frame: &Frame, path: &str) -> Result<Option<Typ>, Failure> {
        resolve_in(frame, &syn::parse_str(path).unwrap())
    }
    fn integer(frame: &Frame, path: &str, kind: &str) {
        assert_eq!(
            query(frame, path).unwrap(),
            Some(Typ::Named(format!("#rust-integer-{kind}")))
        );
    }
    #[test]
    fn current_module_identity_distinguishes_absence_root_and_nested_scope() {
        {
            let _suspended = suspend();
            assert_eq!(current_module_path(), None);
        }
        let root = frame("mod outer{mod child{}}");
        {
            let _scope = root.clone().enter();
            assert_eq!(current_module_path(), Some(vec![]));
        }
        {
            let _scope = root.module("outer").module("child").enter();
            assert_eq!(
                current_module_path(),
                Some(vec!["outer".into(), "child".into()])
            );
        }
    }
    #[test]
    fn structural_alias_argument_ownership_retains_unresolved_children() {
        let file = syn::parse_file("fn value(){}mod child{}type Ref=&NoSuchType;type Array=[NoSuchType;2];type Tuple=(u8,NoSuchType);type Module=&child;type Value=&value;type Mixed=(&NoSuchType,&value);type Valid=(&u8,[u16;2]);").unwrap();
        let _scope = Frame::root(Catalog::new(&file)).enter();
        for name in ["Ref", "Array", "Tuple", "Module"] {
            assert!(
                !source_type_owned(&syn::parse_str(name).unwrap())
                    .unwrap_err()
                    .is_invalid(),
                "{name}"
            );
            assert!(matches!(
                resolve(&syn::parse_str(name).unwrap()),
                Err(Failure::Unsupported(_))
            ));
        }
        for name in ["Value", "Mixed"] {
            assert!(
                source_type_owned(&syn::parse_str(name).unwrap())
                    .unwrap_err()
                    .is_invalid(),
                "{name}"
            );
            assert!(matches!(
                resolve(&syn::parse_str(name).unwrap()),
                Err(Failure::Invalid(_))
            ));
        }
        assert!(source_type_owned(&syn::parse_str("Valid").unwrap()).unwrap());
    }

    #[test]
    fn nominal_type_argument_ownership_retains_arity_through_aliases_and_containers() {
        let file = syn::parse_file("struct Row<T>(T);struct Plain;struct Defaulted<T=u8>(T);mod child{pub struct Public<T>(T);struct Private;}use child::Public as Imported;type Missing=Row;type Applied=Row<u8>;type Nested=(&Row,[Plain;1]);type Mixed=(Row<u8>,Row);type PlainAlias=Plain;").unwrap();
        let _scope = Frame::root(Catalog::new(&file)).enter();
        for path in [
            "Row",
            "Row<u8,u16>",
            "Plain<u8>",
            "Imported",
            "Missing",
            "Nested",
            "child::Private",
        ] {
            let failure = source_type_owned(&syn::parse_str(path).unwrap()).unwrap_err();
            assert!(failure.is_invalid(), "{path}: {failure:?}");
        }
        for path in ["Row<u8>", "Imported<u8>", "Applied", "Defaulted"] {
            let failure = source_type_owned(&syn::parse_str(path).unwrap()).unwrap_err();
            assert!(!failure.is_invalid(), "{path}: {failure:?}");
        }
        for path in ["Plain", "PlainAlias"] {
            assert!(
                source_type_owned(&syn::parse_str(path).unwrap()).unwrap(),
                "{path}"
            );
        }
        // Annotation resolution remains the preexisting nominal None boundary.
        assert_eq!(resolve(&syn::parse_str("Row").unwrap()).unwrap(), None);
        assert_eq!(resolve(&syn::parse_str("Row<u8>").unwrap()).unwrap(), None);
    }

    #[test]
    fn explicit_type_argument_ownership_uses_source_namespace_and_visibility() {
        let file = syn::parse_file("struct Row;mod child{pub struct Public;struct Private;}use child::Public as Imported;fn value(){}type Broken=Broken;").unwrap();
        let _scope = Frame::root(Catalog::new(&file)).enter();
        for name in ["Row", "child::Public", "Imported", "u8"] {
            assert!(
                source_type_owned(&syn::parse_str(name).unwrap()).unwrap(),
                "{name}"
            );
        }
        for name in ["Missing", "value", "child"] {
            assert!(
                !source_type_owned(&syn::parse_str(name).unwrap()).unwrap(),
                "{name}"
            );
        }
        for name in ["child::Private", "Broken"] {
            assert!(
                source_type_owned(&syn::parse_str(name).unwrap())
                    .unwrap_err()
                    .is_invalid(),
                "{name}"
            );
        }
        let generics: syn::Generics = syn::parse_quote!(<T>);
        let _generic_scope = generic_scope(&generics);
        assert!(source_type_owned(&syn::parse_str("T").unwrap()).unwrap());
    }

    #[test]
    fn ordinary_value_owners_defer_modules_but_keep_owned_alias_obligations() {
        let file = syn::parse_file("mod child;type Unknown=crate::other::Value;type Cycle=Cycle;type Duplicate=u8;type Duplicate=u16;").unwrap();
        let _scope = Frame::root(Catalog::module_source(&file)).enter();
        for owner in ["crate::shared", "child"] {
            let path = syn::parse_str(owner).unwrap();
            assert!(resolve_value_owner(&path).unwrap().is_none(), "{owner}");
            assert!(resolve(&path).is_err(), "strict annotation owner {owner}");
        }
        let unknown = resolve_value_owner(&syn::parse_str("Unknown").unwrap()).unwrap_err();
        assert!(!unknown.is_invalid());
        for owner in ["Cycle", "Duplicate"] {
            assert!(
                resolve_value_owner(&syn::parse_str(owner).unwrap())
                    .unwrap_err()
                    .is_invalid()
            );
        }
    }

    #[test]
    fn cfg_presence_evaluates_only_source_known_boolean_combinators() {
        let file: syn::File = syn::parse_quote! {
            #[cfg(any())] type Disabled = u8;
            #[cfg(not(any()))] type Enabled = u8;
            #[cfg(all(feature = "unknown", any()))] type StillDisabled = u8;
            #[cfg(target_os = "macos")] type UnknownTarget = u8;
            #[cfg_attr(all(), cfg(any()))] type AttributeDisabled = u8;
            #[cfg_attr(feature = "unknown", cfg(any()))] type AttributeUnknown = u8;
            #[cfg_attr(feature = "unknown", allow(dead_code))] type Unchanged = u8;
            #[cfg_attr(all(),)] type EmptyAttributes = u8;
        };
        let expected = [
            Presence::Inactive,
            Presence::Active,
            Presence::Inactive,
            Presence::Conditional,
            Presence::Inactive,
            Presence::Conditional,
            Presence::Active,
            Presence::Active,
        ];
        assert!(
            file.items
                .iter()
                .zip(expected)
                .all(|(item, expected)| item_presence(item) == expected)
        );
    }

    #[test]
    fn definite_cfg_alternatives_and_disabled_imports_keep_active_alias_identity() {
        for source in [
            "#[cfg(any())]type Byte=u16;#[cfg(not(any()))]type Byte=u8;",
            "mod values{pub type Byte=u16;}#[cfg(any())]use values::Byte;type Byte=u8;",
            "#[cfg(any())]type Byte=Byte;#[cfg(all())]type Byte=u8;",
        ] {
            integer(&frame(source), "Byte", "u8");
        }
        let root = frame(
            "#[cfg(any())]mod selected{pub type Byte=u16;}#[cfg(all())]mod selected{pub type Byte=u8;}",
        );
        integer(&root, "selected::Byte", "u8");
    }

    #[test]
    fn unknown_cfg_is_unsupported_without_hiding_real_active_duplicates_or_cycles() {
        let root = frame(
            r#"#[cfg(target_os="macos")]type Byte=u8;#[cfg(not(target_os="macos"))]type Byte=u16;"#,
        );
        assert!(matches!(query(&root, "Byte"), Err(Failure::Unsupported(_))));
        let root = frame(r#"#[cfg(feature="unknown")]type Byte=u8;type Byte=u16;type Byte=u32;"#);
        assert!(matches!(query(&root, "Byte"), Err(Failure::Invalid(_))));
        let root = frame("type A=B;type B=A;");
        assert!(matches!(query(&root, "A"), Err(Failure::Invalid(_))));
        let root = frame(r#"#[cfg(feature="unknown")]mod conditional{pub type Byte=u8;}"#);
        assert!(matches!(
            query(&root, "conditional::Byte"),
            Err(Failure::Unsupported(_))
        ));
    }

    #[test]
    fn value_only_imports_share_names_without_shadowing_type_bindings() {
        for source in [
            "mod values{pub fn Byte(){}}use values::Byte;type Byte=u8;",
            "mod values{pub const Byte:i64=7;}use values::Byte;type Byte=u8;",
            "mod values{pub static Byte:i64=7;}use values::Byte;type Byte=u8;",
            "mod values{pub fn Byte(){}}mod forward{pub use crate::values::Byte;}use forward::Byte;type Byte=u8;",
        ] {
            integer(&frame(source), "Byte", "u8");
        }
        let root = frame("mod values{pub fn Byte(){}}type Byte=u8;");
        let block: syn::Block = syn::parse_quote!({
            use crate::values::Byte;
        });
        integer(&root.block(&block), "Byte", "u8");
        let collision = frame("mod types{pub type Byte=u16;}use types::Byte;type Byte=u8;");
        assert!(matches!(
            query(&collision, "Byte"),
            Err(Failure::Invalid(_))
        ));
        let unknown = frame("use external::Byte;type Byte=u8;");
        assert!(matches!(
            query(&unknown, "Byte"),
            Err(Failure::Unsupported(_))
        ));
        let member = frame("enum Kind{Byte}use Kind::Byte;type Byte=u8;");
        assert!(matches!(
            query(&member, "Byte"),
            Err(Failure::Unsupported(_))
        ));
    }

    #[test]
    fn alias_type_lookup_keeps_primitive_and_value_namespaces_separate() {
        integer(&frame("fn u8(){}type Byte=u8;"), "Byte", "u8");
        for source in [
            "fn Value(){}type Byte=Value;",
            "mod values{pub fn Value(){}}use values::Value;type Byte=Value;",
        ] {
            assert!(
                matches!(query(&frame(source), "Byte"), Err(Failure::Invalid(message)) if message.contains("names a value")),
                "{source}"
            );
        }
    }

    #[test]
    fn same_named_external_crate_imports_do_not_invent_alias_cycles() {
        for source in ["use external;", "use external::module as external;"] {
            assert!(
                matches!(
                    query(&frame(source), "external"),
                    Err(Failure::Unsupported(_))
                ),
                "{source}"
            );
        }
        assert!(matches!(
            query(&frame("use B as A;use A as B;"), "A"),
            Err(Failure::Invalid(_))
        ));
        assert!(matches!(
            query(&frame("type A=B;type B=A;"), "A"),
            Err(Failure::Invalid(_))
        ));
    }

    #[test]
    fn isolated_module_sources_do_not_reinterpret_crate_imports_as_local_cycles() {
        let file = syn::parse_file(
            "use crate::parser_registry;use crate::Algorithm;type Local=u8;type A=B;type B=A;",
        )
        .unwrap();
        let module = Frame::root(Catalog::module_source(&file));
        assert!(matches!(
            query(&module, "parser_registry"),
            Err(Failure::Unsupported(_))
        ));
        assert!(matches!(
            query(&module, "Algorithm"),
            Err(Failure::Unsupported(_))
        ));
        integer(&module, "Local", "u8");
        assert!(matches!(query(&module, "A"), Err(Failure::Invalid(_))));
        let rooted = frame("use crate::Alias as Alias;");
        assert!(matches!(query(&rooted, "Alias"), Err(Failure::Invalid(_))));
        let file = syn::parse_file(
            "type Good=u16;mod child{pub type Local=super::Good;pub type Global=crate::Good;}",
        )
        .unwrap();
        let unit = Frame::root(Catalog::module_source(&file));
        integer(&unit, "child::Local", "u16");
        assert!(matches!(
            query(&unit, "child::Global"),
            Err(Failure::Unsupported(_))
        ));
    }
    #[test]
    fn source_qualified_missing_members_never_fall_back_to_bare_primitives() {
        let root = frame(
            "mod empty{}mod owned{pub type Byte=u8;pub use ::core::primitive::u16 as Imported;}type Crate=crate::u8;type Local=self::u8;type Module=empty::u8;",
        );
        for name in ["Crate", "Local", "Module"] {
            assert!(
                matches!(query(&root, name), Err(Failure::Unsupported(_))),
                "{name}"
            );
        }
        for name in ["crate::u8", "self::u8", "empty::u8"] {
            assert_eq!(query(&root, name).unwrap(), None, "{name}");
        }
        integer(&root, "owned::Byte", "u8");
        integer(&root, "owned::Imported", "u16");
        integer(&root, "u8", "u8");
        integer(&root, "core::primitive::u8", "u8");
    }
    #[test]
    fn unknown_globs_cannot_establish_owned_primitive_alias_identity() {
        let root = frame(
            "use external::*;type Byte=u8;type Relative=core::primitive::u8;type Absolute=::core::primitive::u8;use core::primitive::u8 as Imported;use ::core::primitive::u8 as AbsoluteImport;",
        );
        for name in ["Byte", "Relative", "Imported"] {
            assert!(
                matches!(query(&root, name), Err(Failure::Unsupported(_))),
                "{name}"
            );
        }
        integer(&root, "Absolute", "u8");
        integer(&root, "AbsoluteImport", "u8");
        integer(&root, "u8", "u8");
        let scoped = frame("type Byte=u8;");
        let block: syn::Block = syn::parse_quote!({
            use external::*;
            type Local = u8;
        });
        let local = scoped.block(&block);
        assert!(matches!(
            query(&local, "Byte"),
            Err(Failure::Unsupported(_))
        ));
        integer(&local, "self::Byte", "u8");
        assert!(matches!(
            query(&local, "Local"),
            Err(Failure::Unsupported(_))
        ));
        let known = frame(
            "mod first{pub use crate::second::*;}mod second{pub use crate::first::*;pub struct Other;}use first::*;type Byte=u8;",
        );
        integer(&known, "Byte", "u8");
        integer(
            &frame("mod owned{pub type u8=u16;}use owned::*;type Byte=u8;"),
            "Byte",
            "u16",
        );
        for source in [
            "mod owned{pub use external::*;}use owned::*;type Byte=u8;",
            "mod owned{pub use external::Unknown as u8;}use owned::*;type Byte=u8;",
            "mod owned{}#[cfg(unknown)]use owned::*;type Byte=u8;",
            "mod owned{}use crate::{owned::*,external::*};type Byte=u8;",
        ] {
            assert!(
                matches!(query(&frame(source), "Byte"), Err(Failure::Unsupported(_))),
                "{source}"
            );
        }
    }
    #[test]
    fn owned_globs_resolve_unique_visible_aliases_in_their_declaration_scope() {
        for source in [
            "mod owned{pub type Word=u32;}use owned::*;type Selected=Word;",
            "mod owned{pub type Word=u32;}mod export{pub use crate::owned::*;}use export::*;type Selected=Word;",
            "pub type Word=u32;mod inner{use super::*;pub type Selected=Word;}type Selected=inner::Selected;",
            "pub type Word=u32;mod inner{use crate::*;pub type Selected=Word;}type Selected=inner::Selected;",
            "mod first{pub use crate::second::*;}mod second{pub use crate::first::*;pub type Word=u32;}use first::*;type Selected=Word;",
            "mod owned{pub type Word=u32;}mod hidden{type Word=u8;}use owned::*;use hidden::*;type Selected=Word;",
            "mod owned{pub type Word=u32;}mod outer{pub use crate::owned::*;}use owned::*;use outer::*;type Selected=Word;",
            "pub use common::*;mod common{pub use inner::*;mod inner{pub use path::*;pub use iter::*;mod path{pub type Word=u32;}mod iter{use crate::Word;pub type Selected=Word;}}}",
        ] {
            integer(&frame(source), "Selected", "u32");
        }
        let root = frame("type Word=u8;mod owned{pub type Word=u32;}");
        let block: syn::Block = syn::parse_quote!({
            use crate::owned::*;
            type Selected = Word;
        });
        integer(&root.block(&block), "Selected", "u32");
        integer(
            &frame(
                "mod a{pub type Word=u8;}mod b{pub type Word=u32;}use a::*;use b::*;use b::Word;type Selected=Word;",
            ),
            "Selected",
            "u32",
        );
        for source in [
            "mod a{pub type Word=u8;}mod b{pub type Word=u32;}use a::*;use b::*;type Selected=Word;",
            "mod a{pub type Word=u32;}mod b{pub type Word=u32;}use a::*;use b::*;type Selected=Word;",
            "mod owned{type Word=u32;}use owned::*;type Selected=Word;",
            "mod owned{pub type Word=u32;}use owned::*;use external::*;type Selected=Word;",
            "mod a{pub use inner::*;mod inner{type Word=u32;pub type Alias=crate::a::Word;}}type Selected=a::Alias;",
        ] {
            assert!(
                matches!(
                    query(&frame(source), "Selected"),
                    Err(Failure::Unsupported(_))
                ),
                "{source}"
            );
        }
    }
    #[test]
    fn unowned_standard_module_aliases_and_expression_inference_remain_unowned() {
        let root = frame(
            "use std::sync::atomic::Ordering::*;use std::sync::atomic as atom;use core::sync::atomic as core_atom;use external::module as external_module;type Recursive=Vec<Recursive>;",
        );
        assert_eq!(query(&root, "atom::AtomicU32").unwrap(), None);
        assert_eq!(query(&root, "core_atom::AtomicU32").unwrap(), None);
        assert_eq!(query(&root, "std::sync::atomic::AtomicU32").unwrap(), None);
        assert_eq!(query(&root, "Vec<_>").unwrap(), None);
        integer(
            &frame(
                "mod std{pub mod sync{pub mod atomic{pub mod Ordering{pub type u8=u16;}}}}use std::sync::atomic::Ordering::*;type Byte=u8;",
            ),
            "Byte",
            "u16",
        );
        assert!(matches!(
            query(&root, "external_module::AtomicU32"),
            Err(Failure::Unsupported(_))
        ));
        assert!(matches!(
            query(&root, "Recursive"),
            Err(Failure::Invalid(_))
        ));
    }
    #[test]
    fn reexports_cannot_widen_source_visibility_but_type_aliases_can_hide_helpers() {
        let invalid = frame(
            "mod hidden{type Secret=u8;pub use self::Secret as Public;pub(crate) use self::Secret as Crate;}pub use hidden::Public as Chain;",
        );
        for path in ["hidden::Public", "hidden::Crate", "Chain"] {
            assert!(
                matches!(query(&invalid, path), Err(Failure::Invalid(_))),
                "{path}"
            );
        }
        let valid = frame(
            "type Root=u8;pub(crate) use self::Root as Crate;mod hidden{type Secret=u16;pub type Public=Secret;pub mod nested{pub(in crate::hidden) type Scoped=u32;}pub(super) use self::nested::Scoped as Parent;}",
        );
        integer(&valid, "Crate", "u8");
        integer(&valid, "hidden::Public", "u16");
        assert!(matches!(
            query(&valid, "hidden::Parent"),
            Err(Failure::Invalid(_))
        ));
    }
    #[test]
    fn imported_module_suffixes_keep_callers_visibility() {
        let root = frame(
            "pub mod outer{type Secret=u8;pub type Public=u16;pub mod bridge{pub use crate::outer as Parent;}}use outer::bridge::Parent as Imported;",
        );
        for path in ["outer::bridge::Parent::Secret", "Imported::Secret"] {
            assert!(
                matches!(query(&root, path), Err(Failure::Invalid(_))),
                "{path}"
            );
        }
        integer(&root, "outer::bridge::Parent::Public", "u16");
        integer(&root, "Imported::Public", "u16");
    }
    #[test]
    fn source_visibility_uses_access_module_not_resolution_cursor() {
        let root = frame(
            "mod hidden{type Private=u8;pub type Public=Private;pub(crate) type Crate=u16;pub(super) type Parent=u32;pub mod child{pub type Descendant=super::Private;}}mod outer{mod hidden{pub type Byte=u8;}}mod exports{pub type Byte=u8;use self::Byte as PrivateImport;}use hidden::Public as RootAlias;",
        );
        for path in [
            "hidden::Private",
            "outer::hidden::Byte",
            "exports::PrivateImport",
        ] {
            assert!(
                matches!(query(&root, path), Err(Failure::Invalid(_))),
                "{path}"
            );
        }
        integer(&root, "hidden::Public", "u8");
        integer(&root, "RootAlias", "u8");
        integer(&root, "hidden::Crate", "u16");
        integer(&root, "hidden::Parent", "u32");
        integer(&root, "hidden::child::Descendant", "u8");
        integer(
            &root.module("hidden").module("child"),
            "super::Private",
            "u8",
        );
    }
    #[test]
    fn forward_aliases_resolve_in_declaration_scope_through_modules_and_reexports() {
        let root = frame(
            "type First=Next;type Next=u16;mod inner{pub type Local=super::First;pub use self::Local as Export;}use crate::inner::Export as Reexport;",
        );
        integer(&root, "First", "u16");
        integer(&root, "inner::Local", "u16");
        integer(&root, "Reexport", "u16");
        integer(&root.module("inner"), "self::Local", "u16");
        let block: syn::Block = syn::parse_quote!({
            type Next = u8;
        });
        let local = root.block(&block);
        integer(&local, "Next", "u8");
        integer(&local, "First", "u16");
        integer(&local, "crate::Next", "u16");
    }
    #[test]
    fn block_aliases_use_forward_bindings_and_generic_parameters_block_outer_aliases() {
        let root = frame("type Byte=u8;struct Nominal{}type Decoy=Nominal;");
        let block: syn::Block = syn::parse_quote!({
            type Byte = Later;
            type Later = u32;
        });
        integer(&root.block(&block), "Byte", "u32");
        let generics: syn::Generics = syn::parse_quote!(<Byte>);
        assert_eq!(
            query(&root.generics(&generics), "Byte").unwrap(),
            Some(Typ::Generic("Byte".into()))
        );
        assert_eq!(
            query(&root, "Decoy").unwrap(),
            Some(Typ::Named("Nominal".into()))
        );
        assert_eq!(query(&root, "Nominal::u8").unwrap(), None);
    }
    #[test]
    fn only_requested_cycles_and_duplicate_bindings_are_invalid() {
        let root = frame("type A=B;type B=A;type Good=u8;type Duplicate=u8;type Duplicate=u16;");
        integer(&root, "Good", "u8");
        assert!(matches!(query(&root, "A"), Err(Failure::Invalid(_))));
        assert!(matches!(
            query(&root, "Duplicate"),
            Err(Failure::Invalid(_))
        ));
        let recursive = frame("type A=Vec<A>;");
        assert!(matches!(query(&recursive, "A"), Err(Failure::Invalid(_))));
    }
    #[test]
    fn external_and_generic_aliases_are_not_guessed_from_leaf_names() {
        let root = frame(
            "use artifact::Byte;type Generic<T>=T;mod nominal{pub struct u64{}}type Decoy=nominal::u64;",
        );
        assert!(matches!(query(&root, "Byte"), Err(Failure::Unsupported(_))));
        integer(&root, "Generic<u8>", "u8");
        assert_eq!(
            query(&root, "Decoy").unwrap(),
            Some(Typ::Named("nominal::u64".into()))
        );
        assert!(matches!(query(&root, "::u8"), Err(Failure::Unsupported(_))));
        integer(&root, "::core::primitive::u8", "u8");
    }
    #[test]
    fn source_usize_lengths_follow_owned_constants_and_lexical_definition_scope() {
        let root = frame(
            "const A:usize=1;const COUNT:usize=A+2;mod sizes{pub const A:usize=2;pub const COUNT:usize=A+1;}use sizes as alias;use sizes::COUNT as Imported;type Size=usize;const TYPED:Size=3;const BYTE:u8=3;const CAST:usize=BYTE as usize;",
        );
        let _scope = root.enter();
        for source in [
            "COUNT",
            "alias::COUNT",
            "Imported",
            "TYPED",
            "CAST",
            "BYTE as usize",
            "1+2",
            "{let a=1;a+2}",
            "if true{3}else{2}",
            "if true{3}else{1/0}",
            "if true{3}else{usize::MAX+1}",
            "{let mut i=0;while i<3{i+=1;}i}",
        ] {
            assert_eq!(
                usize_length(&syn::parse_str(source).unwrap(), &HashSet::new()),
                Ok(Some(3)),
                "{source}"
            );
        }
    }
    #[test]
    fn source_usize_lengths_preserve_invalidity_and_defer_unknowns() {
        let root = frame(
            "const BYTE:u8=3;const A:usize=B;const B:usize=A;#[cfg(feature=\"x\")]const MAYBE:usize=3;const DUP:usize=1;const DUP:usize=2;",
        );
        let _scope = root.enter();
        for source in [
            "BYTE",
            "A",
            "DUP",
            "3u8",
            "1-2",
            "1/0",
            "if true { 3 } else { false }",
            "if 1 { 3 } else { 3 }",
        ] {
            assert!(
                matches!(
                    usize_length(&syn::parse_str(source).unwrap(), &HashSet::new()),
                    Err(Failure::Invalid(_))
                ),
                "{source}"
            );
        }
        assert!(matches!(
            usize_length(&syn::parse_quote!(MAYBE), &HashSet::new()),
            Err(Failure::Unsupported(_))
        ));
        assert_eq!(
            usize_length(&syn::parse_quote!(UNKNOWN), &HashSet::new()),
            Ok(None)
        );
        assert_eq!(
            usize_length(
                &syn::parse_quote!(if true { 3 } else { UNKNOWN }),
                &HashSet::new()
            ),
            Ok(None)
        );
        assert!(matches!(
            usize_length(&syn::parse_quote!(BYTE), &HashSet::from(["BYTE".into()])),
            Err(Failure::Invalid(_))
        ));
    }
    #[test]
    fn source_usize_lengths_defer_missing_nominal_schemas() {
        let root = frame("struct Pair{x:usize}const P:Pair=Pair{x:3};");
        let _scope = root.enter();
        assert!(matches!(
            usize_length(&syn::parse_quote!(P.x), &HashSet::new()),
            Err(Failure::Unsupported(_))
        ));
    }
    #[test]
    fn source_usize_constant_dags_are_memoized_and_depth_is_bounded() {
        let mut source = "const C0:usize=0;".to_string();
        for index in 1..24 {
            source.push_str(&format!(
                "const C{index}:usize=C{}+C{};",
                index - 1,
                index - 1
            ));
        }
        let root = frame(&source);
        let _scope = root.enter();
        assert_eq!(
            usize_length(&syn::parse_quote!(C23), &HashSet::new()),
            Ok(Some(0))
        );
        let mut source = "const D0:usize=0;".to_string();
        for index in 1..72 {
            source.push_str(&format!("const D{index}:usize=D{};", index - 1));
        }
        let deep = frame(&source);
        let _scope = deep.enter();
        assert!(matches!(
            usize_length(&syn::parse_quote!(D71), &HashSet::new()),
            Err(Failure::Unsupported(_))
        ));
    }
}
