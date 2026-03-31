# Notes: Cladding Expose Port Proxy

## Research Scope
- Feature request: add `cladding expose start <port>` to proxy a port from `cli-pod-cli-app` out to the host.
- Requested behavior:
  - only support ports on `cli-pod-cli-app`; no support for other containers.
  - provide similar lifecycle commands: `start`, `stop`, `list`.
  - ensure exposed-port proxy containers are also shut down when `cladding down` runs.
  - use `./expose.sh` only as a temporary reference; summarize useful design/implementation details rather than treating it as final product behavior.
- User suggestion: the proxy container may run with `--network "container:$BASE_CONTAINER"` so it shares the target container network namespace.

## Existing Product Findings

### CLI surface and command model
- File: `cladding/src/cli.rs`
- Current top-level commands are:
  - `build`
  - `init`
  - `check`
  - `up`
  - `down`
  - `destroy`
  - `run`
  - `run-with-scissors`
  - `reload-proxy`
  - `ps`
- There is no existing nested subcommand pattern, so adding `expose start|stop|list` will introduce the first subcommand group under a top-level command.
- `cladding` uses `clap` derive macros for argument parsing.

### Current runtime and lifecycle model
- `cladding up`:
  - loads config from `.cladding/cladding.json`
  - resolves/allocates a `cladding-N` network at runtime
  - renders `pods.yaml`
  - starts pods through `podman play kube`
- `cladding down`:
  - resolves the active project network from currently running pods
  - renders `pods.yaml`
  - stops the three managed pods through `podman play kube --down`
- `cladding destroy`:
  - force-removes only the three pod names derived from `NetworkSettings`
  - does not clean up any extra standalone containers
- Current cleanup therefore only knows about the proxy pod, sandbox pod, and cli pod. Any new standalone expose container will require explicit cleanup logic.

### How cladding identifies running projects
- File: `cladding/src/podman.rs`
- `list_running_projects()` and `list_running_project_networks()` derive project identity from Podman pod labels:
  - `cladding=<project name>`
  - `project_root=<canonical .cladding path>`
- These helpers only inspect running pods created by `podman pod ps --filter label=cladding --filter status=running`.
- Standalone containers are not part of this discovery model today.

### Current container naming and active-container lookup
- File: `cladding/src/network.rs`
- For a project named `<name>`, pod names are derived as:
  - `<name>-proxy-pod`
  - `<name>-sandbox-pod`
  - `<name>-cli-pod`
- In `cmd_run`, the target cli container name is computed as `<cli_pod_name>-cli-app`.
- For this feature, the only supported target container is therefore the existing `cli-app` container for the current project: `<name>-cli-pod-cli-app`.

### Network/runtime assumptions in the current product
- `pods.yaml` gives `cli-app` no published host ports.
- `cli-app` and `sandbox-app` rely on fixed in-network hostnames and IPs inside the selected `cladding-N` network.
- README states direct egress from `cli-pod` is blocked except to:
  - `sandbox-pod:3000`
  - `proxy-pod:8080`
  - `host.containers.internal` on explicitly configured ports
- Exposing a port to the host is therefore an intentional escape hatch for host access, and should remain explicit and narrow.

### Existing docs and conventions
- README “Useful Commands” lists only current top-level commands and has no existing section for exposing ports.
- Existing PRDs under `docs/features/*/prd.md` use sections:
  - Objective
  - Use Cases
  - Functional Requirements
  - Non-Goals
  - Design Considerations
  - Technical Considerations
  - Success Metrics
  - Open Questions

### Build and image model constraints
- File: `Containerfile.cladding`
- Current default image is a single general-purpose image used for `cli_image` and `sandbox_image` defaults.
- File: `cladding/src/assets.rs`
- `cladding build` currently refreshes embedded tools and builds only the main cladding image(s); there is no helper image build pipeline for auxiliary containers.
- Implication for this feature:
  - using `alpine/socat` exactly as in `./expose.sh` introduces a runtime image dependency outside the current `cladding build` workflow.
  - user decision: use `alpine/socat` directly rather than adding a cladding-managed helper image.
  - implementation should therefore treat helper-image availability/pull behavior as part of runtime command execution rather than image build setup.

## Temporary Script Findings (`./expose.sh`)
- The script is named around `expose-port`, not `cladding expose`, so its UX is only a loose reference.
- Script behavior:
  - `start <internal_port> [base_container] [start_host_port]`
  - `stop <proxy_container_name>`
  - `list`
- The script defaults:
  - base container: `v1-cli-pod-cli-app`
  - starting host port: `8080`
- Current script implementation:
  - inspects the base container IP
  - scans for a free host port, incrementing from the requested starting host port
  - starts a detached `alpine/socat` container with:
    - `--name "$PROXY_NAME"`
    - `-p "$HOST_PORT:$INTERNAL_PORT"`
    - `TCP-LISTEN:$INTERNAL_PORT,fork,reuseaddr`
    - `TCP:$BASE_IP:$INTERNAL_PORT`
  - names proxies like `proxy-${BASE_CONTAINER}-${INTERNAL_PORT}-${HOST_PORT}`
  - lists proxies with `podman ps --filter "name=^proxy-"`
- Useful ideas from the script:
  - auto-selecting an available host port is useful when the requested/default host port is occupied.
  - container naming should encode enough information to make `list` and `stop` understandable.
  - a detached `socat` container is a reasonable implementation direction.
- Limitations of the script relative to product needs:
  - it is not project-aware.
  - it has no labels for current project or project root.
  - it relies on target container IP discovery rather than integration with cladding runtime metadata.
  - it does not integrate with `cladding down` cleanup.
  - it does not prove whether `--network container:<base_container>` plus published host ports is valid for the chosen Podman setup.

## Implementation Constraints and Risks
- Podman is not installed in the current workspace environment, so local command validation is unavailable.
- As a result, exact Podman runtime details for:
  - `--network container:<container>`
  - whether `-p` may be combined with that mode
  - what inspect fields are most reliable for reuse/listing
  are not confirmed from local execution and should be treated as design items to validate during implementation.

## Initial Product Shape Inferred From Request
- CLI likely becomes:
  - `cladding expose <containerport> [hostport]`
  - `cladding expose stop ...`
  - `cladding expose list`
- Since only `cli-pod-cli-app` is in scope, the command does not need a container selector.
- User decision:
  - the primary create command should treat the first positional port as the container port, not a strict host-port binding.
  - host port may be optional and can be auto-selected.
  - `start` may be omitted from the final CLI surface.
- `stop` has at least two plausible UX shapes:
  - stop by the exposed host port
  - stop by generated expose container name/id
- User decision:
  - `cladding expose stop` should identify proxies by host port.
  - if a mapping already exists for a given container port in the current project, a new `cladding expose <containerport> [hostport]` should fail rather than create an additional mapping.
  - when `[hostport]` is omitted, host-port selection should start from the same numeric value as `<containerport>` and increment until a free port is found.
  - `cladding destroy` should remove expose proxies too, not only `cladding down`.
  - use `alpine/socat` directly for the helper container rather than a custom helper image.
- `list` likely needs to show enough metadata to support `stop` and to understand current mappings:
  - host port
  - target container/internal port
  - status
  - maybe project name/project root

## Gaps / Ambiguities To Resolve
- No additional user decisions required before drafting the PRD.
- Remaining implementation-level unknowns:
  - whether Podman allows `-p` together with `--network container:<base_container>` in the target environments.
