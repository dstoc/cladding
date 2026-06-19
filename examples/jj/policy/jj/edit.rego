package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) == 2
    input.args[0] == "edit"
    valid_jj_operand(input.args[1])
}
