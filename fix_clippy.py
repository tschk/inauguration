import re

with open("in-cli/src/compiler/tree_front/c_family.rs", "r") as f:
    c_family = f.read()

c_family = c_family.replace(
    'if !matches!(specifier.kind(), "primitive_type" | "sized_type_specifier")\n        && !(specifier.kind() == "type_identifier" && node_txt(src, specifier) == "_Bool")',
    'if !(matches!(specifier.kind(), "primitive_type" | "sized_type_specifier")\n        || specifier.kind() == "type_identifier" && node_txt(src, specifier) == "_Bool")'
)

with open("in-cli/src/compiler/tree_front/c_family.rs", "w") as f:
    f.write(c_family)

with open("in-cli/src/native_emit/lower/lower_expr.rs", "r") as f:
    lower_expr = f.read()

lower_expr = lower_expr.replace(
    'if !matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool)\n        && !(count == 0 && elem.canonical() == Typ::Void)',
    'if !(matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool)\n        || count == 0 && elem.canonical() == Typ::Void)'
)
with open("in-cli/src/native_emit/lower/lower_expr.rs", "w") as f:
    f.write(lower_expr)
