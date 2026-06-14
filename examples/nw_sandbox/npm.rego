package sandbox.npm

default allow = false

default allow_env = false

allow if {
    input.args[0] == "install"
    input.args[1] == "--no-scripts"
    input.args[2] == "--"
    pkgs := input.args[3:]
    all_pkgs(pkgs)
}

allow if {
    input.args == ["install", "--no-scripts"]
}

allow if {
    input.args[0] == "install"
    input.args[1] == "--no-scripts"
    input.args[2] == "--save"
    input.args[3] == "--"
    pkgs := input.args[4:]
    all_pkgs(pkgs)
}

allow if {
    input.args[0] == "install"
    input.args[1] == "--no-scripts"
    input.args[2] == "--save-dev"
    input.args[3] == "--"
    pkgs := input.args[4:]
    all_pkgs(pkgs)
}

all_pkgs(pkgs) {
    every i in pkgs {
        not startswith(pkgs[i], "-")
    }
}
