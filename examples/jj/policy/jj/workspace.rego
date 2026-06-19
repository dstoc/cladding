package sandbox.jj

code_root := "/home/user/workspace/code"

allow if {
    code_repo_cwd

    count(input.args) == 5
    input.args[0] == "workspace"
    input.args[1] == "add"
    valid_agent_workspace_relpath(input.args[2])
    input.args[3] == "--name"

    dir := agent_workspace_dir_from_relpath(input.args[2])
    input.args[4] == concat("", ["agent-", dir])
}

code_repo_cwd if {
    startswith(input.cwd, concat("", [code_root, "/"]))
    parts := split(input.cwd, "/")
    count(parts) == 6
    valid_name_component(parts[5])
}

valid_agent_workspace_relpath(path) if {
    parts := split(path, "/")

    count(parts) == 4
    parts[0] == ".."
    parts[1] == ".."
    parts[2] == "code-agent"
    valid_name_component(parts[3])
}

agent_workspace_dir_from_relpath(path) := dir if {
    parts := split(path, "/")
    dir := parts[3]
}

valid_name_component(name) if {
    regex.match("^[A-Za-z0-9_-]+$", name)
}
