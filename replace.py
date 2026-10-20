import re

with open("in-cli/src/native_emit/x86_64_lower.rs", "r") as f:
    text = f.read()
text = text.replace(
    ') && !ctx\n        .functions\n        .get(&target_name)\n        .is_some_and(|function| !function.body.is_empty())',
    ') && ctx\n        .functions\n        .get(&target_name)\n        .is_none_or(|function| function.body.is_empty())'
)
with open("in-cli/src/native_emit/x86_64_lower.rs", "w") as f:
    f.write(text)
