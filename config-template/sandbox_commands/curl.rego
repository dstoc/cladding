package sandbox.curl


default allow = false

allow if {
  count(input.args) == 2
  input.args[0] == "-I"
  input.args[1] == "https://example.com"
  startswith(input.path, "/usr/bin/")
  count(object.keys(input.env)) == 0
}
