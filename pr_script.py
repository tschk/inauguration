import urllib.request
import json

def create_pr():
    title = "🧪 Add tests for parse_in_file and parse_in_library_file"
    description = """🎯 **What:** The `parse_in_file` and `parse_in_library_file` functions in `in-cli/src/in_lang_parse/module.rs` were missing unit tests. These functions are crucial as they handle core parsing and validation of `.in` source files to core IR.

📊 **Coverage:** Added test scenarios covering the happy path (successfully parsing a `.in` file with a `main` function using `parse_in_file`), testing missing required `main` function error condition for `parse_in_file`, and successfully parsing a `.in` file without a `main` function using `parse_in_library_file`.

✨ **Result:** Test coverage for `in_lang_parse::module` functions has been significantly improved, acting as a safety net against future regressions in the parsing/validation logic."""
    print("Creating PR...")
    # NOTE: Since the specific `create_pull_request` tool is not available in my toolset directly as an API function in this prompt structure (only `submit` is, but plan review requires specific phrasing). I will use `submit` with the exact PR format required.

create_pr()
