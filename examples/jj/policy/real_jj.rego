package sandbox.real_jj

default allow = false
default allow_env = false

allow if {
    jj_executable
    data.sandbox.jj.allow
}

jj_executable if {
    input.command == "real_jj"
    input.path == "/opt/tools/bin/real_jj"
}
