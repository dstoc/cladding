package sandbox.curl

default allow = false
default allow_env = false

allow if {
  input.args == ["-I", "example.com"]
  input.path == "/usr/bin/curl"
}
