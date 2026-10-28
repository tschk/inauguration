//! Exact source signature identities, before frontend ABI/layout erasure.
//! Namespace lookup and alias substitution remain owned by the shared Catalog.
use super::*;

struct Context {
    id: usize,
    implementation: Option<Rc<syn::ItemImpl>>,
    trait_declaration: Option<Rc<syn::ItemTrait>>,
    declaring: Frame,
    owner: Option<Typ>,
    allow_parameters: bool,
    associated: RefCell<HashSet<String>>,
}
thread_local! { static CONTEXT: RefCell<Option<Rc<Context>>> = const { RefCell::new(None) }; }
pub(super) struct Guard(Option<Rc<Context>>);
impl Drop for Guard {
    fn drop(&mut self) {
        CONTEXT.with(|state| {
            state.replace(self.0.take());
        });
    }
}
pub(super) fn active() -> bool {
    CONTEXT.with(|state| state.borrow().is_some())
}
pub(super) fn cache_key() -> usize {
    CONTEXT.with(|state| state.borrow().as_ref().map_or(0, |context| context.id))
}
fn context() -> Result<Rc<Context>, Failure> {
    CONTEXT
        .with(|state| state.borrow().clone())
        .ok_or_else(|| unsupported("source signature query is not active"))
}
fn leaf(name: &str) -> Typ {
    Typ::Named(format!("#rust-source-signature:{name}"))
}
fn constructor(name: &str, arguments: &[Typ]) -> Typ {
    let mut fields = Vec::with_capacity(arguments.len() + 1);
    fields.push(if let Some(identity) = name.strip_prefix("nominal:") {
        crate::core_ir::source_types::nominal(identity)
    } else {
        leaf(name)
    });
    fields.extend_from_slice(arguments);
    crate::core_ir::tuples::typ(&fields)
}
pub(super) fn resolve(
    source: &syn::Type,
    frame: &Frame,
    implementation: Rc<syn::ItemImpl>,
    trait_declaration: Rc<syn::ItemTrait>,
    declaring: &Frame,
    owner: &[String],
) -> Result<Typ, Failure> {
    let context = Rc::new(Context {
        id: next_id(),
        implementation: Some(implementation),
        trait_declaration: Some(trait_declaration),
        declaring: declaring.clone(),
        owner: Some(crate::core_ir::source_types::nominal(&owner.join("::"))),
        allow_parameters: false,
        associated: RefCell::new(HashSet::new()),
    });
    let _guard = Guard(CONTEXT.with(|state| state.replace(Some(context))));
    let _query = generic_aliases::query()?;
    exact(rhs_type(source, frame, &mut HashSet::new())?)
}
pub(super) fn exact(value: Resolved) -> Result<Typ, Failure> {
    if let Some(failure) = structural_failure(value.clone()) {
        return Err(failure);
    }
    match value.visibility().0 {
        Resolved::Type(value)
            if !matches!(value, Typ::Generic(_)) || context()?.allow_parameters =>
        {
            Ok(value)
        }
        _ => Err(unsupported(
            "source signature type identity is not concrete and owned",
        )),
    }
}
fn arguments(
    arguments: Option<(&syn::PathArguments, &Frame)>,
    active: &mut HashSet<Key>,
) -> Result<(Vec<Typ>, generic_aliases::Dependencies), Failure> {
    let mut dependencies = generic_aliases::Dependencies::default();
    let Some((arguments, frame)) = arguments else {
        return Ok((vec![], dependencies));
    };
    let values = match arguments {
        syn::PathArguments::None => vec![],
        syn::PathArguments::AngleBracketed(arguments) => arguments
            .args
            .iter()
            .map(|argument| match argument {
                syn::GenericArgument::Type(value) => child(value, frame, active, &mut dependencies),
                _ => Err(unsupported(
                    "signature lifetime/const/associated arguments require source obligations",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(unsupported(
                "signature callable arguments require source obligations",
            ));
        }
    };
    Ok((values, dependencies))
}
fn child(
    source: &syn::Type,
    frame: &Frame,
    active: &mut HashSet<Key>,
    dependencies: &mut generic_aliases::Dependencies,
) -> Result<Typ, Failure> {
    let value = rhs_type(source, frame, active)?;
    dependencies.merge(value.dependencies());
    exact(value)
}
pub(super) fn nominal(
    identity: Option<Vec<String>>,
    generics: &syn::Generics,
    context: LookupContext<'_>,
    active: &mut HashSet<Key>,
) -> Result<Resolved, Failure> {
    let identity =
        identity.ok_or_else(|| unsupported("local nominal signature identity is not owned"))?;
    let (args, dependencies) = arguments(context.arguments, active)?;
    let parameters: Vec<_> = generics
        .params
        .iter()
        .filter(|parameter| generic_presence(parameter) != Presence::Inactive)
        .collect();
    if parameters.iter().any(|parameter| {
        generic_presence(parameter) != Presence::Active
            || !matches!(parameter,syn::GenericParam::Type(value) if value.bounds.is_empty())
    }) || generics.where_clause.is_some()
    {
        return Err(unsupported(
            "nominal signature arguments have unproven bounds",
        ));
    }
    let (minimum, maximum) = generic_arity(generics);
    if args.len() < minimum || args.len() > maximum {
        return Err(Failure::Invalid(
            "nominal signature generic argument count differs".into(),
        ));
    }
    if args.len() != parameters.len() {
        return Err(unsupported(
            "nominal signature generic defaults require source substitution",
        ));
    }
    let name = format!("nominal:{}", identity.join("::"));
    Ok(Resolved::Type(if args.is_empty() {
        crate::core_ir::source_types::nominal(&identity.join("::"))
    } else {
        constructor(&name, &args)
    })
    .with_dependencies(dependencies))
}
/// Called only after shared lexical lookup found no source binding (or for an
/// absolute standard path). This does not override source aliases or shadows.
pub(super) fn builtin(
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
        ["str"] | ["std" | "core", "primitive", "str"] => ("primitive:str", 0),
        ["String"] | ["std" | "alloc", "string", "String"] => ("std:string:String", 0),
        ["Result"] | ["std" | "core", "result", "Result"] => ("std:result:Result", 2),
        ["Option"] | ["std" | "core", "option", "Option"] => ("std:option:Option", 1),
        ["Vec"] | ["std" | "alloc", "vec", "Vec"] => ("std:vec:Vec", 1),
        _ => return Ok(None),
    };
    let (args, dependencies) = arguments(context.arguments, active)?;
    if args.len() != arity {
        return Err(Failure::Invalid(
            "standard source signature constructor argument count differs".into(),
        ));
    }
    Ok(Some(
        Resolved::Type(if arity == 0 {
            leaf(name)
        } else {
            constructor(name, &args)
        })
        .with_dependencies(dependencies),
    ))
}
pub(super) fn rhs_type(
    source: &syn::Type,
    frame: &Frame,
    active: &mut HashSet<Key>,
) -> Result<Resolved, Failure> {
    let _query = generic_aliases::query()?;
    let mut dependencies = generic_aliases::Dependencies::default();
    let value = match source {
        syn::Type::Path(path) if path.qself.is_none() => {
            if path.path.leading_colon.is_none()
                && path
                    .path
                    .segments
                    .first()
                    .is_some_and(|segment| segment.ident == "Self")
            {
                let context = context()?;
                if path
                    .path
                    .segments
                    .iter()
                    .any(|segment| has_generic_arguments(&segment.arguments))
                {
                    return Err(unsupported(
                        "generic associated signature type is not owned",
                    ));
                }
                if path.path.segments.len() == 1 {
                    return context.owner.clone().map(Resolved::Type).ok_or_else(|| {
                        unsupported("Self requires an owned source implementation context")
                    });
                }
                if path.path.segments.len() != 2 {
                    return Err(unsupported(
                        "nested associated signature type requires projection proofs",
                    ));
                }
                let member = path.path.segments[1].ident.to_string();
                let trait_declaration = context.trait_declaration.as_ref().ok_or_else(|| {
                    unsupported("associated Self requires an owned source trait context")
                })?;
                let declarations: Vec<_> = trait_declaration
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        syn::TraitItem::Type(item)
                            if item.ident == member
                                && attrs_presence(&item.attrs) != Presence::Inactive =>
                        {
                            Some(item)
                        }
                        _ => None,
                    })
                    .collect();
                if declarations.is_empty() {
                    return Err(Failure::Invalid(
                        "associated signature type is not declared by the source trait".into(),
                    ));
                }
                if declarations.len() != 1
                    || attrs_presence(&declarations[0].attrs) != Presence::Active
                {
                    return Err(unsupported(
                        "trait associated signature type has conditional or duplicate declarations",
                    ));
                }
                if !declarations[0].bounds.is_empty()
                    || !declarations[0].generics.params.is_empty()
                    || declarations[0].generics.where_clause.is_some()
                {
                    return Err(unsupported(
                        "trait associated signature type requires generic obligations",
                    ));
                }

                if !context.associated.borrow_mut().insert(member.clone()) {
                    return Err(Failure::Invalid("cyclic associated signature type".into()));
                }
                let result = (|| {
                    let implementation = context.implementation.as_ref().ok_or_else(|| {
                        unsupported(
                            "associated Self requires an owned source implementation context",
                        )
                    })?;
                    let definitions: Vec<_> = implementation
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            syn::ImplItem::Type(item)
                                if item.ident == member
                                    && attrs_presence(&item.attrs) != Presence::Inactive =>
                            {
                                Some(item)
                            }
                            _ => None,
                        })
                        .collect();
                    let [definition] = definitions.as_slice() else {
                        return Err(unsupported(
                            "associated signature type definition is unresolved or duplicated",
                        ));
                    };
                    if attrs_presence(&definition.attrs) != Presence::Active
                        || !definition.generics.params.is_empty()
                        || definition.generics.where_clause.is_some()
                    {
                        return Err(unsupported(
                            "associated signature type has conditional or generic obligations",
                        ));
                    }
                    rhs_type(&definition.ty, &context.declaring, active)
                })();
                context.associated.borrow_mut().remove(&member);
                return result;
            }
            return path_type(&path.path, frame, active, true);
        }
        syn::Type::Paren(value) => return rhs_type(&value.elem, frame, active),
        syn::Type::Group(value) => return rhs_type(&value.elem, frame, active),
        syn::Type::Tuple(value) if value.elems.is_empty() => Typ::Void,
        syn::Type::Tuple(value) => constructor(
            "tuple",
            &value
                .elems
                .iter()
                .map(|value| child(value, frame, active, &mut dependencies))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        syn::Type::Reference(value) => {
            if value.lifetime.is_some() {
                return Err(unsupported(
                    "explicit signature reference lifetimes require source proof",
                ));
            }
            constructor(
                if value.mutability.is_some() {
                    "ref-mut"
                } else {
                    "ref"
                },
                &[child(&value.elem, frame, active, &mut dependencies)?],
            )
        }
        syn::Type::Slice(value) => constructor(
            "slice",
            &[child(&value.elem, frame, active, &mut dependencies)?],
        ),
        syn::Type::Array(value) => {
            let element = child(&value.elem, frame, active, &mut dependencies)?;
            let length = {
                let _scope = frame.enter();
                super::super::fixed_array_length(&value.len)?
            }
            .ok_or_else(|| unsupported("signature array length is not owned"))?;
            constructor(&format!("array:{length}"), &[element])
        }
        syn::Type::BareFn(value) => return generic_aliases::bare_fn(value, frame, active),
        syn::Type::Never(_) => leaf("never"),
        _ => {
            return Err(unsupported(
                "source signature type form requires additional ownership proofs",
            ));
        }
    };
    Ok(Resolved::Type(value).with_dependencies(dependencies))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structural_signature_queries_preserve_alias_constructor_arguments_and_cache_modes() {
        let file = syn::parse_file(
            "struct Row;type Text=str;type Owned=String;type R<T,E>=Result<T,E>;type Id<T>=T;type Ref<T>=&T;type Pair<T,E>=(T,(E,T));",
        )
        .unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let implementation = Rc::new(syn::parse_str::<syn::ItemImpl>("impl Row{}").unwrap());
        // Keep the shared alias cache alive across BOTH modes, rather than
        // accidentally testing only independently reset top-level queries.
        let _query = generic_aliases::query().unwrap();
        let text_path = syn::parse_str("Text").unwrap();
        let ordinary_before = format!("{:?}", resolve_in(&frame, &text_path));
        let resolve = |source: &str| {
            super::resolve(
                &syn::parse_str(source).unwrap(),
                &frame,
                implementation.clone(),
                Rc::new(syn::parse_str("trait Make{}").unwrap()),
                &frame,
                &["Row".into()],
            )
            .unwrap()
        };
        assert_ne!(resolve("&Text"), resolve("&Owned"));
        assert_eq!(resolve("&Text"), resolve("&str"));
        assert_eq!(resolve("&Owned"), resolve("&String"));
        assert_eq!(resolve("R<i64,bool>"), resolve("Result<i64,bool>"));
        assert_ne!(resolve("R<i64,bool>"), resolve("R<i64,u8>"));
        assert_eq!(resolve("Id<R<i64,bool>>"), resolve("Result<i64,bool>"));
        assert_eq!(resolve("Ref<i64>"), resolve("&i64"));
        assert_eq!(resolve("Pair<i64,bool>"), resolve("(i64,(bool,i64))"));
        assert_eq!(
            ordinary_before,
            format!("{:?}", resolve_in(&frame, &text_path))
        );
        assert_eq!(resolve("&Text"), resolve("&str"));
        assert_eq!(
            ordinary_before,
            format!("{:?}", resolve_in(&frame, &text_path))
        );
    }
    #[test]
    fn structural_signature_queries_respect_source_constructor_shadows() {
        let file = syn::parse_file(
            "struct Row;struct Result<T,E>{value:T,error:E}type R=Result<i64,bool>;",
        )
        .unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let implementation = Rc::new(syn::parse_str::<syn::ItemImpl>("impl Row{}").unwrap());
        let resolve = |source: &str| {
            super::resolve(
                &syn::parse_str(source).unwrap(),
                &frame,
                implementation.clone(),
                Rc::new(syn::parse_str("trait Make{}").unwrap()),
                &frame,
                &["Row".into()],
            )
            .unwrap()
        };
        assert_eq!(resolve("R"), resolve("Result<i64,bool>"));
        assert_ne!(resolve("R"), resolve("::core::result::Result<i64,bool>"));
    }
}

fn next_id() -> usize {
    static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
fn source_guard(frame: &Frame) -> Guard {
    let context = Rc::new(Context {
        id: next_id(),
        implementation: None,
        trait_declaration: None,
        declaring: frame.clone(),
        owner: None,
        allow_parameters: true,
        associated: RefCell::new(HashSet::new()),
    });
    Guard(CONTEXT.with(|state| state.replace(Some(context))))
}
/// Bare callable aliases require source identities even in an ordinary alias query.
/// Preserve an existing trait/source context and the caller's query/recursion state.
pub(super) fn ensure_source_context(frame: &Frame) -> Option<Guard> {
    (!active()).then(|| source_guard(frame))
}
/// General source query; never reconstruct source identity from map_type output.
pub(super) fn source_type(source: &syn::Type, frame: &Frame) -> Result<Typ, Failure> {
    let _context = source_guard(frame);
    let _query = generic_aliases::query()?;
    exact(rhs_type(source, frame, &mut HashSet::new())?)
}
/// Function items share the same source child collector as bare function types.
pub(super) fn declaration(
    signature: &syn::Signature,
    frame: &Frame,
) -> Result<crate::core_ir::rust_callables::Signature, Failure> {
    let _context = source_guard(frame);
    let _query = generic_aliases::query()?;
    if signature.inputs.iter().any(|input| match input {
        syn::FnArg::Typed(input) => attrs_presence(&input.attrs) != Presence::Active,
        syn::FnArg::Receiver(_) => true,
    }) {
        return Err(unsupported(
            "source callable receiver or parameter cfg requires additional source context",
        ));
    }
    let parameters = signature.inputs.iter().filter_map(|input| {
        if let syn::FnArg::Typed(value) = input {
            Some(value.ty.as_ref())
        } else {
            None
        }
    });
    let value = generic_aliases::callable(
        parameters,
        &signature.output,
        signature.abi.as_ref(),
        signature.unsafety.is_some(),
        signature.variadic.is_some(),
        signature.generics.lifetimes().next().is_some(),
        frame,
        &mut HashSet::new(),
    )?;
    let Typ::Named(name) = exact(value)? else {
        return Err(unsupported("source callable signature was not retained"));
    };
    crate::core_ir::rust_callables::decode(&name)
        .ok_or_else(|| unsupported("source callable signature payload is not owned"))
}
#[cfg(test)]
mod standalone_source_tests {
    use super::*;
    #[test]
    fn general_source_queries_preserve_return_constructors_aliases_and_string_identity() {
        let file = syn::parse_file("struct Error;type R=Result<i64,Error>;type Callback=fn()->R;")
            .unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let get = |source: &str| source_type(&syn::parse_str(source).unwrap(), &frame).unwrap();
        assert_eq!(get("R"), get("Result<i64,Error>"));
        assert_ne!(get("R"), get("i64"));
        assert_ne!(get("String"), get("str"));
        assert_eq!(get("Callback"), get("fn()->Result<i64,Error>"));
        assert_ne!(get("Callback"), get("fn()->i64"));
        assert!(source_type(&syn::parse_str("unowned::External").unwrap(), &frame).is_err());
        assert!(source_type(&syn::parse_str("Self").unwrap(), &frame).is_err());
    }
    #[test]
    fn function_item_and_bare_type_use_one_exact_source_signature_mode() {
        let file = syn::parse_file("struct Error;").unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let function: syn::ItemFn =
            syn::parse_str("fn value(input:i64)->Result<i64,Error>{unreachable!()}").unwrap();
        let item = declaration(&function.sig, &frame).unwrap();
        assert_eq!(
            crate::core_ir::rust_callables::typ(item),
            source_type(
                &syn::parse_str("fn(i64)->Result<i64,Error>").unwrap(),
                &frame
            )
            .unwrap()
        );
    }
}
