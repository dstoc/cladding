package sandbox.jj

# Keep only helpers used by more than one jj command policy in this file.
agent_code_root := "/home/user/workspace/code-agent"

agent_code_workspace_cwd if {
    startswith(input.cwd, concat("", [agent_code_root, "/"]))
}
