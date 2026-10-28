import json

request = {
    "title": "🧪 Add missing tests for foreign_stage.rs load_manifest",
    "description": "🎯 **What:** The testing gap for loading and parsing foreign stage manifests was addressed. 📊 **Coverage:** Scenarios for parsing valid manifests and handling empty project names are now tested. ✨ **Result:** The test coverage for foreign_stage.rs load_manifest is improved, ensuring robustness of manifest parsing.",
    "branch_name": "add-foreign-stage-tests",
    "commit_message": "Add tests for foreign_stage.rs load_manifest"
}

with open("request.json", "w") as f:
    json.dump(request, f)
