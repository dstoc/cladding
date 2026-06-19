package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) == 3
    input.args[0] == "commit"
    input.args[1] == "-m"
    valid_commit_message(input.args[2])
}

valid_commit_message(message) if {
    count(message) > 0
    count(message) <= 4096
}
