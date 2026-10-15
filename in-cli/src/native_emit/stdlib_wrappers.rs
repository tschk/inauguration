//! Shared scalar C-ABI wrapper identity and validation. Signatures remain in Core IR's catalog.
use crate::core_ir::Typ;

const WRAPPERS: &[(&str, &str)] = &[
    ("join", "in_vec_join"),
    ("chain", "in_vec_extend"),
    ("extend", "in_vec_extend"),
    ("push", "in_vec_push"),
    ("push-str", "in_vec_push_words"),
    ("print", "in_print"),
    ("read-file", "in_fs_read_to_string"),
    ("write-file", "in_fs_write"),
    ("fs-exists", "in_fs_exists"),
    ("create-dir", "in_fs_create_dir"),
    ("remove-file", "in_fs_remove_file"),
    ("process-run", "in_process_run"),
    ("env-get", "in_env_var"),
    ("env-set", "in_env_set_var"),
    ("env-remove", "in_env_remove_var"),
    ("env-has", "in_env_has"),
    ("env-temp-dir", "in_env_temp_dir"),
    ("env-current-dir", "in_env_current_dir"),
    ("path-join", "in_path_join"),
    ("path-dirname", "in_path_dirname"),
    ("path-basename", "in_path_basename"),
    ("path-extname", "in_path_extname"),
    ("path-normalize", "in_path_normalize"),
    ("str-concat", "in_str_concat"),
    ("str-eq", "in_str_eq"),
    ("json-stringify", "in_json_stringify"),
    ("str-table-has", "in_str_table_has"),
    ("str-table-get-int", "in_str_table_get_int"),
    ("str-contains", "in_str_contains"),
    ("str-starts-with", "in_str_starts_with"),
    ("str-ends-with", "in_str_ends_with"),
    ("str-index-of", "in_str_index_of"),
    ("str-is-int", "in_str_is_int"),
    ("str-slice", "in_str_slice"),
    ("str-split-lines", "in_str_split_lines"),
    ("str-split-spaces", "in_str_split_spaces"),
    ("str-tokenize-expr", "in_str_tokenize_expr"),
    ("str-to-int", "in_str_to_int"),
    ("str-trim", "in_str_trim"),
    ("array-len", "in_array_len"),
    ("print-int", "in_print_int"),
    ("to-string", "in_int_to_string"),
    ("std::env::var", "in_env_var"),
    ("std::env::temp_dir", "in_env_temp_dir"),
    ("std::env::current_dir", "in_env_current_dir"),
    ("std::env::set_var", "in_env_set_var"),
    ("std::env::remove_var", "in_env_remove_var"),
    ("std::fs::read_to_string", "in_fs_read_to_string"),
    ("std::fs::exists", "in_fs_exists"),
    ("std::fs::write", "in_fs_write"),
    ("std::fs::create_dir", "in_fs_create_dir"),
    ("std::fs::remove_file", "in_fs_remove_file"),
];

pub(crate) fn wrapper(name: &str) -> Option<&'static str> {
    WRAPPERS
        .iter()
        .find_map(|(surface, wrapper)| (*surface == name).then_some(*wrapper))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AbiMismatch {
    Declaration,
    Arguments,
}
impl AbiMismatch {
    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::Declaration => "declaration",
            Self::Arguments => "arguments",
        }
    }
}

/// Check the selected wrapper, regardless of how its source callee was spelled.
/// Helpers without a scalar catalog keep their separate generic lowering contract.
pub(crate) fn validate_scalar_call(
    wrapper: &str,
    declared: Option<(&[(String, Typ)], &Typ)>,
    actual: impl ExactSizeIterator<Item = Option<Typ>>,
) -> Result<(), AbiMismatch> {
    let intrinsic = WRAPPERS
        .iter()
        .find_map(|(name, candidate)| (*candidate == wrapper).then_some(*name));
    let params = intrinsic.and_then(crate::core_ir_verifier::intrinsic_param_types);
    let ret = intrinsic.and_then(crate::core_ir_verifier::intrinsic_return_type);
    if let Some((declared_params, declared_ret)) = declared {
        if params.is_none_or(|params| {
            declared_params.len() != params.len()
                || declared_params
                    .iter()
                    .zip(params)
                    .any(|((_, actual), expected)| actual.canonical() != expected.canonical())
        }) || ret.is_none_or(|ret| declared_ret.canonical() != ret.canonical())
        {
            return Err(AbiMismatch::Declaration);
        }
    }
    // Generic vector/array helpers have separate layout checks in their lowering paths.
    if (params.is_none() || ret.is_none())
        && !matches!(
            wrapper,
            "in_vec_join" | "in_vec_extend" | "in_vec_push" | "in_vec_push_words" | "in_array_len"
        )
    {
        return Err(AbiMismatch::Arguments);
    }
    if let Some(params) = params {
        if actual.len() != params.len()
            || actual.zip(params).any(|(actual, expected)| {
                actual.is_none_or(|actual| actual.canonical() != expected.canonical())
            })
        {
            return Err(AbiMismatch::Arguments);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_wrapper_checks_catalog_aliases_declarations_and_actual_arguments() {
        let aliases = vec![("text".into(), Typ::Named("String".into()))];
        assert!(
            validate_scalar_call(
                "in_process_run",
                Some((&aliases, &Typ::String)),
                [Some(Typ::String)].into_iter()
            )
            .is_ok()
        );
        assert_eq!(
            validate_scalar_call("in_env_temp_dir", Some((&[], &Typ::Float)), [].into_iter()),
            Err(AbiMismatch::Declaration)
        );
        assert_eq!(
            validate_scalar_call("in_process_run", None, [Some(Typ::Int)].into_iter()),
            Err(AbiMismatch::Arguments)
        );
        assert_eq!(
            validate_scalar_call("in_process_run", None, [].into_iter()),
            Err(AbiMismatch::Arguments)
        );
        for (wrapper, count) in [
            ("in_env_remove_var", 1),
            ("in_str_table_has", 2),
            ("in_str_table_get_int", 3),
        ] {
            assert!(
                validate_scalar_call(wrapper, None, vec![Some(Typ::String); count].into_iter())
                    .is_ok()
            );
            assert_eq!(
                validate_scalar_call(wrapper, None, vec![Some(Typ::Int); count].into_iter()),
                Err(AbiMismatch::Arguments)
            );
        }
        assert_eq!(
            validate_scalar_call("unknown_scalar_wrapper", None, [Some(Typ::Int)].into_iter()),
            Err(AbiMismatch::Arguments)
        );
        assert_eq!(
            validate_scalar_call("in_vec_join", Some((&[], &Typ::Int)), [].into_iter()),
            Err(AbiMismatch::Declaration)
        );
    }
}
