package sandbox.main

default decision := {
  "allow": false,
  "filesystem": null,
  "baffle_session": null,
}

decision := {
  "allow": true,
  "filesystem": null,
  "baffle_session": null,
} if {
  data.sandbox[input.command].allow
  count(object.keys(input.env)) == 0
}

decision := {
  "allow": true,
  "filesystem": null,
  "baffle_session": null,
} if {
  data.sandbox[input.command].allow
  count(object.keys(input.env)) > 0
  data.sandbox[input.command].allow_env
}
