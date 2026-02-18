# PRD: Network MCP Rust Reimplementation (Command Runner Only)

## Objective
Reimplement the existing Network MCP server from `clawmini/mcp-servers/network` in Rust within `mcp-run`, preserving current command-execution and policy-validation functionality while intentionally removing proxy setup/domain-filtering features.

This feature delivers a Rust-native MCP server that:
- exposes MCP over HTTP/SSE (via `rmcp`),
- runs allowlisted network-capable commands without shell interpretation,
- enforces policy-file validation parity with the current TS command policy model,
- relies on container-provided proxy environment variables instead of internal proxy orchestration or env hardening layers.

## Use Cases
1. A client invokes a network-capable command (for example, `npm`, `git`, `curl`) through MCP, and execution is allowed only when arguments match policy rules.
2. A client runs a script path that must match a required SHA-256 hash in policy.
3. A client passes command-specific environment variables (tokens/config), and only policy-allowed keys are accepted.
4. Operators run the MCP server in a hardened container where `http_proxy` / `https_proxy` / `no_proxy` are preconfigured externally.
5. A blocked command invocation returns clear validation failure details so the caller can self-correct.

## Functional Requirements

### 1. Project Location and Scope
1. Implementation lives entirely in `mcp-run`.
2. This feature does not modify pod specs, sandbox startup scripts, containerfiles, or extension wiring.
3. Existing TS server remains untouched during this feature.

### 2. MCP Transport and Server
1. Rust server uses `rmcp` and exposes MCP over HTTP/SSE directly.
2. Server exposes a command-execution tool equivalent in capability to current TS `run_network_tool`.
3. Tool API may be Rust-optimized (breaking changes are acceptable), but must preserve the same functional coverage.

### 3. Command Execution Behavior
1. Tool accepts command executable, args, optional cwd, and optional env map (or equivalent Rust API fields).
2. Command execution uses direct process spawning with no shell (`sh`, `bash`, `cmd /c`, etc. are never used as wrappers).
3. If `cwd` is provided, command executes in that directory; otherwise uses server default working directory.
4. Return payload includes at minimum:
   - `stdout` (captured text),
   - `stderr` (captured text),
   - `exitCode` (nullable/optional if process failed before exit).
5. Output truncation parity:
   - cap stdout at 1MB and append a truncation marker when exceeded,
   - cap stderr at 1MB and append a truncation marker when exceeded.
6. Timeout behavior parity:
   - do not introduce a new default server-side timeout in this feature.

### 4. Policy Configuration and Validation
1. Policy is loaded from `POLICY_FILE` path (or documented default path if unset).
2. Policy schema supports command rule features equivalent to TS:
   - `command` string matching,
   - arg checks with `exact`, `regex`, and `hash` types,
   - optional `position`,
   - optional `required`,
   - optional per-command env allowlist.
3. Validation semantics mirror current TS behavior:
   - command is allowed if at least one rule for that command fully validates,
   - all provided args must match allowed checks for a candidate rule,
   - all required checks must be satisfied,
   - env input keys must be subset of rule `env` allowlist.
4. `hash` check uses SHA-256 against file contents of the provided argument path.
5. Legacy proxy domain policy is removed:
   - `allowedHosts` is invalid in Rust policy schema,
   - presence of `allowedHosts` causes config validation failure.
6. Invalid or unreadable policy must fail startup/config load with actionable errors.

### 5. Environment Handling
1. Server does not implement proxy injection logic (`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` rewriting).
2. Server does not implement the TS sensitive-env blacklist scrub behavior.
3. Runtime proxy behavior relies on container environment already being configured.
4. Request-provided env variables are still constrained by policy env allowlist before process launch.

### 6. Error Handling and Observability
1. Tool returns explicit, user-actionable errors for:
   - disallowed commands,
   - arg mismatch,
   - missing required args,
   - disallowed env keys,
   - hash mismatch,
   - process spawn failures.
2. Validation errors should preserve enough detail to help callers choose valid command forms.
3. Server logs startup/config validation failures and request execution outcomes at an operationally useful level.

### 7. Testing Requirements
1. Add Rust tests for policy parsing/validation parity:
   - exact/regex/hash checks,
   - position and required semantics,
   - env allowlist checks,
   - rule OR behavior (multiple rules per command).
2. Add tests for process execution behavior:
   - successful allowed command execution,
   - blocked disallowed command,
   - output truncation at limits.
3. Add config schema test that `allowedHosts` causes validation failure.
4. Add HTTP/SSE smoke/integration test for MCP tool invocation (where practical in CI).

## Non-Goals
1. Rebuilding or embedding a proxy server in Rust.
2. Domain/path egress filtering in this server.
3. Updating `pods.yaml`, jailer scripts, `Containerfile.sandbox`, or startup gateway scripts.
4. Migrating consumers from TS server in this feature.
5. Introducing semantic network tools beyond generic command runner.
6. Adding new timeout/resource-governance behavior beyond current TS parity.

## Design Considerations
1. Parity-first migration: preserve behavior users rely on, change implementation language/runtime.
2. Hardened container assumption: proxy routing and network constraints are external responsibilities.
3. Strict schema evolution: rejecting `allowedHosts` prevents accidental belief that host filtering still exists.
4. Keep validation explainable: errors must support LLM/client self-correction workflows.

## Technical Considerations
1. Proposed Rust stack:
   - `rmcp` for MCP server over HTTP/SSE,
   - async runtime (`tokio`),
   - `serde`/`serde_json` for config and tool payloads,
   - `regex` for arg pattern checks,
   - `sha2` for hash validation,
   - `thiserror` or equivalent for structured errors,
   - `tracing` for logs.
2. Schema strictness:
   - use strict deserialization (`deny_unknown_fields`) on policy types where needed to reject `allowedHosts`.
3. Process execution:
   - use `tokio::process::Command`,
   - stream stdout/stderr asynchronously with bounded accumulation,
   - preserve non-shell invocation guarantees.
4. Env merge strategy:
   - inherit process environment as base (including externally supplied proxy vars),
   - apply validated request env values according to policy.
5. Compatibility boundary:
   - behavioral compatibility is required,
   - wire-level response shape may change if documented and test-covered.
6. Security model boundary:
   - command safeness is enforced by policy validation + no-shell process spawning,
   - network egress enforcement is out-of-process and out-of-scope for this feature.

## Success Metrics
1. Feature completeness:
   - all parity-required command policy features implemented in Rust.
2. Safety correctness:
   - no-shell execution guarantee verified in code and tests.
3. Validation correctness:
   - policy test suite passes for all legacy rule types and semantics.
4. Config contract correctness:
   - `allowedHosts` reliably rejected with clear error.
5. Runtime behavior:
   - command output truncation and exit reporting match expected parity behavior.
6. Readiness:
   - Rust server can be started independently in `mcp-run` and invoked via HTTP/SSE MCP client.

## Open Questions
1. Final tool schema naming and exact response envelope for Rust API (breaking changes allowed, but should be finalized before implementation begins).
2. Desired default bind address/port conventions for direct HTTP/SSE deployment in target container.
3. Required audit logging format (if any) beyond operational logs for this phase.
