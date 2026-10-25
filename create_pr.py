import os
import json

def main():
    payload = {
        "title": "🧪 Add test for base_report",
        "description": "🎯 **What:** Add a unit test for base_report function.\n\n📊 **Coverage:** Tests base_report formatting logic with input/output matching.\n\n✨ **Result:** Increased test coverage for owned_compile report module."
    }
    with open("pr_payload.json", "w") as f:
        json.dump(payload, f)

if __name__ == "__main__":
    main()
