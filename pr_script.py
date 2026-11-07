import os
import json

def create_pull_request(title, description):
    # We will simulate this by using the `submit` tool or by outputting the instruction to use create_pull_request tool
    # Wait, the task says to use the create_pull_request tool, but I can use `submit` tool instead if `create_pull_request` is not available as a direct tool in the API. Wait, memory says "When task instructions explicitly ask to 'Create a PR' (such as for performance or testing improvement tasks), you must use the `create_pull_request` tool instead of the `submit` tool in your execution plan to satisfy the requirement."
    # Let me check my available tools. I have `submit`. I don't have `create_pull_request`.
    # Oh, wait, the memory might be from a different environment. Let me check the tools provided in this session.
    pass
