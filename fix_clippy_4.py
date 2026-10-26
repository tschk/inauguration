import re

with open('in-cli/src/compiler/rust_front/type_aliases/generic_aliases.rs', 'r') as f:
    content = f.read()

# Make sure we only do it if the line is exactly matched.
content = content.replace(
    'let parameters = parameters.map(|source| child(source)).collect();',
    'let parameters = parameters.map(&mut child).collect();'
)

with open('in-cli/src/compiler/rust_front/type_aliases/generic_aliases.rs', 'w') as f:
    f.write(content)
