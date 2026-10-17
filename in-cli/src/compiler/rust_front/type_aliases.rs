//! Source-owned primitive alias proofs. Nominal layout/alias lowering stays with the frontend.
use super::{imports, rust_integer_path};
use crate::core_ir::{Typ, type_proofs::ResolutionFailure};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

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
    attrs.iter().fold(Presence::Active, |value, attr| {
        value.and(meta_presence(&attr.meta))
    })
}

type Failure = ResolutionFailure;
#[derive(Clone)]
enum Binding {
    Visible(Box<Binding>, syn::Visibility),
    Alias(syn::Type, bool),
    Import(Vec<String>, bool),
    Module(Vec<String>),
    External,
    Nominal(syn::Generics),
    Generic,
    Value,
    Conditional,
    UnknownImport,
    Duplicate,
}
#[derive(Default)]
struct Scope {
    bindings: HashMap<String, Binding>,
    imports: HashMap<String, Vec<Binding>>,
    values: HashMap<String, Binding>,
    active_types: HashSet<String>,
    glob: bool,
    conditional: bool,
}
impl Scope {
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
            let named = match item {
                syn::Item::Type(item) => Some((
                    item.ident.to_string(),
                    Binding::Alias(
                        (*item.ty).clone(),
                        !item.generics.params.is_empty() || item.generics.where_clause.is_some(),
                    ),
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
                syn::Item::Const(item) => Some((item.ident.to_string(), Binding::Value)),
                syn::Item::Static(item) => Some((item.ident.to_string(), Binding::Value)),
                syn::Item::Use(item) => {
                    let mut leaves = HashMap::new();
                    scope.glob |= imports::import_leaves(
                        &item.tree,
                        &[],
                        item.leading_colon.is_some(),
                        &mut leaves,
                    );
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
                let value_only = matches!(&binding, Binding::Value);
                let binding = if presence == Presence::Conditional {
                    Binding::Conditional
                } else {
                    Binding::Visible(Box::new(binding), visibility.clone())
                };
                if value_only {
                    scope.values.insert(name, binding);
                } else {
                    scope.insert(name, binding);
                }
            }
        }
        scope
    }
}

pub(super) struct Catalog {
    owns_crate_root: bool,
    modules: HashMap<Vec<String>, Rc<Scope>>,
}
impl Catalog {
    pub(super) fn new(file: &syn::File) -> Rc<Self> {
        Self::with_crate_root(file, true)
    }
    pub(super) fn module_source(file: &syn::File) -> Rc<Self> {
        Self::with_crate_root(file, false)
    }
    fn with_crate_root(file: &syn::File, owns_crate_root: bool) -> Rc<Self> {
        fn collect(
            items: &[syn::Item],
            module: Vec<String>,
            modules: &mut HashMap<Vec<String>, Rc<Scope>>,
            conditional: bool,
        ) {
            let mut scope = Scope::items(items.iter(), Some(&module));
            scope.conditional = conditional;
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
                    );
                }
            }
        }
        let mut modules = HashMap::new();
        collect(&file.items, vec![], &mut modules, false);
        Rc::new(Self {
            modules,
            owns_crate_root,
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
            scope.insert(parameter.ident.to_string(), Binding::Generic);
        }
        let mut frame = self.clone();
        frame.locals.push(Rc::new(scope));
        frame
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
struct NominalApplication {
    generics: syn::Generics,
    arguments: usize,
    unsupported_arguments: bool,
}
enum NominalObligation {
    Application(NominalApplication),
    Failure(Failure),
}
fn structural_obligations(value: Resolved) -> Vec<NominalObligation> {
    match value.visibility().0 {
        Resolved::Nominal(obligations) => obligations,
        Resolved::Type(_) => vec![],
        Resolved::Value => vec![NominalObligation::Failure(Failure::Invalid(
            "structural source alias names a value instead of a type".into(),
        ))],
        Resolved::Missing | Resolved::Module(_) => vec![NominalObligation::Failure(unsupported(
            "structural source alias child type is unresolved",
        ))],
        Resolved::Visible(..) => vec![NominalObligation::Failure(unsupported(
            "nested structural source visibility result",
        ))],
    }
}
enum Resolved {
    Visible(Box<Resolved>, Extent),
    Type(Typ),
    Module(Vec<String>),
    Nominal(Vec<NominalObligation>),
    Value,
    Missing,
}
impl Resolved {
    fn visibility(self) -> (Self, Extent) {
        match self {
            Self::Visible(value, extent) => (*value, extent),
            value => (value, Extent::Public),
        }
    }
    fn with_visibility(self, extent: Extent) -> Self {
        let (value, _) = self.visibility();
        Self::Visible(Box::new(value), extent)
    }
}
type Key = (Vec<String>, usize, String);
fn unsupported(message: impl Into<String>) -> Failure {
    Failure::Unsupported(message.into())
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

fn rhs_type(ty: &syn::Type, frame: &Frame, active: &mut HashSet<Key>) -> Result<Resolved, Failure> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => {
            // Only an owned alias RHS requires recursive cycle checks. Ordinary expression
            // type arguments may contain inference placeholders such as Vec::<_>.
            // Visit generic leaves for definite cycles, even though generic alias application is deferred.
            for part in &path.path.segments {
                if let syn::PathArguments::AngleBracketed(arguments) = &part.arguments {
                    for argument in &arguments.args {
                        if let syn::GenericArgument::Type(ty) = argument {
                            rhs_type(ty, frame, active)?;
                        }
                    }
                }
            }
            path_type(&path.path, frame, active, true)
        }
        syn::Type::Reference(value) => Ok(Resolved::Nominal(structural_obligations(rhs_type(
            &value.elem,
            frame,
            active,
        )?))),
        syn::Type::Array(value) => Ok(Resolved::Nominal(structural_obligations(rhs_type(
            &value.elem,
            frame,
            active,
        )?))),
        syn::Type::Slice(value) => Ok(Resolved::Nominal(structural_obligations(rhs_type(
            &value.elem,
            frame,
            active,
        )?))),
        syn::Type::Tuple(value) => {
            let mut applications = Vec::new();
            for item in &value.elems {
                applications.extend(structural_obligations(rhs_type(item, frame, active)?));
            }
            Ok(Resolved::Nominal(applications))
        }
        syn::Type::Paren(value) => rhs_type(&value.elem, frame, active),
        syn::Type::Group(value) => rhs_type(&value.elem, frame, active),
        _ => Err(unsupported(
            "alias target requires unsupported type resolution",
        )),
    }
}
#[derive(Clone, Copy)]
struct LookupContext {
    alias_target: bool,
    source_qualified: bool,
    ordinary_value_owner: bool,
    nominal_arguments: usize,
    unsupported_nominal_arguments: bool,
}
impl LookupContext {
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
    let parts: Vec<_> = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect();
    let generic = path
        .segments
        .iter()
        .any(|part| !matches!(part.arguments, syn::PathArguments::None));
    resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        generic,
        frame,
        &frame.module,
        active,
        LookupContext {
            alias_target,
            source_qualified: false,
            ordinary_value_owner: false,
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
) -> Result<(Option<Binding>, Option<Binding>), Failure> {
    let mut typ = scope.bindings.get(name).cloned();
    let mut value = scope.values.get(name).cloned();
    if matches!(typ, Some(Binding::Duplicate)) {
        return Ok((typ, value));
    }
    for import in scope.imports.get(name).into_iter().flatten() {
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
                alias_target: true,
                source_qualified: false,
                ordinary_value_owner: false,
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
        if matches!(target, Resolved::Value) {
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
    Ok((typ, value))
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
    let Some(first) = parts.first() else {
        return Ok(Resolved::Missing);
    };
    if first == "crate" {
        if !frame.catalog.owns_crate_root {
            if context.ordinary_value_owner {
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
        if !matches!(first.as_str(), "std" | "core") {
            if context.ordinary_value_owner {
                return Ok(Resolved::Missing);
            }
            return Err(unsupported(
                "external artifact alias identity is not owned by this source",
            ));
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
        let (typ, value) = scope_binding(scope, first, &declared, index + 1, active)?;
        if value_only.is_none() {
            value_only = value.map(|binding| (binding, index + 1));
        }
        if let Some(binding) = typ {
            selected = Some((binding, index + 1));
            break;
        }
    }
    if selected.is_none() {
        let mut declared = frame.clone();
        declared.locals.clear();
        let (typ, value) = scope_binding(module, first, &declared, 0, active)?;
        selected = typ.map(|binding| (binding, 0));
        if value_only.is_none() {
            value_only = value.map(|binding| (binding, 0));
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
            Binding::Alias(..) | Binding::Nominal(..) | Binding::Value
        ) || (parts.len() == 1
            && matches!(&binding, Binding::Import(..) | Binding::Module(..)));
        let key = (frame.module.clone(), depth, first.clone());
        let tracked = matches!(&binding, Binding::Alias(..) | Binding::Import(..));
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
            Binding::Duplicate => Err(Failure::Invalid(format!(
                "duplicate type binding `{first}`"
            ))),
            Binding::Generic if parts.len() == 1 && !generic => {
                Ok(Resolved::Type(Typ::Generic(first.clone())))
            }
            Binding::Generic => Err(unsupported("generic type owner requires monomorphization")),
            Binding::Nominal(generics) if parts.len() == 1 => {
                Ok(Resolved::Nominal(vec![NominalObligation::Application(
                    NominalApplication {
                        generics,
                        arguments: context.nominal_arguments,
                        unsupported_arguments: context.unsupported_nominal_arguments,
                    },
                )]))
            }
            Binding::Nominal(..) => Ok(Resolved::Missing),
            Binding::External if context.ordinary_value_owner => Ok(Resolved::Missing),
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
                        alias_target: true,
                        source_qualified: false,
                        ordinary_value_owner: false,
                        nominal_arguments: context.nominal_arguments,
                        unsupported_nominal_arguments: context.unsupported_nominal_arguments,
                    },
                )?;
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
                        result => Ok(result),
                    }
                }
            }
            Binding::Alias(ty, parameters) => {
                if generic || parameters || parts.len() != 1 {
                    Err(unsupported(format!(
                        "generic or projected type alias `{first}`"
                    )))
                } else {
                    match rhs_type(&ty, &declared, active)?.visibility().0 {
                        Resolved::Value => Err(Failure::Invalid(format!(
                            "source alias `{first}` names a value instead of a type"
                        ))),
                        Resolved::Missing => Err(unsupported(format!(
                            "unresolved type alias target `{first}`{}",
                            if module.glob || declared.locals.iter().any(|scope| scope.glob) {
                                " under glob imports"
                            } else {
                                ""
                            }
                        ))),
                        result => Ok(result),
                    }
                }
            }
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
    // Unowned nominal paths retain the frontend's existing resolution boundary. A glob
    // cannot establish alias identity; only explicit source bindings can do that.

    if context.source_qualified {
        return Ok(Resolved::Missing);
    }
    let result = primitive(parts, generic)?;
    if context.alias_target
        && (module.glob || frame.locals.iter().any(|scope| scope.glob))
        && matches!(&result, Resolved::Type(_))
    {
        return Err(unsupported(format!(
            "glob imports cannot prove primitive alias target `{first}`"
        )));
    }
    Ok(result)
}
fn primitive(parts: &[String], generic: bool) -> Result<Resolved, Failure> {
    if generic {
        return Ok(Resolved::Missing);
    }
    let path = syn::parse_str::<syn::Path>(&parts.join("::"))
        .map_err(|_| unsupported("unresolved type path"))?;
    match rust_integer_path(&path) {
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
        Resolved::Nominal(_) | Resolved::Value | Resolved::Missing | Resolved::Module(_) => {
            Ok(None)
        }
        Resolved::Visible(..) => Err(unsupported("nested source visibility result")),
    }
}
/// Resolve a source module in the type namespace, independently of value-only imports.
pub(super) fn source_module_path(path: &syn::Path) -> Result<Option<Vec<String>>, Failure> {
    let Some(frame) = current() else {
        return Ok(None);
    };
    match path_type(path, &frame, &mut HashSet::new(), false)?
        .visibility()
        .0
    {
        Resolved::Module(module) => Ok(Some(module)),
        _ => Ok(None),
    }
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
        .any(|part| !matches!(part.arguments, syn::PathArguments::None));
    let result = resolve_parts(
        &parts,
        path.leading_colon.is_some(),
        generic,
        &frame,
        &frame.module,
        &mut HashSet::new(),
        LookupContext {
            alias_target: false,
            source_qualified: false,
            ordinary_value_owner: true,
            nominal_arguments: 0,
            unsupported_nominal_arguments: false,
        },
    )?;
    match result.visibility().0 {
        Resolved::Type(typ) => Ok(Some(typ)),
        _ => Ok(None),
    }
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
                    };
                    let generics = &application.generics;
                    if application.unsupported_arguments
                        || generics
                            .params
                            .iter()
                            .any(|parameter| !matches!(parameter, syn::GenericParam::Type(_)))
                    {
                        return Err(unsupported(
                            "nominal lifetime/const arguments require source type specialization",
                        ));
                    }
                    let maximum = generics.type_params().count();
                    let minimum = generics
                        .type_params()
                        .filter(|parameter| parameter.default.is_none())
                        .count();
                    if application.arguments < minimum || application.arguments > maximum {
                        return Err(Failure::Invalid(format!(
                            "nominal type requires {minimum}..={maximum} type arguments, found {}",
                            application.arguments
                        )));
                    }
                    if maximum != 0 || generics.where_clause.is_some() {
                        return Err(unsupported(
                            "generic nominal arguments require source type specialization",
                        ));
                    }
                    Ok(())
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
            assert_eq!(resolve(&syn::parse_str(name).unwrap()).unwrap(), None);
        }
        for name in ["Value", "Mixed"] {
            assert!(
                source_type_owned(&syn::parse_str(name).unwrap())
                    .unwrap_err()
                    .is_invalid(),
                "{name}"
            );
            assert_eq!(resolve(&syn::parse_str(name).unwrap()).unwrap(), None);
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
        let _scope = Frame::root(Catalog::with_crate_root(&file, false)).enter();
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
        integer(&local, "Byte", "u8");
        assert!(matches!(
            query(&local, "Local"),
            Err(Failure::Unsupported(_))
        ));
    }
    #[test]
    fn unowned_standard_module_aliases_and_expression_inference_remain_unowned() {
        let root = frame(
            "use std::sync::atomic as atom;use core::sync::atomic as core_atom;use external::module as external_module;type Recursive=Vec<Recursive>;",
        );
        assert_eq!(query(&root, "atom::AtomicU32").unwrap(), None);
        assert_eq!(query(&root, "core_atom::AtomicU32").unwrap(), None);
        assert_eq!(query(&root, "Vec<_>").unwrap(), None);
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
        assert_eq!(query(&root, "Decoy").unwrap(), None);
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
        assert!(matches!(
            query(&root, "Generic<u8>"),
            Err(Failure::Unsupported(_))
        ));
        assert_eq!(query(&root, "Decoy").unwrap(), None);
        assert!(matches!(query(&root, "::u8"), Err(Failure::Unsupported(_))));
        integer(&root, "::core::primitive::u8", "u8");
    }
}
