# PRD: `mcp-run` Policy Check Interface

## Objective
Add a policy-only check interface to `mcp-run` so callers can ask whether a command request would be allowed without executing it.

The check must use the same request shape and the same pre-spawn authorization path as command execution. This avoids side effects while preserving accuracy for policies that depend on executable path, executable hash, args, or forwarded env.

This proposal assumes the neutral terminology rename has landed first, so names below use `run_command` and `RunCommandInput`.

## Use Cases
1. An agent wants to decide whether a delegated sandbox command is available before presenting or attempting an action.
2. A user wants to debug Rego policy decisions without executing the command.
3. A wrapper script wants a fast preflight check that distinguishes "denied by policy" from execution/runtime failure.
4. Tests want to verify policy behavior without relying on a command's side effects or output.

## Functional Requirements

### 1. Scope and Placement
1. Implementation is inside `crates/mcp-run`.
2. Add a policy-check interface for both exposed protocols:
- MCP tool: `check_command`
- HTTP endpoint: `POST /check`
3. Add an explicit `run-remote --check` client mode that calls `/check` instead of `/raw`.
4. `run-remote` does not call `/check` by default.
5. Existing `run_command`, `/raw`, and default `run-remote -- ...` execution behavior must remain unchanged.

### 2. Request Contract
1. `check_command` and `POST /check` accept the same request schema as command execution:
- `executable: string` (required)
- `args: string[]` (optional, default empty)
- `cwd: string` (optional)
- `env: { [key: string]: string }` (optional)
2. `cwd` is accepted for request-shape parity, but the policy check does not start a process.
3. The check must resolve the executable and compute its SHA-256 hash exactly as execution does today, because Rego receives:
- `input.command`
- `input.path`
- `input.hash`
- `input.args`
- `input.env`
4. The check must evaluate the active Rego policy state exactly as execution would before process spawn.

### 3. Response Contract
1. Successful request parsing returns a JSON policy decision object rather than starting a stream or subprocess.
2. Response body shape:
- `allowed: boolean`
- `reason: string | null`
- `executable: string`
- `resolvedPath: string | null`
- `hash: string | null`
- `policyMode: string`
3. `allowed: true` means execution would pass policy validation at the time of the check, assuming the policy and executable file do not change before execution.
4. `allowed: false` means execution would be rejected before process spawn.
5. `reason` should contain the same user-actionable validation message execution would return, for example:
- policy deny-all is active;
- command not allowed;
- policy evaluation failed;
- executable path resolution failed;
- executable hash resolution failed.
6. `resolvedPath` and `hash` are populated only when those steps succeed.
7. `policyMode` reports the current engine mode, for example `rego` or `deny-all`.

### 4. HTTP Semantics
1. `POST /check` returns `200 OK` for well-formed requests, including denied decisions.
2. Malformed JSON or schema-invalid request bodies return `400 Bad Request` with JSON error body.
3. Internal server failures unrelated to normal validation return `500 Internal Server Error`.
4. Policy denial, deny-all mode, path resolution failure, and hash resolution failure are normal check outcomes and should be represented as `allowed: false` with `200 OK`.

### 5. MCP Semantics
1. Add MCP tool `check_command`.
2. The tool accepts the same input schema as `run_command`.
3. The tool returns the same decision object as `POST /check`.
4. Policy denials should be returned as structured successful tool output with `allowed: false`, not as MCP tool errors.
5. Malformed MCP arguments continue to use normal MCP argument decoding errors.

### 6. Shared Authorization Primitive
1. Extract the current pre-spawn validation logic from command spawning into a reusable function.
2. Suggested internal shape:
- `authorize_command_request(policy_engine, input) -> CommandAuthorization`
- `spawn_command_process(...)` calls `authorize_command_request(...)` and only spawns on allowed authorization.
- `check_command(...)` calls `authorize_command_request(...)` and renders the decision without spawning.
3. The shared primitive must perform, in order:
- user env defaulting;
- executable resolution;
- executable SHA-256 hashing;
- policy evaluation.
4. The shared primitive should preserve current execution errors for `spawn_command_process` so existing execution callers keep the same error behavior.

### 7. `run-remote --check`
1. Add `--check` as a pre-delimiter option to `run-remote`.
2. Usage:
- `run-remote --check -- <executable> [args...]`
- `run-remote --check --keep-env=NAME,OTHER -- <executable> [args...]`
3. `--check` builds the same request payload as execution, including `cwd` and forwarded env.
4. `--check` sends the payload to the corresponding `/check` endpoint instead of `/raw`.
5. The server URL should continue to come from the same configured server endpoint used by `run-remote`. If that endpoint includes `/raw`, the client should derive the check endpoint by replacing the final path segment with `/check`.
6. `--check` prints the JSON decision object returned by the server to stdout.
7. Exit-code semantics:
- `0` when the check request succeeds and `allowed: true`;
- `1` when the check request succeeds and `allowed: false`;
- existing local failure exit code for parse errors, missing env, connection failures, malformed server responses, or HTTP errors.
8. `--check` must not execute the command or consume the `/raw` streaming response format.
9. `--check` is a convenience/debugging feature only; it is not required for enforcement.

### 8. Race and Freshness Semantics
1. A check is advisory and point-in-time.
2. If policy files reload between check and execution, execution may produce a different result.
3. If the executable file changes between check and execution, hash-sensitive policies may produce a different result.
4. Documentation must state that callers requiring enforcement must still execute through `mcp-run`; `/check` is not a capability token or authorization grant.

### 9. Observability
1. Log check requests with command and decision.
2. Do not log forwarded env values.
3. Distinguish check decisions from execution denials in log messages.

### 10. Testing Requirements
1. Unit tests for allowed check result with populated `resolvedPath` and `hash`.
2. Unit tests for denied check result when policy returns false.
3. Unit tests for deny-all mode returning `allowed: false`.
4. Unit tests for missing executable returning `allowed: false`.
5. HTTP tests:
- `POST /check` allowed request returns `200` and `allowed: true`;
- `POST /check` denied request returns `200` and `allowed: false`;
- malformed JSON returns `400`.
6. MCP tests:
- tool list includes `check_command`;
- `check_command` returns structured allowed/denied decisions;
- `run_command` behavior remains unchanged.
7. Regression test that denied checks do not spawn a process.
8. `run-remote --check` tests:
- parses alongside `--keep-env` before the delimiter;
- derives `/check` from a configured `/raw` server endpoint;
- prints the decision JSON;
- exits `0` for allowed and `1` for denied;
- does not use the raw streaming parser.

## Non-Goals
1. No authorization tokens or reusable grants from `/check`.
2. No dry-run mode that simulates process output.
3. No partial policy evaluator that skips path resolution or hashing.
4. No change to Rego policy query or input fields.
5. No change to `run-remote` default execution behavior.
6. No caching of policy decisions.

## Suggested Implementation Shape
1. Complete the neutral terminology rename first so the new API does not introduce more network-specific names.
2. In `crates/mcp-run/src/executor.rs`, introduce:
- `RunCommandCheckOutput` or `CommandCheckOutput`;
- an internal authorization result struct containing user env, resolved path, hash, and decision;
- a shared authorization function used by both check and spawn.
3. In `crates/mcp-run/src/raw.rs`, add a `check_handler` or a small new module if the file grows too large.
4. In `crates/mcp-run/src/mcp.rs`, add `check_command` to the same server type as `run_command`.
5. In `crates/mcp-run/src/remote.rs`, add `--check` parsing and a non-streaming request path for the check endpoint.
6. In `crates/mcp-run/src/lib.rs`, export the new check output type only if external Rust callers need it; otherwise keep it crate-local.
7. Update `crates/mcp-run/README.md` with `/check`, `check_command`, `run-remote --check`, and freshness semantics.

## Verification
1. `cargo fmt --check` succeeds.
2. `cargo test -p mcp-run` succeeds.
3. `cargo test --workspace` succeeds.
4. Manual HTTP check:

```bash
curl -sS -X POST http://127.0.0.1:8000/check \
  -H 'content-type: application/json' \
  --data '{"executable":"echo","args":["hello"]}'
```

5. Manual MCP tool listing shows both `run_command` and `check_command`.
6. A denied `/check` request returns `200 OK` with `allowed: false` and does not execute the command.
7. Manual client check:

```bash
run-remote --check -- echo hello
```

## Success Criteria
1. Callers can ask whether a command would pass policy without executing it.
2. Check and execution share one authorization path.
3. Check results account for executable resolution and hashing.
4. Denied checks are represented as data, not protocol failures.
5. Existing execution interfaces remain behaviorally unchanged.
6. `run-remote --check` provides a convenient CLI path for policy checks without changing default execution.
