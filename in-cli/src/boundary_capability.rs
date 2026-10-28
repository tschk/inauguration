use crate::language_gates::{self, evaluate_language_gates};
use crate::language_support::LanguageSupport;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BoundaryCapability {
    pub can_parse: bool,
    pub can_lower: bool,
    pub can_typecheck: bool,
    pub can_boundary: bool,
    pub can_jit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageSupportJson<'a> {
    pub language: &'a str,
    pub parser_id: Option<&'a str>,
    pub extensions: &'a [&'a str],
    pub capabilities: &'a [&'a str],
    pub front: &'a str,
    pub runtime_boundary: &'a str,
    pub example: &'a str,
    pub next_step: &'a str,
    pub passed_gates: Vec<&'static str>,
    pub blocking_gates: Vec<&'static str>,
}

fn gate_report_for(entry: &LanguageSupport) -> language_gates::LanguageGateReport {
    evaluate_language_gates(entry, &language_gates::repo_root())
}

#[must_use]
pub fn boundary_capability_for(entry: &LanguageSupport) -> BoundaryCapability {
    BoundaryCapability {
        can_parse: entry.can_parse(),
        can_lower: entry.can_lower(),
        can_typecheck: entry.can_typecheck(),
        can_boundary: entry.can_boundary(),
        can_jit: entry.can_jit(),
    }
}

#[must_use]
pub fn language_support_json(entry: &LanguageSupport) -> LanguageSupportJson<'_> {
    let report = gate_report_for(entry);
    LanguageSupportJson {
        language: entry.language,
        parser_id: entry.parser_id,
        extensions: entry.extensions,
        capabilities: entry.capabilities,
        front: entry.front,
        runtime_boundary: entry.runtime_boundary,
        example: entry.example,
        next_step: entry.next_step,
        passed_gates: report.passed_gates,
        blocking_gates: report.blocking_gates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_support::{all_language_support, language_support_for_parser};
    use crate::parser_registry::ParserId;

    #[test]
    fn in_has_full_capabilities() {
        let entry = language_support_for_parser(ParserId::In.as_str()).expect("in");
        let cap = boundary_capability_for(entry);
        assert!(cap.can_parse);
        assert!(cap.can_lower);
        assert!(cap.can_typecheck);
        assert!(cap.can_boundary);
        assert!(!cap.can_jit);
    }

    #[test]
    fn icore_has_boundary_capability() {
        let entry = language_support_for_parser(ParserId::Icore.as_str()).expect("icore");
        let cap = boundary_capability_for(entry);
        assert!(cap.can_boundary);
        assert!(!cap.can_jit);
    }

    #[test]
    fn rust_has_boundary_and_typecheck() {
        let entry = language_support_for_parser(ParserId::Rust.as_str()).expect("rust");
        let cap = boundary_capability_for(entry);
        assert!(cap.can_boundary);
        assert!(cap.can_typecheck);
    }

    #[test]
    fn php_can_typecheck() {
        let entry = language_support_for_parser(ParserId::Php.as_str()).expect("php");
        let cap = boundary_capability_for(entry);
        assert!(cap.can_lower);
        assert!(cap.can_typecheck);
    }

    #[test]
    fn language_support_json_includes_capabilities() {
        let entry = all_language_support()
            .iter()
            .find(|entry| entry.language == "in")
            .expect("in");
        let json = language_support_json(entry);
        assert!(json.capabilities.contains(&"boundary"));
        assert!(!json.passed_gates.is_empty());
    }

    #[test]
    fn every_language_has_capabilities() {
        for entry in all_language_support() {
            let cap = boundary_capability_for(entry);
            assert!(cap.can_parse, "{}", entry.language);
            assert!(cap.can_lower, "{}", entry.language);
        }
    }
}
