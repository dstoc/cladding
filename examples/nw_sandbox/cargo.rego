package sandbox.cargo

default allow = false

default allow_env = false

# Allow: cargo (no args), cargo --help, cargo fetch
allow if {
    count(input.args) == 0
}

allow if {
    input.args == ["--help"]
}

allow if {
    input.args == ["fetch"]
}
