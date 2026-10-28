import sys

def create_pull_request(title, description):
    print(f"Creating PR with title: {title}")
    print(f"Description: {description}")
    # In a real scenario, this would call the GitHub API or a CLI tool
    # to create the PR. Here we just print for simulation.

if __name__ == "__main__":
    create_pull_request(sys.argv[1], sys.argv[2])
