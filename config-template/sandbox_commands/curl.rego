package sandbox.curl


default allow = false
default allow_env = false

allow if {
  count(input.args) == 2
  input.args[0] == "-I"
  input.args[1] == "https://example.com"
  startswith(input.path, "/usr/bin/")
}

# This command intentionally allows no forwarded env vars.
