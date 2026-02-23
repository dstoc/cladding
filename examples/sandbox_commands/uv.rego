package sandbox.uv

default allow = false

default allow_env = false

# Allow: uv run --python /usr/bin/python3 pip download -r <requirements.txt> -d ~/pip-cache
allow if {
    count(input.args) == 9
    input.args[0] == "run"
    input.args[1] == "--python"
    input.args[2] == "/usr/bin/python3"
    input.args[3] == "pip"
    input.args[4] == "download"
    input.args[5] == "-r"
    requirement_file(input.args[6])
    input.args[7] == "-d"
    cache_dir(input.args[8])
}

# Allow: uv run --python /usr/bin/python3 pip download <package...> -d ~/pip-cache
allow if {
    count(input.args) >= 8
    input.args[0] == "run"
    input.args[1] == "--python"
    input.args[2] == "/usr/bin/python3"
    input.args[3] == "pip"
    input.args[4] == "download"

    d_idx := count(input.args) - 2
    input.args[d_idx] == "-d"
    cache_dir(input.args[d_idx + 1])

    # Require at least one package spec before -d.
    d_idx > 5

    # Every token between "download" and "-d" must be a safe package spec.
    not invalid_package_spec(d_idx)
}

requirement_file(path) if {
    safe_rel_path(path)
    endswith(path, ".txt")
}

cache_dir(path) if {
    path == "/home/user/pip-cache"
}

safe_rel_path(path) if {
    not startswith(path, "/")
    not startswith(path, "~")
    not contains(path, "..")
    not contains(path, " ")
    not contains(path, "\t")
    not contains(path, "\n")
}

package_spec(spec) if {
    not startswith(spec, "-")
    not contains(spec, "/")
    not contains(spec, "\\")
    not contains(spec, ":")
    not contains(spec, "@")
    not contains(spec, " ")
    not contains(spec, "\t")
    not contains(spec, "\n")
}

invalid_package_spec(d_idx) if {
    some i
    i >= 5
    i <= d_idx - 1
    not package_spec(input.args[i])
}
