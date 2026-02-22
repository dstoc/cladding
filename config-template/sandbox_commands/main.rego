package sandbox.main


default allow = false

allow if {
  data.sandbox[input.command].allow
}
