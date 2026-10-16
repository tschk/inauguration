# Nim grammar

Based on the MIT-licensed `tree-sitter-nim` 0.1.0 crate from
https://github.com/undivisible/tree-sitter-nim. The published crate declares
MIT licensing but contains no standalone license or copyright notice.
Crate SHA-256: `2f489c09ddd48f97728ca92b8e6b17a1a56e0e1c4c0c5fd1d0222f1dc494c0bb`.

Adds parenthesized expressions and explicit right associativity for declarations
and calls to resolve the resulting parser conflicts. Generated with
`tree-sitter generate --abi 15`, tree-sitter-cli 0.25.10. Consumer builds only
compile the checked-in C parser; no JavaScript runtime or generator is needed.
The generated Tree-sitter header license is retained in TREE-SITTER-LICENSE.

This remains a bounded Nim grammar: whitespace does not implement full Nim
indentation, indirect calls and tuples are unsupported, and binary operators
retain the original grammar precedence. The frontend must still validate ASTs.
