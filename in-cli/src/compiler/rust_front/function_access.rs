//! Source-only access facts can cross external-module parser threads without syn spans.
use super::type_aliases::{self, Presence};
use crate::core_ir::type_proofs::ResolutionFailure;
use quote::ToTokens;
use std::collections::HashMap;

#[derive(Clone)]
struct Access {
    visibility: String,
    presence: Presence,
    active_bindings: u8,
}
impl Access {
    fn new(visibility: &syn::Visibility, presence: Presence) -> Self {
        Self {
            visibility: visibility.to_token_stream().to_string(),
            presence,
            active_bindings: u8::from(presence == Presence::Active),
        }
    }
}

#[derive(Default)]
pub(super) struct FunctionAccessFacts {
    modules: HashMap<Vec<String>, Access>,
    functions: HashMap<Vec<String>, Access>,
}
impl FunctionAccessFacts {
    pub(super) fn collect(file: &syn::File) -> Self {
        let mut facts = Self::default();
        for item in &file.items {
            let presence = type_aliases::item_presence(item);
            if presence == Presence::Inactive {
                continue;
            }
            match item {
                syn::Item::Fn(item) => Self::insert(
                    &mut facts.functions,
                    vec![item.sig.ident.to_string()],
                    Access::new(&item.vis, presence),
                ),
                syn::Item::Mod(item) => Self::insert(
                    &mut facts.modules,
                    vec![item.ident.to_string()],
                    Access::new(&item.vis, presence),
                ),
                _ => {}
            }
        }
        facts
    }
    fn insert(map: &mut HashMap<Vec<String>, Access>, path: Vec<String>, access: Access) {
        map.entry(path)
            .and_modify(|previous| {
                previous.active_bindings = previous
                    .active_bindings
                    .saturating_add(access.active_bindings);
                if previous.presence != Presence::Active || access.presence != Presence::Active {
                    previous.presence = Presence::Conditional;
                }
            })
            .or_insert(access);
    }
    pub(super) fn merge(&mut self, other: Self) {
        for (path, access) in other.modules {
            Self::insert(&mut self.modules, path, access);
        }
        for (path, access) in other.functions {
            Self::insert(&mut self.functions, path, access);
        }
    }
    pub(super) fn prefixed(self, name: &str) -> Self {
        fn prefix(map: HashMap<Vec<String>, Access>, name: &str) -> HashMap<Vec<String>, Access> {
            map.into_iter()
                .map(|(mut path, access)| {
                    path.insert(0, name.to_owned());
                    (path, access)
                })
                .collect()
        }
        Self {
            modules: prefix(self.modules, name),
            functions: prefix(self.functions, name),
        }
    }
    pub(super) fn merge_prefixed(&mut self, name: &str, other: Self) {
        self.merge(other.prefixed(name));
    }
    pub(super) fn contains_function(&self, target: &str) -> bool {
        self.functions.contains_key(&parts(target))
    }
    pub(super) fn validate(&self, target: &str, caller: &str) -> Option<ResolutionFailure> {
        let target = parts(target);
        let function = self.functions.get(&target)?;
        let caller = parts(caller);
        let caller_module = (1..caller.len())
            .rev()
            .find_map(|length| {
                self.modules
                    .contains_key(&caller[..length])
                    .then_some(&caller[..length])
            })
            .unwrap_or(&[]);
        for length in 1..target.len() {
            let Some(module) = self.modules.get(&target[..length]) else {
                return Some(ResolutionFailure::Unsupported(format!(
                    "source module access is not owned for `{}`",
                    target.join("::")
                )));
            };
            if let Some(failure) = check(
                module,
                &target[..length - 1],
                caller_module,
                &target[..length],
            ) {
                return Some(failure);
            }
        }
        check(
            function,
            &target[..target.len() - 1],
            caller_module,
            &target,
        )
    }
}
fn parts(name: &str) -> Vec<String> {
    name.split("::").map(str::to_owned).collect()
}
fn check(
    access: &Access,
    declared: &[String],
    caller: &[String],
    path: &[String],
) -> Option<ResolutionFailure> {
    if access.active_bindings >= 2 {
        return Some(ResolutionFailure::Invalid(format!(
            "duplicate source function/module `{}`",
            path.join("::")
        )));
    }
    if access.presence != Presence::Active {
        return Some(ResolutionFailure::Unsupported(format!(
            "source function/module `{}` depends on unresolved cfg predicates",
            path.join("::")
        )));
    }
    match type_aliases::source_visibility_permits(&access.visibility, declared, caller) {
        Ok(true) => None,
        Ok(false) => Some(ResolutionFailure::Invalid(format!(
            "private source function/module `{}`",
            path.join("::")
        ))),
        Err(failure) => Some(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts(source: &str) -> FunctionAccessFacts {
        FunctionAccessFacts::collect(&syn::parse_file(source).unwrap())
    }
    #[test]
    fn private_functions_and_modules_follow_caller_scope_after_prefixing() {
        let mut root = facts("mod child;mod sibling;");
        let mut child = facts(
            "fn private(){}pub fn public(){}pub(super) fn parent(){}pub(crate) fn whole(){}mod secret;",
        );
        child.merge_prefixed("secret", facts("pub fn target(){}"));
        root.merge_prefixed("child", child);
        for caller in ["child::own", "child::secret::own", "child::Row::method"] {
            assert!(root.validate("child::private", caller).is_none());
        }
        for caller in ["main", "sibling::call"] {
            assert!(
                root.validate("child::private", caller)
                    .unwrap()
                    .is_invalid()
            );
            assert!(
                root.validate("child::secret::target", caller)
                    .unwrap()
                    .is_invalid()
            );
            for name in ["public", "parent", "whole"] {
                assert!(root.validate(&format!("child::{name}"), caller).is_none());
            }
        }
        assert!(!root.contains_function("child::Row::method"));
        assert!(root.validate("child::Row::method", "main").is_none());
    }
    #[test]
    fn conditional_duplicate_and_type_value_namespaces_stay_distinct() {
        let facts = facts(
            "type answer=u8;pub fn answer(){}#[cfg(feature=\"unknown\")]pub fn conditional(){}#[cfg(any())]fn absent(){}fn repeated(){}fn repeated(){}",
        );
        assert!(facts.contains_function("answer"));
        assert!(facts.validate("answer", "main").is_none());
        assert!(!facts.validate("conditional", "main").unwrap().is_invalid());
        assert!(!facts.contains_function("absent"));
        assert!(facts.validate("repeated", "main").unwrap().is_invalid());
        fn assert_send<T: Send>() {}
        assert_send::<FunctionAccessFacts>();
    }
    #[test]
    fn conditional_alternatives_are_not_definite_duplicates() {
        for source in [
            r#"#[cfg(feature="choice")]fn selected(){}#[cfg(not(feature="choice"))]fn selected(){}"#,
            r#"fn selected(){}#[cfg(feature="choice")]fn selected(){}"#,
            r#"#[cfg(feature="choice")]fn selected(){}fn selected(){}"#,
        ] {
            let facts = facts(source);
            assert!(
                !facts.validate("selected", "main").unwrap().is_invalid(),
                "{source}"
            );
        }
        let facts =
            facts(r#"fn selected(){}#[cfg(feature="choice")]fn selected(){}fn selected(){}"#);
        assert!(facts.validate("selected", "main").unwrap().is_invalid());
    }

    #[test]
    fn restricted_visibility_is_resolved_after_all_prefixes() {
        let mut child = facts("mod inner;");
        child.merge_prefixed(
            "inner",
            facts("pub(in crate::child) fn limited(){}pub(super) fn parent(){}"),
        );
        let mut root = facts("mod child;mod sibling;");
        root.merge_prefixed("child", child);
        for target in ["child::inner::limited", "child::inner::parent"] {
            assert!(root.validate(target, "child::caller").is_none());
            assert!(
                root.validate(target, "sibling::caller")
                    .unwrap()
                    .is_invalid()
            );
        }
    }
}
