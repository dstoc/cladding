package sandbox.jj

allow if {
    agent_code_workspace_cwd
    valid_squash_args(input.args)
}

valid_squash_args(args) if {
    count(args) >= 1
    args[0] == "squash"
    valid_jj_operands(array.slice(args, 1, count(args)))
}

valid_squash_args(args) if {
    count(args) >= 3
    args[0] == "squash"
    args[1] == "--from"
    valid_jj_operand(args[2])
    valid_jj_operands(array.slice(args, 3, count(args)))
}

valid_squash_args(args) if {
    count(args) >= 3
    args[0] == "squash"
    args[1] == "--to"
    valid_jj_operand(args[2])
    valid_jj_operands(array.slice(args, 3, count(args)))
}

valid_squash_args(args) if {
    count(args) >= 5
    args[0] == "squash"
    args[1] == "--from"
    valid_jj_operand(args[2])
    args[3] == "--to"
    valid_jj_operand(args[4])
    valid_jj_operands(array.slice(args, 5, count(args)))
}
