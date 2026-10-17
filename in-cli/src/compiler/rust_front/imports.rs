//! Syntax-only import leaves; callers retain their own name-resolution policy.
use std::collections::HashMap;

pub(super) type Import = Option<(Vec<String>, bool)>;

/// Record explicit leaves and return whether the tree contains a glob.
/// Duplicate names stay ambiguous; globs do not invent explicit bindings.
pub(super) fn import_leaves(
    tree: &syn::UseTree,
    prefix: &[String],
    absolute: bool,
    out: &mut HashMap<String, Import>,
) -> bool {
    let mut insert = |alias: String, path: Vec<String>| {
        out.entry(alias)
            .and_modify(|value| *value = None)
            .or_insert(Some((path, absolute)));
    };
    match tree {
        syn::UseTree::Path(value) => {
            let mut path = prefix.to_vec();
            path.push(value.ident.to_string());
            import_leaves(&value.tree, &path, absolute, out)
        }
        syn::UseTree::Name(value) if value.ident == "self" => {
            if let Some(alias) = prefix.last() {
                insert(alias.clone(), prefix.to_vec());
            }
            false
        }
        syn::UseTree::Name(value) => {
            let mut path = prefix.to_vec();
            path.push(value.ident.to_string());
            insert(value.ident.to_string(), path);
            false
        }
        syn::UseTree::Rename(value) => {
            let mut path = prefix.to_vec();
            if value.ident != "self" {
                path.push(value.ident.to_string());
            }
            insert(value.rename.to_string(), path);
            false
        }
        syn::UseTree::Group(value) => {
            let mut glob = false;
            for item in &value.items {
                // Do not short-circuit: later explicit leaves still matter.
                glob |= import_leaves(item, prefix, absolute, out);
            }
            glob
        }
        syn::UseTree::Glob(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(source: &str) -> (HashMap<String, Import>, bool) {
        let item: syn::ItemUse = syn::parse_str(source).unwrap();
        let mut imports = HashMap::new();
        let glob = import_leaves(&item.tree, &[], item.leading_colon.is_some(), &mut imports);
        (imports, glob)
    }

    fn path(parts: &[&str], absolute: bool) -> Import {
        Some((
            parts.iter().map(|part| (*part).to_string()).collect(),
            absolute,
        ))
    }

    #[test]
    fn rename_group_and_self_leaves_keep_exact_paths() {
        let (imports, glob) = collect(
            "use crate::types::{self, self as module, Byte as Octet, nested::{Word, Half as Small}};",
        );
        assert!(!glob);
        assert_eq!(imports.len(), 5);
        assert_eq!(imports["types"], path(&["crate", "types"], false));
        assert_eq!(imports["module"], path(&["crate", "types"], false));
        assert_eq!(imports["Octet"], path(&["crate", "types", "Byte"], false));
        assert_eq!(
            imports["Word"],
            path(&["crate", "types", "nested", "Word"], false)
        );
        assert_eq!(
            imports["Small"],
            path(&["crate", "types", "nested", "Half"], false)
        );
    }

    #[test]
    fn duplicate_leaves_remain_ambiguous_after_later_definitions() {
        let (imports, glob) =
            collect("use crate::{one::Byte, two::Word as Byte, three::Byte, Kept};");
        assert!(!glob);
        assert_eq!(imports["Byte"], None);
        assert_eq!(imports["Kept"], path(&["crate", "Kept"], false));
    }

    #[test]
    fn absolute_import_preserves_rootedness_for_each_leaf() {
        let (imports, glob) = collect("use ::core::primitive::{u8 as Byte, u16};");
        assert!(!glob);
        assert_eq!(imports["Byte"], path(&["core", "primitive", "u8"], true));
        assert_eq!(imports["u16"], path(&["core", "primitive", "u16"], true));
    }

    #[test]
    fn globs_do_not_skip_later_group_leaves_or_invent_bindings() {
        let (imports, glob) = collect("use crate::{first::*, Middle, later::{*, Last}};");
        assert!(glob);
        assert_eq!(imports.len(), 2);
        assert_eq!(imports["Middle"], path(&["crate", "Middle"], false));
        assert_eq!(imports["Last"], path(&["crate", "later", "Last"], false));
    }
}
