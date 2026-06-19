package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) >= 1
    input.args[0] == "abandon"
    valid_jj_operands(array.slice(input.args, 1, count(input.args)))
}
