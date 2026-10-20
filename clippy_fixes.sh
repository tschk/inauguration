#!/bin/bash
sed -i 's/if !matches!(specifier.kind(), "primitive_type" | "sized_type_specifier")/if !(matches!(specifier.kind(), "primitive_type" | "sized_type_specifier")/' in-cli/src/compiler/tree_front/c_family.rs
sed -i 's/&& !(specifier.kind() == "type_identifier" && node_txt(src, specifier) == "_Bool")/|| (specifier.kind() == "type_identifier" \&\& node_txt(src, specifier) == "_Bool"))/' in-cli/src/compiler/tree_front/c_family.rs

sed -i 's/if !matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool)/if !(matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool)/' in-cli/src/native_emit/lower/lower_expr.rs
sed -i 's/&& !(count == 0 && elem.canonical() == Typ::Void)/|| (count == 0 \&\& elem.canonical() == Typ::Void))/' in-cli/src/native_emit/lower/lower_expr.rs

sed -i 's/) && !ctx/) \&\& ctx/' in-cli/src/native_emit/x86_64_lower.rs
sed -i 's/\.is_some_and(|function| !function.body.is_empty())/\.is_none_or(|function| function.body.is_empty())/' in-cli/src/native_emit/x86_64_lower.rs
