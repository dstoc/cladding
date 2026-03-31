# Questions: Cladding Expose Port Proxy

## Q1 (resolved)
- Question: When the user runs `cladding expose start <port>`, should cladding try to bind that same host port exactly, or should it treat `<port>` as the internal target port and auto-pick an available host port when needed?
- Why it matters: This changes the command contract, the `list`/`stop` UX, error handling, and whether users can rely on deterministic `localhost:<port>` behavior.
- Answer: Treat `<port>` as the container port and auto-pick a free host port when needed. Support `cladding expose <containerport> [<hostport>]`. `start` can likely be omitted.
- Decision/Impact: The PRD should define the primary create command as `cladding expose <containerport> [<hostport>]`, with `<containerport>` always targeting `cli-pod-cli-app`. Host-port selection should default to “first available at or above the requested/default host port” rather than strict same-port binding.

## Q2 (resolved)
- Question: For stopping an expose proxy, should `cladding expose stop` identify the proxy by host port, by generated proxy name/id, or should the CLI avoid `stop` entirely and instead support only `cladding expose list` plus `cladding down` cleanup?
- Why it matters: This determines how users target a specific proxy, what `list` must display, and whether the exposed-proxy naming scheme is user-facing or internal-only.
- Answer: The host port is the most idiomatic identifier.
- Decision/Impact: `cladding expose stop <hostport>` should be the primary removal command. `cladding expose list` must display host-port mappings clearly, and generated proxy container names can remain internal implementation details.

## Q3 (resolved)
- Question: If a project already has an expose proxy for container port `<containerport>`, should a second `cladding expose <containerport> [hostport]` create an additional mapping, be treated as idempotent if the same mapping already exists, or fail unless the existing mapping is removed first?
- Why it matters: This determines whether the feature behaves like a many-to-one port publisher, how duplicate requests are handled, and how much state validation/listing logic is needed.
- Answer: Fail with an error.
- Decision/Impact: The PRD should require at most one active expose mapping per container port for a given project. `cladding expose` must detect an existing mapping for the requested container port and return a clear error instead of creating duplicates or silently reusing it.

## Q4 (resolved)
- Question: When `[hostport]` is omitted in `cladding expose <containerport> [hostport]`, should cladding start searching from the same numeric port as `<containerport>`, or from a fixed default range/base?
- Why it matters: This affects predictability of the default localhost URL and determines the port-allocation algorithm.
- Answer: Start searching from the same numeric port as `<containerport>`.
- Decision/Impact: Default host-port allocation should begin at `<containerport>` and increment until a free host port is found. This makes the default behavior predictable while still avoiding hard failures when the same port is already occupied.

## Q5 (resolved)
- Question: Should `cladding destroy` also remove any expose proxy containers for the current project, or should expose proxies be cleaned up only by `cladding down` and explicit `cladding expose stop`?
- Why it matters: This determines whether expose proxies are part of the project’s full teardown lifecycle and affects implementation in `destroy`.
- Answer: `cladding destroy` should remove them too.
- Decision/Impact: Expose proxy containers are part of project-managed runtime state and must be cleaned up by both `cladding down` and `cladding destroy`, in addition to explicit `cladding expose stop <hostport>`.

## Q6 (resolved)
- Question: Should the expose helper use a cladding-managed helper image, or should it use `alpine/socat` directly?
- Why it matters: This determines image/build workflow, runtime dependencies, and whether `cladding build` needs to provision a separate helper image.
- Answer: Use `alpine/socat`, not a cladding-managed helper image.
- Decision/Impact: The PRD should specify `alpine/socat` as the helper container image. No custom helper image build path is required for this feature.
