//! Implicit associated function dispatch while source scopes still own identities.
//! This is not method-name guessing over flattened IR functions.
use super::*;
use syn::visit::Visit;

mod bounds;

type Identity = Vec<String>;
#[derive(Clone)]
struct Implementation {
    module: Identity,
    item: Rc<syn::ItemImpl>,
    presence: Presence,
}
#[derive(Clone, Default)]
pub(super) struct Index {
    implementations: Vec<Implementation>,
    members: HashMap<String, Vec<usize>>,
    traits: HashMap<Identity, HashSet<String>>,
    default_members: HashSet<String>,
    trait_sources: HashMap<Identity, (Identity, Rc<syn::ItemTrait>)>,
    declared_members: HashMap<Identity, HashMap<String, Presence>>,
    incomplete: bool,
    unmodeled_receivers: HashSet<Identity>,
}
struct NestedImpl(bool);
impl<'ast> Visit<'ast> for NestedImpl {
    fn visit_item_impl(&mut self, _: &'ast syn::ItemImpl) {
        self.0 = true;
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if !mac.path.is_ident("macro_rules") {
            self.0 = true;
        }
    }
}
// Derives may add applicable impls even when nested in cfg_attr. Only a
// predicate proven inactive removes that source of applicability uncertainty.
fn may_derive(meta: &syn::Meta) -> bool {
    if meta.path().is_ident("derive") {
        return true;
    }
    if let syn::Meta::List(list) = meta
        && list.path.is_ident("cfg_attr")
    {
        let Some(arguments) = meta_arguments(list) else {
            return true;
        };
        let Some((predicate, attributes)) = arguments.split_first() else {
            return true;
        };
        return cfg_predicate(predicate) != Presence::Inactive && attributes.iter().any(may_derive);
    }
    false
}
impl Index {
    pub(super) fn generic_bounds(
        &self,
        function: &syn::ItemFn,
        frame: &Frame,
    ) -> Result<Option<Vec<crate::core_ir::rust_generics::TraitObligation>>, Failure> {
        bounds::obligations(self, function, frame)
    }

    pub(super) fn collect(&mut self, items: &[syn::Item], module: &[String], conditional: bool) {
        for item in items {
            let presence = item_presence(item);
            if presence == Presence::Inactive {
                continue;
            }
            match item {
                syn::Item::Mod(item) if item.content.is_none() => self.incomplete = true,
                syn::Item::Macro(item) if !item.mac.path.is_ident("macro_rules") => {
                    self.incomplete = true
                }
                syn::Item::Struct(item) if item.attrs.iter().any(|attr| may_derive(&attr.meta)) => {
                    let mut identity = module.to_vec();
                    identity.push(item.ident.to_string());
                    self.unmodeled_receivers.insert(identity);
                }
                syn::Item::Enum(item) if item.attrs.iter().any(|attr| may_derive(&attr.meta)) => {
                    let mut identity = module.to_vec();
                    identity.push(item.ident.to_string());
                    self.unmodeled_receivers.insert(identity);
                }
                syn::Item::Union(item) if item.attrs.iter().any(|attr| may_derive(&attr.meta)) => {
                    let mut identity = module.to_vec();
                    identity.push(item.ident.to_string());
                    self.unmodeled_receivers.insert(identity);
                }
                syn::Item::Trait(item) => {
                    let mut identity = module.to_vec();
                    identity.push(item.ident.to_string());
                    let mut defaults = HashSet::new();
                    let mut declared_members = HashMap::new();
                    for member in &item.items {
                        if let syn::TraitItem::Fn(method) = member {
                            let presence = attrs_presence(&method.attrs);
                            if presence != Presence::Inactive {
                                declared_members
                                    .entry(method.sig.ident.to_string())
                                    .and_modify(|value| *value = Presence::Conditional)
                                    .or_insert(presence);
                            }
                        }
                    }
                    self.declared_members
                        .insert(identity.clone(), declared_members);
                    for member in &item.items {
                        if let syn::TraitItem::Fn(method) = member
                            && method.default.is_some()
                            && attrs_presence(&method.attrs) != Presence::Inactive
                        {
                            defaults.insert(method.sig.ident.to_string());
                            self.default_members.insert(method.sig.ident.to_string());
                        }
                    }
                    self.trait_sources
                        .insert(identity.clone(), (module.to_vec(), Rc::new(item.clone())));
                    self.traits.insert(identity, defaults);
                }
                syn::Item::Impl(item) => {
                    let index = self.implementations.len();
                    for member in &item.items {
                        if let syn::ImplItem::Fn(method) = member {
                            if attrs_presence(&method.attrs) != Presence::Inactive {
                                self.members
                                    .entry(method.sig.ident.to_string())
                                    .or_default()
                                    .push(index);
                            }
                            let mut nested = NestedImpl(false);
                            nested.visit_block(&method.block);
                            self.incomplete |= nested.0;
                        }
                    }
                    self.implementations.push(Implementation {
                        module: module.to_vec(),
                        item: Rc::new(item.clone()),
                        presence: if conditional {
                            Presence::Conditional
                        } else {
                            presence
                        },
                    });
                }
                syn::Item::Fn(item) => {
                    let mut nested = NestedImpl(false);
                    nested.visit_block(&item.block);
                    self.incomplete |= nested.0;
                }
                syn::Item::Const(item) => {
                    let mut nested = NestedImpl(false);
                    nested.visit_expr(&item.expr);
                    self.incomplete |= nested.0;
                }
                syn::Item::Static(item) => {
                    let mut nested = NestedImpl(false);
                    nested.visit_expr(&item.expr);
                    self.incomplete |= nested.0;
                }
                _ => {}
            }
        }
    }
    fn visible_traits(&self, frame: &Frame) -> Result<HashSet<Identity>, Failure> {
        let module = frame
            .catalog
            .modules
            .get(&frame.module)
            .ok_or_else(|| unsupported("associated function caller scope is not owned"))?;
        if module.conditional || module.glob || frame.locals.iter().any(|scope| scope.glob) {
            return Err(unsupported(
                "associated trait scope depends on unresolved cfg or glob imports",
            ));
        }
        let mut names = HashSet::new();
        for scope in std::iter::once(module.as_ref()).chain(frame.locals.iter().map(Rc::as_ref)) {
            names.extend(scope.bindings.keys().cloned());
            names.extend(scope.imports.keys().cloned());
        }
        let mut visible = HashSet::new();
        for name in names {
            // A type alias to a trait is not a trait import. Keep the actual
            // lexical winner, including local shadows, before identity lookup.
            let winner = frame.locals.iter().rev().find(|scope| {
                scope.bindings.contains_key(&name) || scope.imports.contains_key(&name)
            });
            let binding = winner.map_or_else(
                || module.bindings.get(&name),
                |scope| scope.bindings.get(&name),
            );
            let binding = binding.map(|binding| {
                if let Binding::Visible(value, _) = binding {
                    value.as_ref()
                } else {
                    binding
                }
            });
            if matches!(binding, Some(Binding::Alias(_))) {
                continue;
            }
            let Ok(path) = syn::parse_str::<syn::Path>(&name) else {
                continue;
            };
            match trait_nominal(&path, frame) {
                Ok(Some((identity, _))) if self.traits.contains_key(&identity) => {
                    visible.insert(identity);
                }
                Err(failure)
                    if matches!(binding, Some(Binding::Conditional | Binding::Duplicate))
                        || frame
                            .locals
                            .iter()
                            .any(|scope| scope.imports.contains_key(&name))
                        || module.imports.contains_key(&name) =>
                {
                    return Err(failure);
                }
                _ => {}
            }
        }
        Ok(visible)
    }
    pub(super) fn resolve(
        &self,
        path: &syn::Path,
        frame: &Frame,
    ) -> Option<Result<String, Failure>> {
        if path.segments.len() < 2 {
            return None;
        }
        let mut owner = path.clone();
        let member = owner.segments.pop()?.into_value();
        let name = member.ident.to_string();
        if !self.members.contains_key(&name) && !self.default_members.contains(&name) {
            return None;
        }
        // This seam does not claim modules, primitives, generic parameters, or
        // external paths merely because their last segment matches a method.
        let receiver = path_type_with_mode(
            &owner,
            frame,
            &mut HashSet::new(),
            false,
            false,
            false,
            true,
        )
        .and_then(|value| nominal_result(value, frame));
        let (identity, receiver_generic) = match receiver {
            Ok(Some(value)) => value,
            Err(failure) if failure.is_invalid() => return Some(Err(failure)),
            _ => return None,
        };
        match self.resolve_owned(&identity, receiver_generic, &name, &member.arguments, frame) {
            Ok(Some(symbol)) => Some(Ok(symbol)),
            Ok(None) => None,
            Err(failure) => Some(Err(failure)),
        }
    }
    /// Explicit trait selection uses the same source identities and impl target
    /// spelling as implicit dispatch, without requiring the trait to be imported.
    pub(super) fn resolve_ufcs(
        &self,
        qself: &syn::QSelf,
        path: &syn::Path,
        frame: &Frame,
    ) -> Option<Result<String, Failure>> {
        if qself.position == 0 || path.segments.len() != qself.position + 1 {
            return None;
        }
        let syn::Type::Path(receiver) = qself.ty.as_ref() else {
            return None;
        };
        if receiver.qself.is_some() {
            return None;
        }
        let (owner, generic) = match nominal(&receiver.path, frame) {
            Ok(Some(value)) => value,
            Err(failure) if failure.is_invalid() => return Some(Err(failure)),
            _ => return None,
        };
        let mut trait_path = path.clone();
        let member = trait_path.segments.pop()?.into_value();
        let (trait_id, trait_generic) = match trait_nominal(&trait_path, frame) {
            Ok(Some(value)) if self.traits.contains_key(&value.0) => value,
            Err(failure) if failure.is_invalid() => return Some(Err(failure)),
            _ => return None,
        };
        if let Err(failure) = self.member_role(&trait_id, &member.ident.to_string()) {
            return Some(Err(failure));
        }
        let mut selected = Vec::new();
        for record in &self.implementations {
            let Some((negative, implementation_trait, _)) = &record.item.trait_ else {
                continue;
            };
            let declaring = Frame {
                catalog: frame.catalog.clone(),
                module: record.module.clone(),
                locals: vec![],
            }
            .generics(&record.item.generics);
            let syn::Type::Path(self_type) = record.item.self_ty.as_ref() else {
                continue;
            };
            if self_type.qself.is_some() {
                continue;
            }
            let Ok(Some((receiver_id, receiver_generic))) = nominal(&self_type.path, &declaring)
            else {
                continue;
            };
            let Ok(Some((implementation_id, implementation_generic))) =
                trait_nominal(implementation_trait, &declaring)
            else {
                continue;
            };
            if receiver_id != owner || implementation_id != trait_id {
                continue;
            }
            for item in &record.item.items {
                if let syn::ImplItem::Fn(method) = item
                    && method.sig.ident == member.ident
                    && attrs_presence(&method.attrs) != Presence::Inactive
                {
                    if method.sig.receiver().is_some() {
                        return None;
                    }
                    selected.push((
                        record,
                        method,
                        receiver_generic || implementation_generic || negative.is_some(),
                    ));
                }
            }
        }
        if selected.is_empty() {
            return Some(Err(unsupported(
                "explicit trait static dispatch has no materialized owned implementation",
            )));
        }
        if selected.len() != 1 {
            return Some(Err(
                if selected
                    .iter()
                    .filter(|(record, method, _)| {
                        record.presence == Presence::Active
                            && attrs_presence(&method.attrs) == Presence::Active
                    })
                    .count()
                    > 1
                {
                    Failure::Invalid(
                        "duplicate explicit trait associated function implementations".into(),
                    )
                } else {
                    unsupported("explicit trait dispatch depends on conditional implementations")
                },
            ));
        }
        if generic || trait_generic || !matches!(member.arguments, syn::PathArguments::None) {
            return Some(Err(unsupported(
                "generic explicit trait dispatch requires applicability proofs",
            )));
        }
        let (record, method, unresolved) = selected[0];
        Some(
            self.signature(record, method, frame)
                .and_then(|()| target(record, method, unresolved, frame)),
        )
    }
    fn signature(
        &self,
        record: &Implementation,
        method: &syn::ImplItemFn,
        caller: &Frame,
    ) -> Result<(), Failure> {
        let Some((_, trait_path, _)) = &record.item.trait_ else {
            return Ok(());
        };
        let implementation = Frame {
            catalog: caller.catalog.clone(),
            module: record.module.clone(),
            locals: vec![],
        }
        .generics(&record.item.generics);
        let Some((identity, false)) = trait_nominal(trait_path, &implementation)? else {
            return Err(unsupported("trait signature identity is unresolved"));
        };
        let Some((module, declaration)) = self.trait_sources.get(&identity) else {
            return Err(unsupported("trait signature declaration is not owned"));
        };
        if constrained(&declaration.generics) || constrained(&record.item.generics) {
            return Err(unsupported(
                "generic trait signature requires substitution proofs",
            ));
        }
        let declaring = Frame {
            catalog: caller.catalog.clone(),
            module: module.clone(),
            locals: vec![],
        };
        let signatures: Vec<_> = declaration
            .items
            .iter()
            .filter_map(|item| match item {
                syn::TraitItem::Fn(item)
                    if item.sig.ident == method.sig.ident
                        && attrs_presence(&item.attrs) != Presence::Inactive =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        let [required] = signatures.as_slice() else {
            return Err(unsupported(
                "trait signature has conditional or duplicate declarations",
            ));
        };
        if attrs_presence(&required.attrs) != Presence::Active {
            return Err(unsupported("trait signature depends on unresolved cfg"));
        }
        for signature in [&required.sig, &method.sig] {
            if constrained(&signature.generics)
                || signature.asyncness.is_some()
                || signature.unsafety.is_some()
                || signature.abi.is_some()
                || signature.variadic.is_some()
                || signature.constness.is_some()
            {
                return Err(unsupported(
                    "trait signature has unsupported qualifiers or generic obligations",
                ));
            }
        }
        if required.sig.inputs.len() != method.sig.inputs.len()
            || required.sig.receiver().is_some() != method.sig.receiver().is_some()
        {
            return Err(Failure::Invalid(
                "trait implementation associated function parameter count or receiver differs"
                    .into(),
            ));
        }
        let owner_path = match record.item.self_ty.as_ref() {
            syn::Type::Path(path) if path.qself.is_none() => &path.path,
            _ => return Err(unsupported("trait receiver signature is unresolved")),
        };
        let Some((owner, false)) = nominal(owner_path, &implementation)? else {
            return Err(unsupported("trait receiver signature is generic"));
        };
        let resolve = |source: &syn::Type, scope: &Frame| -> Result<Typ, Failure> {
            source_signatures::resolve(
                source,
                scope,
                record.item.clone(),
                declaration.clone(),
                &implementation,
                &owner,
            )
        };
        for (required, actual) in required.sig.inputs.iter().zip(&method.sig.inputs) {
            match (required, actual) {
                (syn::FnArg::Typed(required), syn::FnArg::Typed(actual)) => {
                    if resolve(&required.ty, &declaring)? != resolve(&actual.ty, &implementation)? {
                        return Err(Failure::Invalid(
                            "trait implementation associated function parameter type differs"
                                .into(),
                        ));
                    }
                }
                (syn::FnArg::Receiver(required), syn::FnArg::Receiver(actual)) => {
                    for receiver in [required, actual] {
                        if receiver.colon_token.is_some()
                            || receiver
                                .reference
                                .as_ref()
                                .is_some_and(|(_, lifetime)| lifetime.is_some())
                        {
                            return Err(unsupported(
                                "trait receiver requires explicit type or lifetime obligations",
                            ));
                        }
                    }
                    let form = |receiver: &syn::Receiver| {
                        (
                            receiver.reference.is_some(),
                            receiver.reference.is_some() && receiver.mutability.is_some(),
                        )
                    };
                    if form(required) != form(actual) {
                        return Err(Failure::Invalid(
                            "trait implementation receiver borrow differs".into(),
                        ));
                    }
                }
                _ => {
                    return Err(Failure::Invalid(
                        "trait implementation receiver position differs".into(),
                    ));
                }
            }
        }
        let output = |value: &syn::ReturnType, scope: &Frame| -> Result<Typ, Failure> {
            match value {
                syn::ReturnType::Default => Ok(Typ::Void),
                syn::ReturnType::Type(_, typ) => resolve(typ, scope),
            }
        };
        if output(&required.sig.output, &declaring)? != output(&method.sig.output, &implementation)?
        {
            return Err(Failure::Invalid(
                "trait implementation associated function return type differs".into(),
            ));
        }
        Ok(())
    }
    fn member_role(&self, trait_id: &Identity, name: &str) -> Result<(), Failure> {
        match self
            .declared_members
            .get(trait_id)
            .and_then(|members| members.get(name))
        {
            Some(Presence::Active) => Ok(()),
            Some(_) => Err(unsupported(
                "trait associated member depends on unresolved cfg predicates",
            )),
            None => Err(Failure::Invalid(
                "associated function is not a declared member of the source trait".into(),
            )),
        }
    }
    fn resolve_owned(
        &self,
        owner: &Identity,
        receiver_generic: bool,
        name: &str,
        arguments: &syn::PathArguments,
        frame: &Frame,
    ) -> Result<Option<String>, Failure> {
        let mut indices = self.members.get(name).cloned().unwrap_or_default();
        if self.default_members.contains(name) {
            indices.extend(0..self.implementations.len());
        }
        indices.sort_unstable();
        indices.dedup();
        let mut inherent = vec![];
        let mut trait_candidates = vec![];
        let mut uncertain_blanket = vec![];
        for index in indices {
            let record = &self.implementations[index];
            let declaring = Frame {
                catalog: frame.catalog.clone(),
                module: record.module.clone(),
                locals: vec![],
            }
            .generics(&record.item.generics);
            let syn::Type::Path(self_type) = record.item.self_ty.as_ref() else {
                continue;
            };
            if self_type.qself.is_some() {
                continue;
            }
            let resolved = nominal(&self_type.path, &declaring);
            let (receiver, receiver_unresolved) =
                match resolved {
                    Ok(Some(value)) => value,
                    _ => {
                        if !record.item.generics.params.is_empty()
                            && self_type.path.segments.len() == 1
                            && record.item.generics.type_params().any(|parameter| {
                                parameter.ident == self_type.path.segments[0].ident
                            })
                        {
                            uncertain_blanket.push(index);
                        }
                        continue;
                    }
                };
            if &receiver != owner {
                continue;
            }
            let methods: Vec<_> = record
                .item
                .items
                .iter()
                .filter_map(|item| {
                    if let syn::ImplItem::Fn(method) = item {
                        (method.sig.ident == name
                            && attrs_presence(&method.attrs) != Presence::Inactive)
                            .then_some(method)
                    } else {
                        None
                    }
                })
                .collect();
            if methods
                .iter()
                .filter(|method| attrs_presence(&method.attrs) == Presence::Active)
                .count()
                > 1
            {
                return Err(Failure::Invalid(
                    "duplicate source impl associated function".into(),
                ));
            }
            let receiver_unresolved = receiver_unresolved || methods.len() > 1;
            let method = methods.first().copied();
            if record.item.trait_.is_none() {
                if let Some(method) = method {
                    inherent.push((record, method, receiver_unresolved));
                }
            } else {
                trait_candidates.push((record, method, receiver_unresolved));
            }
        }
        if !inherent.is_empty() {
            if receiver_generic || !matches!(arguments, syn::PathArguments::None) {
                return Err(unsupported(
                    "generic associated function dispatch requires applicability proofs",
                ));
            }
            if inherent
                .iter()
                .filter(|(record, method, _)| {
                    record.presence == Presence::Active
                        && attrs_presence(&method.attrs) == Presence::Active
                })
                .count()
                > 1
            {
                return Err(Failure::Invalid(
                    "multiple inherent associated functions have the same name".into(),
                ));
            }
            if inherent.len() != 1 {
                return Err(unsupported(
                    "inherent associated function depends on conditional declarations",
                ));
            }
            let (record, method, generic) = inherent[0];
            if method.sig.receiver().is_some() {
                return Ok(None);
            }
            return target(record, method, generic, frame).map(Some);
        }
        if trait_candidates.is_empty() && uncertain_blanket.is_empty() {
            return Ok(None);
        }
        let visible = self.visible_traits(frame)?;
        let mut selected = vec![];
        let mut uncertainty = false;
        let mut out_of_scope = false;
        let mut claimed = false;
        for (record, method, generic) in trait_candidates {
            let declaring = Frame {
                catalog: frame.catalog.clone(),
                module: record.module.clone(),
                locals: vec![],
            }
            .generics(&record.item.generics);
            let Some((negative, trait_path, _)) = &record.item.trait_ else {
                continue;
            };
            claimed |= method.is_some();
            let trait_id = match trait_nominal(trait_path, &declaring) {
                Ok(Some((identity, false))) if self.traits.contains_key(&identity) => identity,
                _ => {
                    uncertainty = true;
                    continue;
                }
            };
            let default = self
                .traits
                .get(&trait_id)
                .is_some_and(|defaults| defaults.contains(name));
            if method.is_none() && !default {
                continue;
            }
            self.member_role(&trait_id, name)?;
            claimed = true;
            if !visible.contains(&trait_id) {
                out_of_scope |= method.is_some();
                continue;
            }
            if negative.is_some() || generic || record.presence != Presence::Active {
                uncertainty = true;
                continue;
            }
            let Some(method) = method else {
                if self
                    .traits
                    .get(&trait_id)
                    .is_some_and(|defaults| defaults.contains(name))
                {
                    uncertainty = true;
                }
                continue;
            };
            if attrs_presence(&method.attrs) != Presence::Active
                || constrained(&record.item.generics)
                || constrained(&method.sig.generics)
            {
                uncertainty = true;
                continue;
            }
            selected.push((record, method));
        }
        for index in uncertain_blanket {
            let record = &self.implementations[index];
            if let Some((_, path, _)) = &record.item.trait_ {
                let declaring = Frame {
                    catalog: frame.catalog.clone(),
                    module: record.module.clone(),
                    locals: vec![],
                }
                .generics(&record.item.generics);
                if nominal(path, &declaring)
                    .ok()
                    .flatten()
                    .is_none_or(|(identity, _)| visible.contains(&identity))
                {
                    uncertainty = true;
                    claimed = true;
                }
            }
        }
        if !claimed && !uncertainty {
            return Ok(None);
        }
        if receiver_generic || !matches!(arguments, syn::PathArguments::None) {
            return Err(unsupported(
                "generic associated function dispatch requires applicability proofs",
            ));
        }
        if selected.len() > 1 {
            return Err(Failure::Invalid(
                "multiple visible trait associated functions are applicable".into(),
            ));
        }
        if uncertainty || self.incomplete || self.unmodeled_receivers.contains(owner) {
            return Err(unsupported(
                "trait associated function applicability is not fully owned by this source",
            ));
        }
        if let Some((record, method)) = selected.pop() {
            self.signature(record, method, frame)?;
            return target(record, method, false, frame).map(Some);
        }
        if out_of_scope {
            return Err(Failure::Invalid(
                "associated function trait is not in the caller's source scope".into(),
            ));
        }
        Err(unsupported(
            "owned associated function has no proven applicable source implementation",
        ))
    }
}
fn constrained(generics: &syn::Generics) -> bool {
    !generics.params.is_empty() || generics.where_clause.is_some()
}
fn trait_nominal(path: &syn::Path, frame: &Frame) -> Result<Option<(Identity, bool)>, Failure> {
    let result = path_type_with_mode(path, frame, &mut HashSet::new(), false, false, true, false)?;
    nominal_result(result, frame)
}
fn nominal(path: &syn::Path, frame: &Frame) -> Result<Option<(Identity, bool)>, Failure> {
    nominal_result(path_type(path, frame, &mut HashSet::new(), false)?, frame)
}
fn nominal_result(result: Resolved, frame: &Frame) -> Result<Option<(Identity, bool)>, Failure> {
    let Resolved::Nominal(obligations) = result.visibility().0 else {
        return Ok(None);
    };
    let mut identity = None;
    let mut generic = false;
    for obligation in obligations {
        match obligation {
            NominalObligation::Application(application) => {
                nominal_application(&application)?;
                if !application.type_extent.permits(&frame.module) {
                    return Err(Failure::Invalid(
                        "private underlying nominal type is not accessible through its alias"
                            .into(),
                    ));
                }
                generic |= constrained(&application.generics)
                    || application.arguments != 0
                    || application.unsupported_arguments;
                if let Some(owner) = application.identity {
                    if identity.as_ref().is_some_and(|previous| previous != &owner) {
                        return Err(unsupported(
                            "associated receiver nominal identity is ambiguous",
                        ));
                    }
                    identity = Some(owner);
                }
            }
            NominalObligation::Failure(failure) => return Err(failure),
            NominalObligation::Projection => return Err(projection_failure()),
        }
    }
    Ok(identity.map(|identity| (identity, generic)))
}
fn target(
    record: &Implementation,
    method: &syn::ImplItemFn,
    generic: bool,
    caller: &Frame,
) -> Result<String, Failure> {
    if record.presence != Presence::Active || attrs_presence(&method.attrs) != Presence::Active {
        return Err(unsupported(
            "associated function requires conditional, generic, or receiver applicability proofs",
        ));
    }
    if record.item.trait_.is_none()
        && !visibility_extent(&method.vis, &record.module)?.permits(&caller.module)
    {
        return Err(Failure::Invalid(
            "private inherent associated function is not accessible".into(),
        ));
    }
    if generic
        || constrained(&record.item.generics)
        || constrained(&method.sig.generics)
        || method.sig.receiver().is_some()
    {
        return Err(unsupported(
            "associated function requires generic or receiver applicability proofs",
        ));
    }
    let symbol = super::super::impl_method_symbol(&record.item, &method.sig.ident);
    Ok(if record.module.is_empty() {
        symbol
    } else {
        format!("{}::{symbol}", record.module.join("::"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lookup(source: &str, path: &str) -> Option<Result<String, Failure>> {
        let mut file = syn::parse_file(source).unwrap();
        // Oracle main only prints/asserts reference_main; candidate fixtures use
        // their scalar entry directly, without that unsupported macro wrapper.
        file.items
            .retain(|item| !matches!(item,syn::Item::Fn(item)if item.sig.ident=="main"));
        let frame = Frame::root(Catalog::new(&file));
        frame
            .catalog
            .trait_statics
            .resolve(&syn::parse_str(path).unwrap(), &frame)
    }
    #[test]
    fn derive_uncertainty_includes_unions_and_conditional_attributes() {
        for declaration in [
            "#[derive(Clone,Copy)] union Row{value:i64}",
            "#[cfg_attr(not(any()),derive(Default))] struct Row;",
            r#"#[cfg_attr(feature="optional",derive(Default))] struct Row;"#,
            "#[cfg_attr(not(any()),cfg_attr(not(any()),derive(Default)))] struct Row;",
        ] {
            let source = format!(
                "{declaration}trait Make{{fn value()->i64;}}impl Make for Row{{fn value()->i64{{42}}}}"
            );
            assert!(
                !lookup(&source, "Row::value")
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
        }
        for declaration in [
            "#[cfg_attr(any(),derive(Default))] struct Row;",
            "#[cfg_attr(not(any()),cfg_attr(any(),derive(Default)))] struct Row;",
        ] {
            let source = format!(
                "{declaration}trait Make{{fn value()->i64;}}impl Make for Row{{fn value()->i64{{42}}}}"
            );
            assert_eq!(
                lookup(&source, "Row::value").unwrap().unwrap(),
                "Row::Make::value"
            );
        }
    }
    #[test]
    fn selected_trait_static_signatures_use_exact_source_types() {
        for source in [
            "trait Make{fn value()->Self::Extra;}struct Row;impl Make for Row{type Extra=i64;fn value()->i64{42}}",
            "trait Make{fn value()->Result<i64,bool>;}struct Row;impl Make for Row{fn value()->i64{42}}",
            "trait Make{fn value()->Result<i64,bool>;}struct Row;impl Make for Row{fn value()->Result<i64,u8>{Ok(42)}}",
            "trait Make{fn value(x:&str)->i64;}struct Row;impl Make for Row{fn value(x:&String)->i64{42}}",
            "type Text=str;type Owned=String;trait Make{fn value(x:&Text)->i64;}struct Row;impl Make for Row{fn value(x:&Owned)->i64{42}}",
            "trait Make{fn value()->bool;}struct Row;impl Make for Row{fn value()->i64{42}}",
            "trait Make{fn value(x:bool)->i64;}struct Row;impl Make for Row{fn value(x:i64)->i64{42}}",
            "trait Make{fn value(x:i64)->i64;}struct Row;impl Make for Row{fn value()->i64{42}}",
            "trait Make{fn value(x:usize)->i64;}struct Row;impl Make for Row{fn value(x:u64)->i64{42}}",
            "mod a{pub struct Item;}mod b{pub struct Item;}trait Make{fn value(x:a::Item)->i64;}struct Row;impl Make for Row{fn value(x:b::Item)->i64{42}}",
        ] {
            assert!(
                lookup(source, "Row::value")
                    .unwrap()
                    .unwrap_err()
                    .is_invalid(),
                "{source}"
            );
        }
        for source in [
            "type R<T,E>=Result<T,E>;trait Make{fn value()->R<i64,bool>;}struct Row;impl Make for Row{fn value()->Result<i64,bool>{Ok(42)}}",
            "type Text=str;trait Make{fn value(x:&Text)->i64;}struct Row;impl Make for Row{fn value(x:&str)->i64{42}}",
            "type Owned=String;trait Make{fn value(x:&Owned)->i64;}struct Row;impl Make for Row{fn value(x:&String)->i64{42}}",
            "type Number=i64;trait Make{fn value(x:Number)->Number;}struct Row;impl Make for Row{fn value(x:i64)->i64{42}}",
            "trait Make{type Output;fn value()->Self::Output;}struct Row;impl Make for Row{type Output=f32;fn value()->Self::Output{42.0}}",
            "mod a{pub struct Item;pub type Alias=Item;pub trait Make{fn value(x:Alias)->i64;}pub struct Row;impl Make for Row{fn value(x:Item)->i64{42}}}use a::Make;use a::Row;",
        ] {
            assert!(lookup(source, "Row::value").unwrap().is_ok(), "{source}");
        }
    }
    #[test]
    fn trait_static_dispatch_requires_declared_trait_member() {
        let conditional = r#"trait Make{#[cfg(feature="value")]fn value()->i64;}struct Row;impl Make for Row{fn value()->i64{42}}"#;
        assert!(
            !lookup(conditional, "Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        let inactive = "trait Make{#[cfg(any())]fn value()->i64;}struct Row;impl Make for Row{fn value()->i64{42}}";
        assert!(
            lookup(inactive, "Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );

        let source = "trait Make{}struct Row;impl Make for Row{fn value()->i64{42}}";
        assert!(
            lookup(source, "Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        let file = syn::parse_file(source).unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let expression: syn::ExprPath = syn::parse_str("<Row as Make>::value").unwrap();
        assert!(
            frame
                .catalog
                .trait_statics
                .resolve_ufcs(expression.qself.as_ref().unwrap(), &expression.path, &frame)
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
    }
    #[test]
    fn trait_position_rejects_type_aliases_but_preserves_use_aliases() {
        let file = syn::parse_file("trait Make{fn value()->i64;}struct Row;impl Make for Row{fn value()->i64{42}}type Alias=Make;mod exports{pub type Bad=crate::Make;}use Make as Imported;use exports::Bad as ImportedBad;").unwrap();
        let frame = Frame::root(Catalog::new(&file));
        for name in ["Alias", "exports::Bad", "ImportedBad"] {
            let path = syn::parse_str(name).unwrap();
            assert!(
                trait_nominal(&path, &frame).unwrap_err().is_invalid(),
                "{name}"
            );
            let expression: syn::ExprPath =
                syn::parse_str(&format!("<Row as {name}>::value")).unwrap();
            assert!(
                frame
                    .catalog
                    .trait_statics
                    .resolve_ufcs(expression.qself.as_ref().unwrap(), &expression.path, &frame)
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
        }
        assert!(
            trait_nominal(&syn::parse_str("Imported").unwrap(), &frame)
                .unwrap()
                .is_some()
        );
        let _scope = frame.enter();
        assert!(
            trait_path_failure(&syn::parse_str("Alias").unwrap())
                .unwrap()
                .is_invalid()
        );
        assert!(trait_path_failure(&syn::parse_str("Imported").unwrap()).is_none());
    }
    #[test]
    fn explicit_trait_selection_uses_owned_impl_target_identity() {
        let source = "mod source{pub trait Factory{fn value()->f32;}pub struct Item;impl Factory for Item{fn value()->f32{42.0}}}type Alias=source::Item;use source::Factory as Make;";
        let file = syn::parse_file(source).unwrap();
        let frame = Frame::root(Catalog::new(&file));
        for expression in [
            "<source::Item as source::Factory>::value",
            "<Alias as Make>::value",
        ] {
            let expression: syn::ExprPath = syn::parse_str(expression).unwrap();
            assert_eq!(
                frame
                    .catalog
                    .trait_statics
                    .resolve_ufcs(expression.qself.as_ref().unwrap(), &expression.path, &frame)
                    .unwrap()
                    .unwrap(),
                "source::Item::Factory::value"
            );
        }
        let file = syn::parse_file("#[derive(Default)]struct Row;trait Make{fn default()->i64;}impl Make for Row{fn default()->i64{42}}").unwrap();
        let frame = Frame::root(Catalog::new(&file));
        let expression: syn::ExprPath = syn::parse_str("<Row as Make>::default").unwrap();
        assert_eq!(
            frame
                .catalog
                .trait_statics
                .resolve_ufcs(expression.qself.as_ref().unwrap(), &expression.path, &frame)
                .unwrap()
                .unwrap(),
            "Row::Make::default"
        );
    }
    #[test]
    fn trait_static_owned_scope_priority_alias_and_cfg_oracles() {
        assert_eq!(lookup("trait Factory { fn value()->f32; } struct Item; impl Factory for Item { fn value()->f32{42.0} } fn reference_main()->i64{if Item::value()==42.0f32{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap(),"Item::Factory::value");
        assert_eq!(lookup("trait Factory { fn value()->f32; } struct Item; impl Factory for Item{fn value()->f32{7.0}} impl Item{fn value()->f32{42.0}} fn reference_main()->i64{if Item::value()==42.0f32{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap(),"Item::value");
        assert_eq!(lookup("mod source{pub trait Factory{fn value()->f32;} pub struct Item; impl Factory for Item{fn value()->f32{42.0}}} use source::Factory as Make; fn reference_main()->i64{if source::Item::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","source::Item::value").unwrap().unwrap(),"source::Item::Factory::value");
        assert_eq!(lookup("trait Factory{fn value()->f32;} struct Item; impl Factory for Item{fn value()->f32{42.0}} type Alias=Item; fn reference_main()->i64{if Alias::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Alias::value").unwrap().unwrap(),"Item::Factory::value");
        assert_eq!(lookup("#[cfg(any())] trait Hidden{fn value()->f32;} trait Factory{fn value()->f32;} struct Item; impl Factory for Item{#[cfg(not(any()))] fn value()->f32{42.0} #[cfg(any())] fn value()->f32{7.0}} fn reference_main()->i64{if Item::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap(),"Item::Factory::value");
        assert_eq!(lookup("trait Factory{fn value()->i64;} struct Item; impl Factory for Item{fn value()->i64{42}} fn reference_main()->i64{Item::value()}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap(),"Item::Factory::value");
        assert!(lookup("trait A{fn value()->f32;} trait B{fn value()->f32;} struct Item; impl A for Item{fn value()->f32{42.0}} impl B for Item{fn value()->f32{7.0}} fn reference_main()->i64{if Item::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap_err().is_invalid());
        assert!(lookup("mod source{pub trait Factory{fn value()->f32;} pub struct Item; impl Factory for Item{fn value()->f32{42.0}}} fn reference_main()->i64{if source::Item::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","source::Item::value").unwrap().unwrap_err().is_invalid());
        assert!(!lookup("#[cfg(feature=\"factory\")] trait Factory{fn value()->f32;} struct Item; #[cfg(feature=\"factory\")] impl Factory for Item{fn value()->f32{42.0}} fn reference_main()->i64{if Item::value()==42.0{42}else{7}}\nfn main(){let actual=reference_main();println!(\"{actual}\");assert_eq!(actual,42);}\n","Item::value").unwrap().unwrap_err().is_invalid());
    }
    #[test]
    fn unrelated_generic_and_receiver_paths_remain_not_applicable() {
        let source = "trait Make{fn value()->i64;}struct Other;impl Make for Other{fn value()->i64{42}}struct Generic<T>{item:T}struct Row;impl Row{fn value(self)->i64{42}}";
        assert!(lookup(source, "Generic::<u8>::value").is_none());
        assert!(lookup(source, "Generic::value").is_none());
        assert!(
            lookup(&format!("{source}type Missing=Generic;"), "Missing::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        assert!(lookup(source, "Row::value").is_none());
        assert!(lookup(source, "external::Other::value").is_none());
    }
    #[test]
    fn conditional_or_ambiguous_trait_imports_are_not_silently_discarded() {
        let prefix = "mod a{pub trait Make{fn value()->i64;}pub struct Row;impl Make for Row{fn value()->i64{42}}}use a::Make;";
        let conditional = format!("{prefix}#[cfg(feature=\"other\")]use a::Make as Other;");
        assert!(
            !lookup(&conditional, "a::Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        let ambiguous = format!(
            "{prefix}mod b{{pub trait Make{{}}}}use a::Make as Other;use b::Make as Other;"
        );
        assert!(
            lookup(&ambiguous, "a::Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
    }
    #[test]
    fn same_impl_conditional_alternatives_never_select_by_order() {
        for methods in [
            "fn value()->i64{42}#[cfg(feature=\"other\")]fn value()->i64{7}",
            "#[cfg(feature=\"other\")]fn value()->i64{7}fn value()->i64{42}",
        ] {
            let source =
                format!("trait Make{{fn value()->i64;}}struct Row;impl Make for Row{{{methods}}}");
            assert!(
                !lookup(&source, "Row::value")
                    .unwrap()
                    .unwrap_err()
                    .is_invalid(),
                "{source}"
            );
        }
        let duplicate = "trait Make{fn value()->i64;}struct Row;impl Make for Row{fn value()->i64{42}fn value()->i64{7}}";
        assert!(
            lookup(duplicate, "Row::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
    }
    #[test]
    fn local_trait_import_shadows_outer_type_alias_and_derive_stays_unproven() {
        let source = "mod source{pub trait Make{fn value()->i64;}pub struct Row;impl Make for Row{fn value()->i64{42}}}type Make=i64;fn owner(){use source::Make;}";
        let file = syn::parse_file(source).unwrap();
        let root = Frame::root(Catalog::new(&file));
        let block: syn::Block = syn::parse_str("{use source::Make;}").unwrap();
        let caller = root.block(&block);
        assert_eq!(
            caller
                .catalog
                .trait_statics
                .resolve(&syn::parse_str("source::Row::value").unwrap(), &caller)
                .unwrap()
                .unwrap(),
            "source::Row::Make::value"
        );
        let derived = "#[derive(Default)]struct Row;trait Make{fn default()->i64;}impl Make for Row{fn default()->i64{42}}";
        assert!(
            !lookup(derived, "Row::default")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        let inherent = format!("{derived}impl Row{{fn default()->i64{{42}}}}");
        assert_eq!(
            lookup(&inherent, "Row::default").unwrap().unwrap(),
            "Row::default"
        );
    }
    #[test]
    fn nominal_alias_access_checks_type_extent_without_rejecting_hidden_module_reexports() {
        let private = "mod hidden{pub trait Make{fn value()->i64;}struct Row;impl Make for Row{fn value()->i64{42}}pub type Alias=Row;}use hidden::Make;";
        assert!(
            lookup(private, "hidden::Alias::value")
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        let public = "mod hidden{pub struct Row;}pub type Alias=hidden::Row;trait Make{fn value()->i64;}impl Make for hidden::Row{fn value()->i64{42}}";
        assert_eq!(
            lookup(public, "Alias::value").unwrap().unwrap(),
            "hidden::Row::Make::value"
        );
    }
    #[test]
    fn unexpanded_impl_macros_make_only_claimed_trait_applicability_unowned() {
        for invocation in ["add!();", "fn helper(){add!();}"] {
            let source = format!(
                "struct Row;trait Make{{fn value()->i64;}}impl Make for Row{{fn value()->i64{{42}}}}macro_rules! add{{()=>{{impl Row{{}}}}}}{invocation}"
            );
            assert!(
                !lookup(&source, "Row::value")
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
            let inherent = format!("{source}impl Row{{fn value()->i64{{42}}}}");
            assert_eq!(
                lookup(&inherent, "Row::value").unwrap().unwrap(),
                "Row::value"
            );
        }
        let definition_only = "struct Row;trait Make{fn value()->i64;}impl Make for Row{fn value()->i64{42}}macro_rules! unused{()=>{impl Row{}}}";
        assert_eq!(
            lookup(definition_only, "Row::value").unwrap().unwrap(),
            "Row::Make::value"
        );
        let unrelated = "#[derive(Default)]struct Other;struct Row;trait Make{fn value()->i64;}impl Make for Row{fn value()->i64{42}}";
        assert_eq!(
            lookup(unrelated, "Row::value").unwrap().unwrap(),
            "Row::Make::value"
        );
    }
}
