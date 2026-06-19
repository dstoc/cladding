package sandbox.jj

# Keep only helpers used by more than one jj command policy in this file.
agent_code_root := "/home/user/workspace/code-agent"

agent_code_workspace_cwd if {
    startswith(input.cwd, concat("", [agent_code_root, "/"]))
}

valid_jj_operand(operand) if {
    count(operand) > 0
    count(operand) <= 4096
    not startswith(operand, "-")
}

valid_jj_operands(operands) if {
    every operand in operands {
        valid_jj_operand(operand)
    }
}

valid_jj_message(message) if {
    count(message) > 0
    count(message) <= 4096
}
