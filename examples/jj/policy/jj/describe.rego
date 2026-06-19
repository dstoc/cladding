package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) == 3
    valid_describe_command(input.args[0])
    input.args[1] == "-m"
    valid_jj_message(input.args[2])
}

allow if {
    agent_code_workspace_cwd

    count(input.args) == 4
    valid_describe_command(input.args[0])
    valid_jj_operand(input.args[1])
    input.args[2] == "-m"
    valid_jj_message(input.args[3])
}

valid_describe_command(command) if {
    command == "desc"
}

valid_describe_command(command) if {
    command == "describe"
}
