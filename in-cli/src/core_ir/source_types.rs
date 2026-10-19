//! Private source nominal identities, independent of runtime layout carriers.
use super::Typ;
const NOMINAL: &str = "#rust-source-signature:nominal:";
pub(crate) fn nominal(identity: &str) -> Typ {
    Typ::Named(format!("{NOMINAL}{identity}"))
}
pub(crate) fn nominal_identity(name: &str) -> Option<&str> {
    name.strip_prefix(NOMINAL)
        .filter(|identity| !identity.is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_nominal_source_tags_expose_a_namespace_identity() {
        let Typ::Named(name) = nominal("a::Record") else {
            panic!()
        };
        assert_eq!(nominal_identity(&name), Some("a::Record"));
        assert_eq!(
            nominal_identity("#rust-source-signature:std:string:String"),
            None
        );
        assert_eq!(
            nominal_identity("#rust-source-signature:primitive:str"),
            None
        );
        assert_eq!(nominal_identity(NOMINAL), None);
        assert_eq!(nominal_identity("Record"), None);
    }
}
