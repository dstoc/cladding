package sandbox.jj

allow if {
    agent_code_workspace_cwd

    count(input.args) == 2
    input.args[0] == "util"
    input.args[1] == "snapshot"
}
