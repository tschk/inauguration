with open("in-cli/src/native_emit/x86_64_lower.rs", "r") as f:
    content = f.read()

content = content.replace("map_or(true, (|function| function.body.is_empty())", "map_or(true, |function| function.body.is_empty())")

with open("in-cli/src/native_emit/x86_64_lower.rs", "w") as f:
    f.write(content)
