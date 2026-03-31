# PRD: Cladding Expose Port Proxy

## Objective
Add a project-scoped port exposure feature to `cladding` that lets a developer publish a port from the current project’s `cli-pod-cli-app` container to the host.

The feature should provide a simple CLI for creating, listing, and removing exposed ports, while keeping the lifecycle managed by `cladding`:
- `cladding expose <containerport> [hostport]`
- `cladding expose stop <hostport>`
- `cladding expose list`

Exposed-port proxy containers must be cleaned up automatically when the project is stopped with `cladding down` or force-removed with `cladding destroy`.

## Use Cases
1. A developer wants temporary host access to a service running inside `cli-pod-cli-app` without changing `pods.yaml` or permanently publishing ports.
2. A developer runs `cladding expose 3000` and gets a host port mapping such as `localhost:3000` or the next available port if `3000` is already taken.
3. A developer wants a predictable preferred host port and runs `cladding expose 3000 9000`, allowing cladding to start at `9000` and increment if needed.
4. A developer runs `cladding expose list` to see which host ports are currently mapped for the current project.
5. A developer removes one mapping explicitly with `cladding expose stop 9000`.
6. A developer runs `cladding down` or `cladding destroy` and all expose proxies for that project are removed automatically.

## Functional Requirements
1. **Command surface**
   - Add a new top-level command group `expose`.
   - Support the following user-facing commands:
     - `cladding expose <containerport> [hostport]`
     - `cladding expose stop <hostport>`
     - `cladding expose list`
   - `start` is not required for the initial version.
2. **Target scope**
   - `cladding expose` only supports exposing ports from the current project’s `cli-pod-cli-app` container.
   - No container selector argument is supported.
   - Attempts to expose ports from any other container are out of scope.
3. **Project scoping**
   - Expose operations are scoped to the current `.cladding` project, using the same project identity model already used by `cladding` (`name` plus canonical `project_root`).
   - `list` and `stop` must only operate on expose proxies belonging to the current project.
4. **Create behavior**
   - `cladding expose <containerport> [hostport]` requires the current project to be running and the current project’s `cli-pod-cli-app` container to exist.
   - `<containerport>` is the target port inside `cli-pod-cli-app`.
   - `[hostport]` is optional.
   - If `[hostport]` is omitted, cladding starts searching from `<containerport>` on the host.
   - If `[hostport]` is provided, cladding starts searching from that host port.
   - If the starting host port is already in use, cladding increments until it finds an available host port.
   - On success, cladding prints the resolved host port and resulting localhost URL or socket mapping in a human-readable form.
5. **Duplicate mapping rule**
   - A project may have at most one active expose mapping per `containerport`.
   - If an expose proxy for the requested `containerport` already exists for the current project, `cladding expose <containerport> [hostport]` must fail with a clear error.
   - Cladding must not silently reuse, replace, or duplicate the mapping.
6. **Stop behavior**
   - `cladding expose stop <hostport>` removes the expose proxy for the current project whose published host port matches `<hostport>`.
   - If no such expose proxy exists for the current project, the command must fail with a clear error.
   - The command should not affect expose proxies owned by other cladding projects.
7. **List behavior**
   - `cladding expose list` shows current expose proxies for the current project.
   - Output must include at least:
     - host port
     - target container port
     - status
   - The exact display format may remain simple text or table-style, consistent with current CLI conventions.
   - If no expose proxies exist for the current project, the command should print a clear empty-state message.
8. **Lifecycle cleanup**
   - `cladding down` must stop and remove all expose proxies for the current project as part of project shutdown.
   - `cladding destroy` must force-remove all expose proxies for the current project as part of teardown.
   - Cleanup should be tolerant of “already gone” resources and should not fail merely because no expose proxies exist.
9. **Runtime identification**
   - Expose proxy containers must carry enough labels/metadata to support reliable discovery by current project, host port, and container port.
   - Name prefixes alone are insufficient as the primary discovery mechanism.
10. **Error handling**
   - Failures should clearly distinguish:
     - project not running / target CLI container missing
     - requested container port already exposed for this project
     - requested host port not found for stop
     - inability to allocate a free host port
     - Podman container create/start/inspect failures

## Non-Goals
1. Exposing ports from `sandbox-app`, `proxy`, or any other container besides `cli-pod-cli-app`.
2. Adding permanent port-publishing configuration to `cladding.json` or `pods.yaml`.
3. Supporting UDP, Unix sockets, or protocols beyond simple TCP port forwarding.
4. Supporting multiple host-port mappings for the same container port within one project.
5. Changing `cladding ps` output.
6. Supporting cross-project management from one working directory; `expose list` and `expose stop` remain current-project scoped.
7. Requiring users to manage raw proxy-container names directly.
8. Preserving the temporary `./expose.sh` interface as a compatibility contract.

## Design Considerations
1. **Minimal CLI friction**
   - The main action should be a short command: `cladding expose <containerport> [hostport]`.
   - The default should feel intuitive: try the same host port first, then increment as needed.
2. **Project-managed lifecycle**
   - Expose proxies are not independent infrastructure; they are runtime attachments owned by the project.
   - They should disappear with `cladding down` and `cladding destroy` without requiring extra user cleanup.
3. **Project-safe targeting**
   - `stop` by host port is user-friendly, but it must remain project-scoped to avoid collisions with other running cladding projects.
4. **Operational clarity**
   - Users need to understand where to connect from the host and what is currently published.
   - `list` output should make the mapping obvious without exposing low-value implementation details.
5. **Narrow scope**
   - Restricting the feature to `cli-pod-cli-app` keeps the security and UX model simple for the first version.

## Technical Considerations
1. **CLI parsing**
   - `cladding/src/cli.rs` currently has only top-level commands.
   - Implementation will need a new `Expose` command shape with:
     - default action for create (`<containerport> [hostport]`)
     - subcommands for `stop` and `list`
   - The parsing design should preserve straightforward help/usage output.
2. **Discovering the target container**
   - The target container name can be derived from the current project’s active network settings:
     - `<name>-cli-pod-cli-app`
   - Creation should reuse existing “project is running” and active project resolution logic where possible.
3. **Expose proxy discovery model**
   - Add helper(s) in `cladding/src/podman.rs` to list expose proxy containers by labels rather than by name prefix.
   - Recommended labels include:
     - `cladding=<project name>`
     - `project_root=<canonical .cladding path>`
     - `cladding_expose=true`
     - `cladding_expose_target=cli-app`
     - `cladding_expose_container_port=<containerport>`
     - `cladding_expose_host_port=<hostport>`
   - This supports:
     - current-project `list`
     - `stop <hostport>`
     - duplicate detection by container port
     - cleanup in `down` and `destroy`
4. **Proxy-container implementation**
   - The temporary `./expose.sh` demonstrates a viable pattern:
     - run a detached helper container
     - use `socat`
     - publish a host port with `-p`
     - forward host traffic to the CLI container
   - The script’s useful ideas are:
     - dynamic host-port allocation by incrementing from a base port
     - detached standalone helper container
     - runtime-readable mapping metadata
   - The script should not be copied directly because it lacks project labels, lifecycle integration, and a stable discovery model.
5. **Networking approach**
   - Preferred implementation direction:
     - start a helper container that shares the target container namespace via `--network container:<cli-container>`, if Podman allows that together with host port publishing in the supported environments.
   - If that mode does not support `-p` in practice, a fallback is:
     - run the helper container on the same `cladding-N` network
     - discover the active CLI container IP
     - proxy to that IP:port
   - This should be validated during implementation; the PRD does not require exposing ports from arbitrary project networks or containers.
6. **Image/build strategy**
   - Current product flow does not manage an auxiliary proxy image.
   - The feature should use `alpine/socat` for the expose helper container, matching the temporary script direction.
   - No custom cladding-managed helper image is required for the initial version.
   - Implementation should handle the fact that this introduces a runtime image dependency outside the existing `cladding build` flow.
7. **Cleanup integration**
   - `cmd_down` should remove current-project expose proxies in addition to `podman play kube --down`.
   - `cmd_destroy` should force-remove current-project expose proxies in addition to the three project pods it already removes.
   - Cleanup helpers should be project-scoped and robust when no expose proxies exist.
8. **Port allocation**
   - The temporary script uses `/dev/tcp` probing to find a free host port.
   - Rust implementation should use a reliable host-port availability check that works in the supported environments and avoids races as much as practical before container start.
   - Final failure handling still needs to cope with Podman reporting a bind conflict during container creation if the port becomes unavailable after probing.
9. **Testing**
   - Add unit tests for argument parsing and any label/metadata parsing helpers.
   - Add integration coverage for:
     - creating a mapping with omitted host port
     - creating a mapping with explicit host port
     - duplicate container-port failure
     - stop by host port
     - list output with one or more mappings
     - cleanup on `down`
     - cleanup on `destroy`
   - Tests that depend on Podman networking details may need to be integration-only.
10. **Documentation**
   - Update README command list and usage examples.
   - Document that the feature is temporary/runtime-only and scoped to the current project’s `cli-pod-cli-app`.

## Success Metrics
1. A running project can expose a TCP port from `cli-pod-cli-app` to the host with a single command.
2. When no host port is specified, cladding successfully picks the first available host port starting at the requested container port.
3. `cladding expose stop <hostport>` reliably removes the matching current-project mapping.
4. `cladding expose list` accurately reports active mappings for the current project.
5. `cladding down` and `cladding destroy` remove all current-project expose proxies automatically.
6. Attempts to create a second mapping for the same container port fail with a clear error.
7. The implementation does not require users to manually inspect or manage proxy container names.

## Open Questions
1. Confirm in the target Podman environments whether `podman run -p ... --network container:<base_container>` is valid and reliable; if not, use the same-network-plus-target-IP fallback.
2. Decide the exact helper-image provisioning path:
   - Resolved: use `alpine/socat`.
3. Decide whether `cladding expose list` should show only running proxies or also surface stopped/exited expose containers if any remain after failed cleanup attempts. The recommended initial behavior is to show only active/running mappings.
