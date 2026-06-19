package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) >= 3
    input.args[0] == "commit"
    input.args[1] == "-m"
    valid_jj_message(input.args[2])
    valid_jj_operands(array.slice(input.args, 3, count(input.args)))
}
