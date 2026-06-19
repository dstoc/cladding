package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) == 1
    input.args[0] == "new"
}

allow if {
    agent_code_workspace_cwd

    count(input.args) == 2
    input.args[0] == "new"
    valid_jj_operand(input.args[1])
}
