//! Exact source signature identities, before frontend ABI/layout erasure.
//! Namespace lookup and alias substitution remain owned by the shared Catalog.
use super::*;

struct Context {
    id: usize,
    implementation: Rc<syn::ItemImpl>,
    trait_declaration: Rc<syn::ItemTrait>,
    declaring: Frame,
    owner: Typ,
    associated: RefCell<HashSet<String>>,
}
thread_local! { static CONTEXT: RefCell<Option<Rc<Context>>> = const { RefCell::new(None) }; }
struct Guard(Option<Rc<Context>>);
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
    fields.push(leaf(name));
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
    static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let context = Rc::new(Context {
        id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        implementation,
        trait_declaration,
        declaring: declaring.clone(),
        owner: leaf(&format!("nominal:{}", owner.join("::"))),
        associated: RefCell::new(HashSet::new()),
    });
    let _guard = Guard(CONTEXT.with(|state| state.replace(Some(context))));
    let _query = generic_aliases::query()?;
    exact(rhs_type(source, frame, &mut HashSet::new())?)
}
fn exact(value: Resolved) -> Result<Typ, Failure> {
    if let Some(failure) = structural_failure(value.clone()) {
        return Err(failure);
    }
    match value.visibility().0 {
        Resolved::Type(value) if !matches!(value, Typ::Generic(_)) => Ok(value),
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
        leaf(&name)
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
                    return Ok(Resolved::Type(context.owner.clone()));
                }
                if path.path.segments.len() != 2 {
                    return Err(unsupported(
                        "nested associated signature type requires projection proofs",
                    ));
                }
                let member = path.path.segments[1].ident.to_string();
                let declarations: Vec<_> = context
                    .trait_declaration
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
                    let definitions: Vec<_> = context
                        .implementation
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
