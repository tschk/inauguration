//! Bounded generic alias expansion in the existing source resolver.
use super::*;
use quote::ToTokens;

// Dependencies describe the normalized RHS, independently of source proof/layout
// failures. Unknown summaries never establish that a parameter is unused.
#[derive(Clone, Debug, Default)]
pub(super) struct Dependencies {
    used: std::collections::BTreeSet<String>,
    unknown: bool,
}
impl Dependencies {
    fn unknown() -> Self {
        Self {
            unknown: true,
            ..Self::default()
        }
    }
    fn parameter(name: String) -> Self {
        Self {
            used: [name].into_iter().collect(),
            unknown: false,
        }
    }
    pub(super) fn merge(&mut self, other: Self) {
        self.used.extend(other.used);
        self.unknown |= other.unknown;
    }
}
impl Resolved {
    pub(super) fn dependencies(&self) -> Dependencies {
        match self {
            Self::Dependencies(_, dependencies) => dependencies.clone(),
            Self::Visible(value, _) => value.dependencies(),
            Self::Type(_) | Self::Value | Self::Constant(_) => Dependencies::default(),
            _ => Dependencies::unknown(),
        }
    }
    pub(super) fn with_dependencies(self, dependencies: Dependencies) -> Self {
        match self {
            Self::Dependencies(value, _) => Self::Dependencies(value, dependencies),
            value => Self::Dependencies(Box::new(value), dependencies),
        }
    }
}

const MAX_DEPTH: usize = 128;
const MAX_FUEL: usize = 65536;
const MAX_TYPE_BYTES: usize = 65536;
#[derive(Default)]
struct Query {
    depth: usize,
    fuel: usize,
    completed: HashMap<String, Result<Resolved, Failure>>,
    defaults: HashSet<String>,
}
thread_local! { static QUERY: RefCell<Option<Query>> = const { RefCell::new(None) }; }
pub(super) struct QueryGuard(bool);
impl Drop for QueryGuard {
    fn drop(&mut self) {
        QUERY.with(|state| {
            let mut state = state.borrow_mut();
            if self.0 {
                *state = None;
            } else if let Some(query) = state.as_mut() {
                query.depth -= 1;
            }
        });
    }
}
pub(super) fn query() -> Result<QueryGuard, Failure> {
    QUERY.with(|state| {
        let mut state = state.borrow_mut();
        let root = state.is_none();
        let query = state.get_or_insert_with(|| Query {
            fuel: MAX_FUEL,
            ..Query::default()
        });
        if query.depth >= MAX_DEPTH || query.fuel == 0 {
            return Err(unsupported(
                "source alias resolution depth or work limit exceeded",
            ));
        }
        query.depth += 1;
        query.fuel -= 1;
        Ok(QueryGuard(root))
    })
}
pub(super) fn deferred(message: impl Into<String>) -> Resolved {
    Resolved::Nominal(vec![NominalObligation::Failure(unsupported(message))])
}
fn failure_result(failure: Option<Failure>) -> Resolved {
    Resolved::Nominal(
        failure
            .into_iter()
            .map(NominalObligation::Failure)
            .collect(),
    )
}
fn frame_key(frame: &Frame) -> String {
    let mut key = format!(
        "{:p}:{:?}:signature{}",
        Rc::as_ptr(&frame.catalog),
        frame.module,
        source_signatures::cache_key()
    );
    for scope in &frame.locals {
        if scope.parameters {
            let mut parameters: Vec<_> = scope.bindings.iter().collect();
            parameters.sort_by_key(|(name, _)| *name);
            for (name, binding) in parameters {
                key.push_str(name);
                if let Binding::Parameter(value) = binding {
                    key.push_str(&value_key(value));
                } else {
                    key.push('?');
                }
            }
        } else {
            key.push_str(&format!("/{:p}", Rc::as_ptr(scope)));
        }
    }
    key
}
fn value_key(value: &Resolved) -> String {
    match value {
        Resolved::Dependencies(value, dependencies) => {
            format!("{:?}:{}", dependencies, value_key(value))
        }
        Resolved::Type(typ) => format!("{typ:?}"),
        _ => structural_failure(value.clone())
            .map_or_else(|| "nominal".into(), |failure| failure.symbol()),
    }
}
fn concrete(value: &Resolved) -> Option<Typ> {
    let Resolved::Type(typ) = value.clone().visibility().0 else {
        return None;
    };
    if matches!(typ, Typ::Generic(_)) {
        return None;
    }
    if format!("{typ:?}").len() > MAX_TYPE_BYTES {
        return None;
    }
    Some(typ)
}
fn argument(typ: &syn::Type, frame: &Frame, active: &mut HashSet<Key>) -> Resolved {
    match rhs_type(typ, frame, active) {
        Ok(value) => {
            let dependencies = value.dependencies();
            let result = if let Some(failure) = structural_failure(value.clone()) {
                failure_result(Some(failure))
            } else {
                match value.visibility().0 {
                    Resolved::Nominal(_) => {
                        deferred("nominal alias argument lacks an exact native representation")
                    }
                    value => value,
                }
            };
            result.with_dependencies(dependencies)
        }
        Err(failure) => failure_result(Some(failure)),
    }
}

// Defaults expand before entering the alias body. Track their own declaration stage:
// finite explicit self-applications are valid, but omitted cyclic defaults are not.
fn default_argument(
    typ: &syn::Type,
    frame: &Frame,
    declared: &Frame,
    key: &Key,
    index: usize,
    active: &mut HashSet<Key>,
) -> Resolved {
    let stage = format!("{}:{key:?}:{index}", frame_key(declared));
    let entered = QUERY.with(|state| {
        state
            .borrow_mut()
            .as_mut()
            .unwrap()
            .defaults
            .insert(stage.clone())
    });
    if !entered {
        return failure_result(Some(Failure::Invalid("cyclic source alias default".into())));
    }
    let result = argument(typ, frame, active);
    QUERY.with(|state| {
        state.borrow_mut().as_mut().unwrap().defaults.remove(&stage);
    });
    result
}

pub(super) fn expand(
    syntax: &(syn::Generics, syn::Type),
    name: &str,
    key: &Key,
    declared: &Frame,
    access: &[String],
    active: &mut HashSet<Key>,
    context: LookupContext<'_>,
    direct: bool,
) -> Result<Resolved, Failure> {
    let _query = query()?;
    if !direct {
        return Err(unsupported(
            "projected source alias requires associated type resolution",
        ));
    }
    // Import routing guards belong to lookup, not alias expansion. Start every
    // expansion stage from the caller's pre-route ancestry, retaining alias body
    // ancestors while allowing defaults/body paths to traverse the same imports.
    let mut expansion_active = context.caller_active.unwrap_or(active).clone();
    let active = &mut expansion_active;
    let (generics, body) = syntax;
    let parameters: Vec<_> = generics
        .params
        .iter()
        .filter(|parameter| generic_presence(parameter) != Presence::Inactive)
        .collect();
    let mut failure = None;
    let mut names = HashSet::new();
    for parameter in &parameters {
        let identifier = match parameter {
            syn::GenericParam::Type(parameter) => &parameter.ident,
            syn::GenericParam::Const(parameter) => &parameter.ident,
            syn::GenericParam::Lifetime(parameter) => &parameter.lifetime.ident,
        }
        .to_string();
        if generic_presence(parameter) == Presence::Conditional {
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                Some(unsupported(
                    "alias parameters depend on unresolved cfg predicates",
                )),
            );
        } else if !names.insert((
            matches!(parameter, syn::GenericParam::Lifetime(_)),
            identifier.clone(),
        )) {
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                Some(Failure::Invalid(format!(
                    "duplicate alias parameter `{identifier}`"
                ))),
            );
        }
    }
    let no_arguments = syn::PathArguments::None;
    let (arguments, caller) = context.arguments.unwrap_or((&no_arguments, declared));
    let supplied = match arguments {
        syn::PathArguments::None => vec![],
        syn::PathArguments::AngleBracketed(arguments) => arguments.args.iter().collect(),
        _ => {
            return Err(unsupported(
                "parenthesized alias arguments require callable type resolution",
            ));
        }
    };
    let (minimum, maximum) = generic_arity(generics);
    if !context.declaration && (supplied.len() < minimum || supplied.len() > maximum) {
        failure = crate::core_ir::type_proofs::prefer_failure(
            failure,
            Some(Failure::Invalid(format!(
                "source alias `{name}` requires {minimum}..={maximum} arguments, found {}",
                supplied.len()
            ))),
        );
    }
    // Associated value lookup needs an inference proof for omitted generic
    // arguments; defaults alone only establish a type-position application.
    if context.ordinary_value_owner && supplied.len() < parameters.len() {
        return Err(failure.filter(Failure::is_invalid).unwrap_or_else(|| {
            unsupported("generic alias value owner requires explicit arguments or source inference")
        }));
    }
    let mut scope = Scope {
        parameters: true,
        ..Scope::default()
    };
    let mut owner = declared.clone();
    let mut bindings = Vec::new();
    let mut declared_parameters = Vec::new();
    for (index, parameter) in parameters.iter().enumerate() {
        let syn::GenericParam::Type(parameter) = parameter else {
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                Some(unsupported(
                    "alias lifetime/const parameters require source specialization",
                )),
            );
            continue;
        };
        if context.declaration
            && generic_presence(parameters[index]) == Presence::Active
            && let Some(default) = &parameter.default
        {
            let mut default_frame = owner.clone();
            default_frame.locals.push(Rc::new(scope.clone()));
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                structural_failure(default_argument(
                    default,
                    &default_frame,
                    declared,
                    key,
                    index,
                    active,
                )),
            );
        }
        let value = if generic_presence(parameters[index]) == Presence::Conditional {
            deferred("conditional alias parameter")
        } else if context.declaration {
            let identity = format!("{}:{key:?}:{index}", frame_key(declared));
            declared_parameters.push((identity.clone(), parameter.ident.to_string()));
            deferred(format!(
                "symbolic alias parameter {key:?}:{}",
                parameter.ident
            ))
            .with_dependencies(Dependencies::parameter(identity))
        } else if let Some(syn::GenericArgument::Type(typ)) = supplied.get(index) {
            // Caller syntax is evaluated before entering this callee's alias body.
            let mut caller_active = context.caller_active.unwrap_or(active).clone();
            argument(typ, caller, &mut caller_active)
        } else if supplied.get(index).is_some() {
            deferred("non-type alias argument requires source specialization")
        } else if let Some(default) = &parameter.default {
            let mut default_frame = owner.clone();
            default_frame.locals.push(Rc::new(scope.clone()));
            default_argument(default, &default_frame, declared, key, index, active)
        } else {
            deferred("missing source alias argument")
        };
        failure =
            crate::core_ir::type_proofs::prefer_failure(failure, structural_failure(value.clone()));
        bindings.push(value_key(&value));
        scope.insert(
            parameter.ident.to_string(),
            if generic_presence(parameters[index]) == Presence::Conditional {
                Binding::Conditional
            } else {
                Binding::Parameter(Rc::new(value))
            },
        );
        if !parameter.bounds.is_empty() {
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                Some(unsupported(
                    "alias parameter bounds require source obligation resolution",
                )),
            );
        }
    }
    if !scope.bindings.is_empty() {
        owner.locals.push(Rc::new(scope));
    }
    if generics.where_clause.is_some() {
        failure = crate::core_ir::type_proofs::prefer_failure(
            failure,
            Some(unsupported(
                "alias where bounds require source obligation resolution",
            )),
        );
    }
    let cache_key = format!(
        "{key:?}:{}:{access:?}:{}:{}:{:?}:{}",
        frame_key(declared),
        context.declaration,
        frame_key(caller),
        bindings,
        arguments.to_token_stream()
    );
    if active.contains(key) {
        return Err(Failure::Invalid(format!("cyclic type alias `{name}`")));
    }
    if let Some(result) = QUERY.with(|state| {
        state
            .borrow()
            .as_ref()
            .and_then(|query| query.completed.get(&cache_key).cloned())
    }) {
        return result;
    }
    active.insert(key.clone());
    let result = match rhs_type(body, &owner, active) {
        Ok(value) => {
            let dependencies = value.dependencies();
            if !dependencies.unknown {
                for (identity, parameter) in &declared_parameters {
                    if !dependencies.used.contains(identity) {
                        failure = crate::core_ir::type_proofs::prefer_failure(
                            failure,
                            Some(Failure::Invalid(format!(
                                "type parameter `{parameter}` is never used in source alias `{name}`"
                            ))),
                        );
                    }
                }
            }
            failure = crate::core_ir::type_proofs::prefer_failure(
                failure,
                structural_failure(value.clone()),
            );
            Ok(if let Some(failure) = failure {
                failure_result(Some(failure))
            } else {
                value.visibility().0
            }
            .with_dependencies(dependencies))
        }
        Err(error) => Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
            failure,
            Some(error),
        ))),
    };
    active.remove(key);
    // Cache semantic values/proof summaries only; none retains an owning Frame or Catalog.
    QUERY.with(|state| {
        if let Some(query) = state.borrow_mut().as_mut() {
            if query.completed.len() < 4096 {
                query.completed.insert(cache_key, result.clone());
            }
        }
    });
    result
}

pub(super) fn rhs_type(
    typ: &syn::Type,
    frame: &Frame,
    active: &mut HashSet<Key>,
) -> Result<Resolved, Failure> {
    let _query = query()?;
    if source_signatures::active() {
        return source_signatures::rhs_type(typ, frame, active);
    }
    match typ {
        syn::Type::Path(path) => {
            let mut failure = None;
            let mut arguments = Dependencies::default();
            for segment in &path.path.segments {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    for argument in &args.args {
                        if let syn::GenericArgument::Type(typ) = argument {
                            let child = match rhs_type(typ, frame, active) {
                                Ok(value) => {
                                    arguments.merge(value.dependencies());
                                    structural_failure(value)
                                }
                                Err(error) => {
                                    arguments.unknown = true;
                                    Some(error)
                                }
                            };
                            failure = crate::core_ir::type_proofs::prefer_failure(failure, child);
                        }
                    }
                }
            }
            if let Some(qself) = &path.qself {
                let value = rhs_type(&qself.ty, frame, active)?;
                arguments.merge(value.dependencies());
                failure =
                    crate::core_ir::type_proofs::prefer_failure(failure, structural_failure(value));
                return Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                    failure,
                    Some(unsupported(
                        "qualified alias projection requires source specialization",
                    )),
                ))
                .with_dependencies(arguments));
            }
            match path_type(&path.path, frame, active, true) {
                Ok(value) => {
                    // A nominal preserves its type arguments, whereas an alias can
                    // discard them. Eager argument validation above proves errors,
                    // but contributes usage only to the nominal application.
                    let dependencies = match value.clone().visibility().0 {
                        Resolved::Nominal(ref obligations)
                            if obligations.iter().any(|obligation| {
                                matches!(obligation, NominalObligation::Projection)
                            }) =>
                        {
                            // Only source projections consume their associated
                            // argument dependencies here. Alias results already
                            // carry the dependencies selected by their own RHS.
                            let mut dependencies = value.dependencies();
                            dependencies.merge(arguments);
                            dependencies
                        }
                        Resolved::Nominal(ref obligations)
                            if !obligations.is_empty()
                                && obligations.iter().all(|obligation| {
                                    matches!(obligation, NominalObligation::Application(_))
                                }) =>
                        {
                            arguments
                        }
                        _ => value.dependencies(),
                    };
                    let failure = crate::core_ir::type_proofs::prefer_failure(
                        failure,
                        structural_failure(value.clone()),
                    );
                    Ok(if failure.is_some() {
                        failure_result(failure)
                    } else {
                        value
                    }
                    .with_dependencies(dependencies))
                }
                Err(error) => Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                    failure,
                    Some(error),
                ))),
            }
        }
        syn::Type::Tuple(tuple) => {
            let mut failure = None;
            let mut dependencies = Dependencies::default();
            let mut types = Vec::new();
            let mut exact = true;
            let mut bytes = 0usize;
            for element in &tuple.elems {
                match rhs_type(element, frame, active) {
                    Ok(value) => {
                        dependencies.merge(value.dependencies());
                        failure = crate::core_ir::type_proofs::prefer_failure(
                            failure,
                            structural_failure(value.clone()),
                        );
                        if let Some(typ) = concrete(&value) {
                            bytes = bytes.saturating_add(format!("{typ:?}").len());
                            types.push(typ);
                        } else {
                            exact = false;
                        }
                    }
                    Err(error) => {
                        dependencies.unknown = true;
                        exact = false;
                        failure = crate::core_ir::type_proofs::prefer_failure(failure, Some(error));
                    }
                }
            }
            Ok(
                if exact && failure.is_none() && bytes <= MAX_TYPE_BYTES / 4 && types.len() <= 64 {
                    Resolved::Type(crate::core_ir::tuples::typ(&types))
                } else {
                    failure_result(failure.or_else(|| {
                        Some(unsupported("alias tuple layout is not concretely bounded"))
                    }))
                }
                .with_dependencies(dependencies),
            )
        }
        syn::Type::Reference(reference) => {
            let value = rhs_type(&reference.elem, frame, active)?;
            let dependencies = value.dependencies();
            if reference.lifetime.is_some() {
                return Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                    structural_failure(value),
                    Some(unsupported(
                        "alias reference lifetime requires source specialization",
                    )),
                ))
                .with_dependencies(dependencies));
            }
            if let Some(typ) = concrete(&value) {
                if matches!(
                    typ.canonical(),
                    Typ::Int | Typ::Bool | Typ::Float | Typ::String
                ) {
                    let runtime = crate::core_ir::rust_references::erase_type(typ.clone());
                    return Ok(Resolved::Type(crate::core_ir::rust_references::typ(
                        crate::core_ir::rust_references::ReferenceType {
                            mutable: reference.mutability.is_some(),
                            referent: typ,
                            runtime,
                        },
                    ))
                    .with_dependencies(dependencies));
                }
            }
            Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                structural_failure(value),
                Some(unsupported(
                    "alias borrow lacks an exact native representation",
                )),
            ))
            .with_dependencies(dependencies))
        }
        syn::Type::Ptr(pointer) => {
            let value = rhs_type(&pointer.elem, frame, active)?;
            let dependencies = value.dependencies();
            Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                structural_failure(value),
                Some(unsupported(
                    "alias raw pointer requires a native pointer ABI",
                )),
            ))
            .with_dependencies(dependencies))
        }
        syn::Type::Array(array) => {
            let value = rhs_type(&array.elem, frame, active)?;
            let mut dependencies = value.dependencies();
            // Simple constant paths/literals cannot capture a type parameter;
            // other const expressions retain uncertainty rather than guessing.
            if !matches!(&array.len, syn::Expr::Lit(_))
                && !matches!(&array.len, syn::Expr::Path(path) if path.qself.is_none() && path.path.segments.iter().all(|segment| !has_generic_arguments(&segment.arguments)))
            {
                dependencies.unknown = true;
            }
            let _scope = frame.enter();
            let length = super::super::fixed_array_length(&array.len);
            Ok(match (concrete(&value), length) {
                (Some(element), Ok(Some(length))) => {
                    Resolved::Type(crate::core_ir::rust_arrays::typ(element, length))
                }
                (_, Err(error)) => failure_result(crate::core_ir::type_proofs::prefer_failure(
                    structural_failure(value),
                    Some(error),
                )),
                _ => failure_result(crate::core_ir::type_proofs::prefer_failure(
                    structural_failure(value),
                    Some(unsupported(
                        "alias array layout requires owned element and length",
                    )),
                )),
            }
            .with_dependencies(dependencies))
        }
        syn::Type::Slice(slice) => {
            let value = rhs_type(&slice.elem, frame, active)?;
            let dependencies = value.dependencies();
            Ok(failure_result(crate::core_ir::type_proofs::prefer_failure(
                structural_failure(value),
                Some(unsupported("alias slice requires a native slice ABI")),
            ))
            .with_dependencies(dependencies))
        }
        syn::Type::BareFn(function) => bare_fn(function, frame, active),
        syn::Type::Never(_) => Ok(deferred("alias never type lacks a native representation")
            .with_dependencies(Dependencies::default())),
        syn::Type::Paren(value) => rhs_type(&value.elem, frame, active),
        syn::Type::Group(value) => rhs_type(&value.elem, frame, active),
        _ => Err(unsupported(
            "alias target requires unsupported type resolution",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(source: &str) -> Frame {
        Frame::root(Catalog::new(&syn::parse_file(source).unwrap()))
    }
    fn resolved(frame: &Frame, path: &str) -> Result<Option<Typ>, Failure> {
        resolve_in(frame, &syn::parse_str(path).unwrap())
    }
    fn integer(frame: &Frame, path: &str, kind: &str) {
        assert_eq!(
            resolved(frame, path),
            Ok(Some(Typ::Named(format!("#rust-integer-{kind}"))))
        );
    }
    #[test]
    fn generic_alias_declarations_keep_independent_source_errors() {
        for source in [
            "type A<T>=A<T>;",
            "type A<T>=B<T>;type B<U>=A<U>;",
            "type A<T>=B<A<T>>;type B<U>=U;",
            "type A<T>=(T,A<T>);",
            "type A<T>=A<(T,)>;",
            "type Identity<T>=T;type Wrong=Identity<u8,u16>;",
            "struct Row<T>{x:T}type Broken<T>=Row;",
            "fn value(){}type Broken<T>=value;",
            "type Bad<T,T>=u8;",
            "type Id<T=Id>=T;",
            "type A<T=B>=T;type B<U=A>=U;",
            "type A<T>=(T::Associated,A<T>);",
            "type A<T>=(external::Missing,A<T>);",
            "type A<T:Copy>=A<T>;",
        ] {
            let file = syn::parse_file(source).unwrap();
            let failures = validate_declarations(&file, Catalog::new(&file));
            assert!(
                failures.iter().any(Failure::is_invalid),
                "{source}: {failures:?}"
            );
        }
    }
    #[test]
    fn alias_stages_preserve_finite_callers_and_conditional_defaults() {
        for source in [
            "mod owner{pub type Identity<T>=T;}use owner::Identity;type Value=Identity<Identity<u8>>;",
            "type Pick<'a,a>=a;",
            "type Identity<U>=U;type Alias<#[cfg(feature=\"x\")] T=Identity<u8,u16>>=u8;",
            "type Id<T=Id<u8>>=T;type Value=Id;",
        ] {
            let file = syn::parse_file(source).unwrap();
            let failures = validate_declarations(&file, Catalog::new(&file));
            assert!(
                !failures.iter().any(Failure::is_invalid),
                "{source}: {failures:?}"
            );
        }
        let imported = frame("mod owner{pub type Identity<T>=T;}use owner::Identity;");
        integer(&imported, "Identity<Identity<u8>>", "u8");
        let defaults = frame("type Id<T=Id<u8>>=T;");
        integer(&defaults, "Id", "u8");
        integer(&defaults, "Id<Id<u8>>", "u8");
        // A conditional parameter does not conceal an unconditional body error.
        let file = syn::parse_file("type Bad<#[cfg(feature=\"x\")] T=u8>=Bad;").unwrap();
        assert!(
            validate_declarations(&file, Catalog::new(&file))
                .iter()
                .any(Failure::is_invalid)
        );
    }
    #[test]
    fn imported_alias_defaults_use_caller_body_ancestry() {
        for (source, path) in [
            (
                "mod owner{pub type Id<T=super::Id<u8>>=T;}use owner::Id;",
                "Id",
            ),
            (
                "mod owner{pub type Id<T=super::Second<u8>>=T;}use owner::Id as First;use First as Second;",
                "Second",
            ),
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                validate_declarations(&file, Catalog::new(&file)).is_empty(),
                "{source}"
            );
            integer(&frame(source), path, "u8");
        }
        for source in [
            "mod owner{pub type Id<T=super::Id>=T;}use owner::Id;",
            "mod owner{pub type Id<T=super::Second>=T;}use owner::Id as First;use First as Second;",
            "mod owner{pub type Id<T>=super::Id<T>;}use owner::Id;",
            "mod owner{pub type Id<T>=super::Second<(T,)>;}use owner::Id as First;use First as Second;",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                validate_declarations(&file, Catalog::new(&file))
                    .iter()
                    .any(Failure::is_invalid),
                "{source}"
            );
        }
    }
    #[test]
    fn associated_alias_owners_do_not_invent_generic_defaults() {
        let root = frame(
            "type Id<T=u8>=T;type Concrete=Id;mod owner{pub type Id<T=super::Imported<u8>>=T;}use owner::Id as Imported;",
        );
        let _scope = root.enter();
        for path in ["Id", "Id::<>", "owner::Id", "Imported"] {
            let error = resolve_value_owner(&syn::parse_str(path).unwrap()).unwrap_err();
            assert!(!error.is_invalid(), "{path}: {error:?}");
        }
        for path in ["Id::<u8>", "Concrete", "owner::Id::<u8>", "Imported::<u8>"] {
            assert_eq!(
                resolve_value_owner(&syn::parse_str(path).unwrap()),
                Ok(Some(Typ::Named("#rust-integer-u8".into()))),
                "{path}"
            );
        }
        // An associated-function signature does not itself prove type inference.
        let function: syn::ItemFn = syn::parse_quote! {
            fn typed() -> fn(u8, u8) -> u8 { Id::wrapping_add }
        };
        let syn::Stmt::Expr(syn::Expr::Path(mut associated), _) = function.block.stmts[0].clone()
        else {
            panic!("associated function fixture")
        };
        associated.path.segments.pop();
        assert!(
            !resolve_value_owner(&associated.path)
                .unwrap_err()
                .is_invalid()
        );
        integer(&root, "Id", "u8");
        integer(&root, "Concrete", "u8");
    }
    #[test]
    fn generic_arity_retains_only_definite_errors() {
        for (source, path) in [
            ("type Identity<'a,T>=T;", "Identity<u8>"),
            ("type Borrow<'a,T>=&'a T;", "Borrow<u8>"),
            ("struct Row<'a,T>(&'a T);", "Row<u8>"),
            (
                "type Conditional<#[cfg(feature=\"x\")] T>=u8;",
                "Conditional",
            ),
        ] {
            let root = frame(source);
            let _scope = root.enter();
            assert!(
                !source_type_owned(&syn::parse_str(path).unwrap())
                    .unwrap_err()
                    .is_invalid(),
                "{source}: {path}"
            );
        }
        for source in [
            "type Maybe<#[cfg(feature=\"x\")] T>=u8;type Bad=Maybe<u8,u16>;",
            "struct Maybe<#[cfg(feature=\"x\")] T>{}type Bad=Maybe<u8,u16>;",
            "type Identity<'a,T>=T;type Bad=Identity;",
            "struct Row<'a,T>(&'a T);type Bad=Row;",
            "type Broken<'a,T>=(T,Broken<'a,T>);",
            "type Maybe<#[cfg(feature=\"x\")] T>=Maybe<u8,u16>;",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                validate_declarations(&file, Catalog::new(&file))
                    .iter()
                    .any(Failure::is_invalid),
                "{source}"
            );
        }
    }
    #[test]
    fn type_parameter_and_primitive_applications_keep_arity() {
        for source in [
            "type Id<T>=T<u8>;",
            "type Bad=u8<u16>;",
            "type Bad=bool<u8>;",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                validate_declarations(&file, Catalog::new(&file))
                    .iter()
                    .any(Failure::is_invalid),
                "{source}"
            );
        }
        let root = frame("type Id<T>=T<>;mod owner{pub type u8<T>=T;}type Shadow=owner::u8<u16>;");
        integer(&root, "Id<u8>", "u8");
        integer(&root, "u8<>", "u8");
        integer(&root, "Shadow", "u16");
        assert_eq!(resolved(&root, "bool<>"), Ok(Some(Typ::Bool)));
        assert_eq!(
            resolved(&root, "f64<>"),
            Ok(Some(crate::core_ir::rust_floats::typ(
                crate::core_ir::rust_floats::Kind::F64
            )))
        );
        let _scope = root.enter();
        assert_eq!(
            resolve_value_owner(&syn::parse_str("u8::<>").unwrap()),
            Ok(Some(Typ::Named("#rust-integer-u8".into())))
        );
    }
    #[test]
    fn unused_type_parameters_follow_normalized_rhs_dependencies() {
        for source in [
            "type Ignored<T>=u8;",
            "type A<T:Copy>=u8;",
            "type A<T> where T:Copy =u8;",
            "type A<T=u8,U=T>=U;",
            "type Ignore<T>=u8;type A<T>=Ignore<T>;",
            "type Ignore<T=u8>=u8;type A<T>=Ignore<T>;",
            "type Ignore<T>=u8;type Next<T>=Ignore<T>;type A<T>=Next<T>;",
            "type First<A,B>=A;type Outer<T,U>=First<T,U>;",
            "trait Trait{type Item;}type A<T:Trait<Item=u8>>=u8;",
            "type A<T=u8,const N:usize=1>=[u8;N];",
            "type A<#[cfg(all())] T>=u8;",
            "type A<T>=fn()->u8;",
        ] {
            let file = syn::parse_file(source).unwrap();
            let failures = validate_declarations(&file, Catalog::new(&file));
            assert!(
                failures.iter().any(Failure::is_invalid),
                "{source}: {failures:?}"
            );
        }
        for source in [
            "fn outer(){type Ignored<T>=u8;}",
            "fn outer(){mod local{type Ignored<T>=u8;}}",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                validate_declarations(&file, Catalog::module_source(&file))
                    .iter()
                    .any(Failure::is_invalid),
                "{source}"
            );
        }
        // Check the caller itself: validating the callee alone would conceal a
        // regression which counted an eagerly validated but discarded argument.
        let root = frame("type First<A,B>=A;type Outer<T,U>=First<T,U>;");
        let error =
            validate_alias_declaration(&syn::parse_str("Outer").unwrap(), &root).unwrap_err();
        assert!(
            matches!(error, Failure::Invalid(message) if message.contains("`U`") && message.contains("`Outer`"))
        );
    }
    #[test]
    fn used_or_unproven_type_parameters_do_not_gain_unused_failures() {
        for source in [
            "type A<T=u8,U=T>=(T,U);",
            "type Id<T>=T;type Next<T>=Id<T>;type A<T>=Next<T>;",
            "type A<T>=(T,u8);",
            "type A<T>=*const T;type B<T>=&'static T;",
            "struct Wrap<T>(T);type A<T>=Wrap<T>;",
            "type A<T>=core::marker::PhantomData<T>;",
            "trait Trait{type Item;}type A<T>=<T as Trait>::Item;",
            "trait Trait{type Item;}type A<T:Trait>=T::Item;",
            "type A<'a>=u8;",
            "type A<'a,T>=&'a T;",
            "type A<const N:usize>=u8;",
            "type A<#[cfg(any())] T>=u8;",
            "type A<#[cfg(feature=\"x\")] T>=u8;",
            "type A<T>=external::Unknown;",
            "type A<T>=fn(T)->T;",
            "mod owner{pub type Id<T>=T;}use owner::Id;type A<T>=Id<T>;",
        ] {
            let file = syn::parse_file(source).unwrap();
            let failures = validate_declarations(&file, Catalog::new(&file));
            assert!(
                !failures.iter().any(Failure::is_invalid),
                "{source}: {failures:?}"
            );
        }
    }
    #[test]
    fn projected_associated_arguments_retain_parameter_dependencies() {
        for source in [
            "trait Trait{type Assoc<U>;}type A<T:Trait,U>=T::Assoc<U>;",
            "trait Trait{type Assoc<U>;}type Id<T>=T;type A<T:Trait,U>=T::Assoc<Id<U>>;",
            "trait Trait{type Assoc<U>;}type A<T:Trait,U>=<T as Trait>::Assoc<U>;",
        ] {
            let root = frame(source);
            let result = validate_alias_declaration(&syn::parse_str("A").unwrap(), &root);
            assert!(
                !result.is_err_and(|failure| failure.is_invalid()),
                "{source}"
            );
        }
    }
    #[test]
    fn projected_arguments_preserve_nested_alias_discarding() {
        let root = frame(
            "trait Trait{type Assoc<U>;}type Ignore<X>=u8;type A<T:Trait,U>=T::Assoc<Ignore<U>>;",
        );
        let result = validate_alias_declaration(&syn::parse_str("A").unwrap(), &root);
        assert!(
            matches!(result, Err(Failure::Invalid(message)) if message.contains("`U`") && message.contains("`A`"))
        );
    }
    #[test]
    fn alias_nominal_results_keep_authoritative_discarded_dependencies() {
        for source in [
            "struct Unit;type Ignore<T>=Unit;type Outer<T>=Ignore<T>;",
            "struct Unit;type Ignore<T>=Unit;type Middle<T>=Ignore<T>;type Outer<T>=Middle<T>;",
        ] {
            let root = frame(source);
            let result = validate_alias_declaration(&syn::parse_str("Outer").unwrap(), &root);
            assert!(
                matches!(result, Err(Failure::Invalid(message)) if message.contains("`T`") && message.contains("`Outer`")),
                "{source}"
            );
        }
    }
    #[test]
    fn dependency_cache_distinguishes_equal_layout_parameter_arguments() {
        let root = frame("type First<T,U>=T;type Both<T,U>=(First<T,U>,First<U,T>);");
        let result = validate_alias_declaration(&syn::parse_str("Both").unwrap(), &root);
        assert!(!result.is_err_and(|failure| failure.is_invalid()));
    }
    #[test]
    fn generic_alias_applications_resolve_owned_arguments_and_defaults() {
        let root = frame(
            "type Identity<T>=T;type Defaulted<T=u8>=T;type Nested<T>=Identity<T>;type Pair<A=u8,B=A>=(A,B);mod owner{pub type T=u16;pub type Identity<T>=T;pub type Defaulted<U=T>=U;}type T=u8;",
        );
        for path in [
            "Identity<u8>",
            "Defaulted",
            "Nested<Identity<u8>>",
            "Identity<Identity<u8>>",
            "Identity<Identity<Identity<u8>>>",
            "owner::Identity<T>",
        ] {
            integer(&root, path, "u8");
        }
        integer(&root, "owner::Defaulted", "u16");
        let Some(Typ::Named(pair)) = resolved(&root, "Pair").unwrap() else {
            panic!("pair")
        };
        assert_eq!(
            crate::core_ir::tuples::fields(&pair).unwrap(),
            vec![
                ("_0".into(), Typ::Named("#rust-integer-u8".into())),
                ("_1".into(), Typ::Named("#rust-integer-u8".into())),
            ]
        );
        let block = syn::parse_str::<syn::Block>("{type T=u16;}").unwrap();
        integer(&root.block(&block), "owner::Identity<T>", "u16");
    }
    #[test]
    fn generic_alias_builtin_arguments_use_existing_scalar_representations() {
        let root = frame("type Identity<T>=T;type Pair<T>=(T,T);");
        let _scope = root.enter();
        for (path, typ) in [
            ("Identity<bool>", Typ::Bool),
            (
                "Identity<f64>",
                crate::core_ir::rust_floats::typ(crate::core_ir::rust_floats::Kind::F64),
            ),
            (
                "Identity<core::primitive::f32>",
                crate::core_ir::rust_floats::typ(crate::core_ir::rust_floats::Kind::F32),
            ),
        ] {
            assert_eq!(resolved(&root, path), Ok(Some(typ)));
            assert!(source_type_owned(&syn::parse_str(path).unwrap()).unwrap());
        }
        let Some(Typ::Named(pair)) = resolved(&root, "Pair<bool>").unwrap() else {
            panic!("pair")
        };
        assert_eq!(
            crate::core_ir::tuples::fields(&pair).unwrap(),
            vec![("_0".into(), Typ::Bool), ("_1".into(), Typ::Bool)]
        );
    }
    #[test]
    fn generic_alias_arrays_and_private_owners_keep_source_identity() {
        let root = frame(
            "type Fixed<T>=[T;2];mod owner{type Hidden=u16;pub type Identity<T>=T;pub type Defaulted<T=Hidden>=T;}",
        );
        let Some(Typ::Named(array)) = resolved(&root, "Fixed<u8>").unwrap() else {
            panic!("array")
        };
        let array = crate::core_ir::rust_arrays::decode(&array).unwrap();
        assert_eq!(array.length, 2);
        assert_eq!(array.element, Typ::Named("#rust-integer-u8".into()));
        integer(&root, "owner::Defaulted", "u16");
        assert!(matches!(
            resolved(&root, "owner::Identity<owner::Hidden>"),
            Err(Failure::Invalid(_))
        ));
    }
    #[test]
    fn generic_aliases_defer_unowned_and_non_native_obligations() {
        let root = frame(
            "struct Row;type Identity<T>=T;type Bound<T:Copy>=T;type Borrow<'a>=u8;type Projection<T>=T::Associated;type Conditional<#[cfg(feature=\"x\")] T=u8>=T;",
        );
        for path in [
            "Identity<Row>",
            "Bound<u8>",
            "Borrow<'static>",
            "Projection<u8>",
            "Conditional",
        ] {
            assert!(
                matches!(resolved(&root, path), Err(Failure::Unsupported(_))),
                "{path}"
            );
        }
        let file =
            syn::parse_file("#[cfg(feature=\"x\")]mod hidden{type A<T>=A<T>;}type Identity<T>=T;")
                .unwrap();
        assert!(validate_declarations(&file, Catalog::new(&file)).is_empty());
        let file = syn::parse_file("type Identity<T>=T;type A=Identity<crate::Other>;").unwrap();
        assert!(validate_declarations(&file, Catalog::module_source(&file)).is_empty());
    }
    #[test]
    fn generic_alias_declaration_dags_and_late_errors_are_bounded() {
        let mut source = "type A0<T>=T;type B0=u8;".to_string();
        for index in 1..24 {
            source.push_str(&format!(
                "type A{index}<T>=(A{}<T>,A{}<T>);type B{index}=(B{},B{});",
                index - 1,
                index - 1,
                index - 1,
                index - 1
            ));
        }
        source.push_str("type Broken<T>=Broken<(T,)>;");
        let file = syn::parse_file(&source).unwrap();
        assert!(
            validate_declarations(&file, Catalog::new(&file))
                .iter()
                .any(Failure::is_invalid)
        );
        let mut source = String::new();
        for index in 0..1200 {
            source.push_str(&format!("type Good{index}=u8;"));
        }
        source.push_str("type Bad<T>=Bad<T>;");
        let file = syn::parse_file(&source).unwrap();
        assert!(
            validate_declarations(&file, Catalog::new(&file))
                .iter()
                .any(Failure::is_invalid)
        );
    }
}

// Shared syntax collector for ordinary alias and exact trait-signature queries.
// Child resolution, dependencies, recursion/fuel and failures stay in rhs_type.
pub(super) fn bare_fn(
    function: &syn::TypeBareFn,
    frame: &Frame,
    active: &mut HashSet<Key>,
) -> Result<Resolved, Failure> {
    if function
        .inputs
        .iter()
        .any(|argument| attrs_presence(&argument.attrs) != Presence::Active)
    {
        return Err(unsupported(
            "callable parameter cfg requires source obligations",
        ));
    }
    callable(
        function.inputs.iter().map(|argument| &argument.ty),
        &function.output,
        function.abi.as_ref(),
        function.unsafety.is_some(),
        function.variadic.is_some(),
        function.lifetimes.is_some(),
        frame,
        active,
    )
}
#[allow(clippy::too_many_arguments)] // Flat syntax facts share one existing child resolver.
pub(super) fn callable<'a>(
    parameters: impl Iterator<Item = &'a syn::Type>,
    output: &syn::ReturnType,
    abi: Option<&syn::Abi>,
    unsafe_call: bool,
    variadic: bool,
    bound_lifetimes: bool,
    frame: &Frame,
    active: &mut HashSet<Key>,
) -> Result<Resolved, Failure> {
    let _source_context = source_signatures::ensure_source_context(frame);
    let mut dependencies = Dependencies::default();
    let mut failure = None;
    let mut child = |source: &syn::Type| -> Typ {
        match rhs_type(source, frame, active) {
            Ok(value) => {
                dependencies.merge(value.dependencies());
                failure = crate::core_ir::type_proofs::prefer_failure(
                    failure.take(),
                    structural_failure(value.clone()),
                );
                let exact = source_signatures::exact(value);
                match exact {
                    Ok(typ) => typ,
                    Err(error) => {
                        failure = crate::core_ir::type_proofs::prefer_failure(
                            failure.take(),
                            Some(error),
                        );
                        Typ::Void
                    }
                }
            }
            Err(error) => {
                dependencies.unknown = true;
                failure = crate::core_ir::type_proofs::prefer_failure(failure.take(), Some(error));
                Typ::Void
            }
        }
    };
    #[allow(clippy::redundant_closure)]
    let parameters = parameters.map(|source| child(source)).collect();
    let result = match output {
        syn::ReturnType::Default => Typ::Void,
        syn::ReturnType::Type(_, typ) => child(typ),
    };
    let abi = abi.map_or(crate::core_ir::rust_callables::Abi::Rust, |abi| {
        match abi
            .name
            .as_ref()
            .map_or_else(|| "C".into(), syn::LitStr::value)
            .as_str()
        {
            "Rust" => crate::core_ir::rust_callables::Abi::Rust,
            name => crate::core_ir::rust_callables::Abi::Explicit(name.into()),
        }
    });
    let typ = crate::core_ir::rust_callables::typ(crate::core_ir::rust_callables::Signature {
        abi,
        unsafe_call,
        variadic,
        bound_lifetimes,
        parameters,
        result,
    });
    if format!("{typ:?}").len() > MAX_TYPE_BYTES {
        failure = crate::core_ir::type_proofs::prefer_failure(
            failure,
            Some(unsupported(
                "callable alias layout exceeds owned type budget",
            )),
        );
    }
    Ok(if failure.is_some() {
        failure_result(failure)
    } else {
        Resolved::Type(typ)
    }
    .with_dependencies(dependencies))
}

#[cfg(test)]
mod callable_alias_tests {
    use super::*;
    #[test]
    fn callable_aliases_keep_semantic_parameters_and_nested_invalids() {
        let file = syn::parse_file("type Callback<T>=fn(T)->T;type Plain=Callback<u8>;").unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let typ = resolve_in(&frame, &syn::parse_str("Plain").unwrap())
            .unwrap()
            .unwrap();
        let Typ::Named(name) = typ else {
            unreachable!()
        };
        let signature = crate::core_ir::rust_callables::decode(&name).unwrap();
        assert_eq!(
            signature.parameters,
            vec![Typ::Named("#rust-integer-u8".into())]
        );
        assert_eq!(signature.result, signature.parameters[0]);
        assert!(validate_declarations(&file, frame.catalog.clone()).is_empty());
        let bad = syn::parse_file("type Bad=fn(unowned::Unknown)->bool<u8>;").unwrap();
        assert!(
            validate_declarations(&bad, Catalog::new(&bad))
                .iter()
                .any(Failure::is_invalid)
        );
    }
}

#[cfg(test)]
mod ordinary_callable_source_context_tests {
    use super::*;
    #[test]
    fn ordinary_callable_alias_children_are_exact_source_types() {
        let file = syn::parse_file(
            "type Callback<T>=fn(Result<T,bool>,String)->Result<T,bool>; type Plain=Callback<i64>;",
        )
        .unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let source = syn::parse_str("Plain").unwrap();
        let ordinary = resolve_in(&frame, &source).unwrap().unwrap();
        let exact_source = syn::Type::Path(syn::TypePath {
            qself: None,
            path: source,
        });
        let exact = source_signatures::source_type(&exact_source, &frame).unwrap();
        assert!(crate::core_ir::rust_references::same_source_type(
            &ordinary, &exact
        ));
        let signature = crate::core_ir::rust_callables::signature_of(&ordinary).unwrap();
        assert_ne!(signature.result, Typ::Named("#rust-integer-i64".into()));
        assert_ne!(signature.parameters[1], Typ::String);
        assert!(validate_declarations(&file, frame.catalog.clone()).is_empty());
    }
}
