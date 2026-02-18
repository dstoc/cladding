# Questions: Network MCP Rust Reimplementation

## Q1 (pending)
- Question: Should the Rust server preserve full policy compatibility with the current TS `network-policy.json` schema (`exact`, `regex`, `hash`, `position`, `required`, and per-command `env` allowlist), or can the policy model be simplified?
- Why it matters: This directly determines PRD scope, migration complexity, and whether existing policy files can be reused without changes.
- Answer: Keep the same configuration and features, while dropping the proxy domain list.
- Decision/Impact: Rust implementation will preserve command policy schema/behavior parity with TS for command rules and env allowlist, but will remove `allowedHosts`/proxy-domain filtering from scope and config.

## Q2 (pending)
- Question: For MCP transport, should the Rust server support only stdio (to keep `supergateway --stdio` compatibility), or should it also support Unix socket mode like the TS server?
- Why it matters: This changes runtime architecture, CLI integration, and implementation complexity.
- Answer: It could expose HTTP/SSE directly (using `rmcp` crate).
- Decision/Impact: Rust implementation target will be direct MCP-over-HTTP/SSE server; `supergateway` stdio wrapper and Unix socket transport parity are out of primary scope unless needed for migration fallback.

## Q3 (pending)
- Question: Should the new Rust policy parser still accept `allowedHosts` in `network-policy.json` for backward compatibility (but ignore it), or should `allowedHosts` be treated as invalid and fail validation?
- Why it matters: This determines migration friction for existing policy files and strictness of configuration validation.
- Answer: invalid/fail
- Decision/Impact: Rust policy schema will reject `allowedHosts` and fail fast on startup/config load when present.

## Q4 (pending)
- Question: Your steps specify both `./docs/features/FEATURE` (step 1) and `./docs/FEATURE/prd.md` (step 4). Which path should contain the final PRD?
- Why it matters: It determines where the canonical product proposal artifact is stored.
- Answer: put it in ./docs/features/FEATURE/prd.md
- Decision/Impact: Final proposal file path is `docs/features/network-mcp-rust-reimplementation/prd.md`.

## Q5 (pending)
- Question: Should rollout be a hard cutover (replace TS server immediately) or a phased migration that keeps TS server available behind a feature flag during validation?
- Why it matters: This changes migration requirements, operational complexity, and rollback strategy.
- Answer: no migration now, implement the rust version entirely in the new directory, then we will update the sandbox/pods etc. later, that part is out of scope for this feature
- Decision/Impact: PRD scope is implementation-only inside `mcp-run`; container/pods/sandbox wiring changes are explicitly deferred and out of scope.

## Q6 (pending)
- Question: Should the Rust MCP API keep exact compatibility for `run_network_tool` (same tool name and input/output fields), or are breaking API changes acceptable?
- Why it matters: This determines integration risk and whether downstream clients need coordinated updates.
- Answer: changing that API is OK
- Decision/Impact: PRD may define a Rust-optimized API contract instead of strict wire-compatibility with the TS tool response format.

## Q7 (pending)
- Question: Should the Rust implementation enforce a default per-command timeout (for example 30 seconds), or leave timeout control entirely to the called process?
- Why it matters: This determines baseline safety/resource control behavior and affects long-running command workflows.
- Answer: Match behavior of current implementation. If missing and needed later, consider then. In general keep parity with current functionality.
- Decision/Impact: Rust implementation will not introduce a new default command timeout if TS implementation does not enforce one; parity-first behavior.
