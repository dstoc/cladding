# Per-component Baffle session files

Copy `cladding.json` to `.cladding/cladding.json`. Copy the files in
`config/proxy/sessions/` to `.cladding/config/proxy/sessions/`, keeping their
relative paths. The JSON selects a separate session policy for each enabled
component.

Each selected file keeps the stable socket name for its component. Baffle uses
that name to connect the policy to Cladding's existing component socket.

Run `cladding check` before starting the project. It validates the selected
version 2 session files. Edit a selected file and run `cladding reload-proxy`
to apply a policy update. Change `session_config` only while recreating the
proxy runtime.
