import json
import sys

def create_pull_request(title, description):
    print(f"Creating pull request with title: {title}")
    print(f"Description:\n{description}")

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: python create_pr.py <title> <description>")
        sys.exit(1)

    title = sys.argv[1]
    description = sys.argv[2]
    create_pull_request(title, description)
