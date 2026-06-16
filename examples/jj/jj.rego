package sandbox.real_jj

default allow = false
default allow_env = false

code_root := "/home/user/workspace/code"
agent_code_root := "/home/user/workspace/code-agent"

allow if {
    jj_executable
    workspace_add
}

allow if {
    jj_executable
    commit
}

allow if {
    jj_executable
    util_snapshot
}

jj_executable if {
    input.command == "real_jj"
    input.path == "/opt/tools/bin/real_jj"
}

# Commands

workspace_add if {
    code_repo_cwd

    count(input.args) == 5
    input.args[0] == "workspace"
    input.args[1] == "add"
    valid_agent_workspace_relpath(input.args[2])
    input.args[3] == "--name"
    valid_agent_workspace_name(input.args[4])

    dir := agent_workspace_dir_from_relpath(input.args[2])
    input.args[4] == concat("", ["agent-", dir])
}

commit if {
    agent_code_workspace_cwd

    count(input.args) == 3
    input.args[0] == "commit"
    input.args[1] == "-m"
    valid_commit_message(input.args[2])
}

util_snapshot if {
    agent_code_workspace_cwd

    count(input.args) == 2
    input.args[0] == "util"
    input.args[1] == "snapshot"
}

### Path util

code_repo_cwd if {
    startswith(input.cwd, concat("", [code_root, "/"]))
    parts := split(input.cwd, "/")
    count(parts) == 6
    valid_name_component(parts[5])
}

agent_code_workspace_cwd if {
    startswith(input.cwd, concat("", [agent_code_root, "/"]))
    # parts := split(input.cwd, "/")
    # count(parts) == 6
    # valid_name_component(parts[5])
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

valid_agent_workspace_name(name) if {
    regex.match("^agent-[A-Za-z0-9_-]+$", name)
}

### Name Validation

valid_name_component(name) if {
    regex.match("^[A-Za-z0-9_-]+$", name)
}

valid_commit_message(message) if {
    count(message) > 0
    count(message) <= 4096
}
