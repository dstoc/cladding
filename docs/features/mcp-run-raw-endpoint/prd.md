# PRD: `mcp-run` `/raw` Streaming Endpoint + `run-remote` Client

## Objective
Add a second execution interface to `mcp-run` at `POST /raw` that accepts the same invocation shape as the existing MCP tool and streams process output as chunked NDJSON events. Add a companion CLI binary, `run-remote`, that sends those requests and replays remote `stdout`/`stderr` locally with correct stream routing.

This enables simple, scriptable remote command execution without MCP protocol/client overhead while preserving command policy enforcement.

## Use Cases
1. A user wants to run `run-remote some-command ...` and watch output in real time from a remote sandbox server.
2. A script or CI job needs remote command execution with local-process-compatible exit codes.
3. A command emits binary or non-UTF8 output and must be replayed without lossy text conversion.
4. A caller needs to forward only explicit environment variables (`--keep-env`) and avoid accidental secret leakage.
5. A client needs actionable errors when policy validation fails before execution.

## Functional Requirements

### 1. Scope and Placement
1. Feature implementation is inside `mcp-run`.
2. Existing `/mcp` endpoint and existing `run_network_tool` behavior remain available.
3. A new binary named `run-remote` is added in the same crate.

### 2. `/raw` Endpoint
1. Server exposes `POST /raw` on the same HTTP listener used by `/mcp`.
2. Request body is JSON with the same schema as current tool input:
- `executable: string` (required)
- `args: string[]` (optional, default empty)
- `cwd: string` (optional)
- `env: { [key: string]: string }` (optional)
3. `/raw` request parsing and validation reuses the same policy enforcement semantics as MCP execution:
- command allowlist checks
- arg rule checks (`exact` / `regex` / `hash`, `position`, `required`)
- env allowlist checks
4. Command execution must remain no-shell (direct process spawn only).

### 3. `/raw` Success Response (Streaming)
1. On accepted execution, server returns:
- HTTP `200 OK`
- `Content-Type: application/x-ndjson`
- chunked response body
2. Response emits one JSON object per line (NDJSON event stream).
3. Event types for v1:
- `start`: execution accepted; contains metadata (at minimum `event`, optionally `pid`/timestamps/request id).
- `stdout`: chunk from process stdout.
- `stderr`: chunk from process stderr.
- `exit`: terminal event including `exitCode` (nullable if unavailable).
4. `stdout`/`stderr` events carry payload as base64 bytes (binary-safe), for example `data_b64`.
5. Ordering contract:
- order is preserved within each stream (`stdout` sequence is ordered, `stderr` sequence is ordered)
- merged global ordering across both streams is not guaranteed.
6. `/raw` does not apply current 1 MiB truncation behavior; output streams in full.

### 4. `/raw` Error Responses
1. If request is rejected before process start (invalid request, policy denial), server returns standard non-200 HTTP response with JSON error body; no stream is created.
2. Response codes should be consistent and actionable:
- `400` for malformed/invalid request payload
- `403` for policy denial
- `500` for internal failures prior to stream start
3. If execution has started and a server-side runtime failure occurs during streaming, server emits a terminal error event (schema-defined) and closes the stream.

### 5. Execution Semantics Parity
1. `/raw` and MCP tool invocation must share core execution and policy logic.
2. `cwd` semantics remain: use request `cwd` when present, else server default cwd.
3. Env application remains policy-gated.
4. Security boundary remains unchanged from existing service model (no new auth layer added in this feature).

### 6. `run-remote` Binary
1. New binary name: `run-remote`.
2. Server target comes from env var `RUN_REMOTE_SERVER` and is required.
3. `RUN_REMOTE_SERVER` must be a full URL in v1 (example: `http://127.0.0.1:8000/raw`).
4. CLI command format:
- `run-remote [--keep-env=VAR1,VAR2,...] -- <executable> [args...]`
5. `--` delimiter is required to separate `run-remote` client options from remote command arguments.
6. `--keep-env` behavior:
- default forwards no environment variables
- forwards only listed variable names
- if any listed name is unset locally, client fails fast before request and reports missing names
7. Client includes caller current directory as request `cwd` by default.
8. Client request payload uses `executable` field name (not `command`).
9. Client reads NDJSON stream line-by-line, decodes base64 payloads, and writes bytes to local `stdout`/`stderr` accordingly.
10. On normal completion, client exits with the remote `exitCode`.
11. On client/protocol/network errors, client exits non-zero with a distinct local failure code and prints diagnostic details to `stderr`.

### 7. Observability and Logging
1. Server logs `/raw` request lifecycle: accepted/denied, command, duration, and exit status.
2. Log events should differentiate pre-execution rejection from runtime execution failures.
3. Client logs only user-relevant errors by default; streamed process output is passed through unchanged.

### 8. Testing Requirements
1. Server tests:
- `/raw` accepts valid request and streams `start`, output chunks, and `exit`
- policy denial returns non-200 JSON error (no stream)
- output beyond 1 MiB is fully streamed (no truncation)
- base64 payloads decode to exact emitted bytes
- per-stream order maintained under concurrent stdout/stderr output
2. Client tests:
- parses and replays `stdout`/`stderr` correctly
- propagates remote exit code
- fails fast on missing `RUN_REMOTE_SERVER`
- fails fast when `--keep-env` references missing local vars
- handles non-200 JSON error responses cleanly
3. Regression tests:
- existing `/mcp` tool behavior remains unchanged, including truncation behavior there.

## Non-Goals
1. Adding authentication/authorization specifically for `/raw`.
2. Replacing MCP; `/mcp` remains first-class.
3. JSON-RPC envelope for `/raw` request/response framing.
4. Global cross-stream ordering guarantees between stdout/stderr.
5. Retry/resume/session semantics for interrupted streams.
6. Container/pod wiring changes beyond `mcp-run` feature scope.

## Design Considerations
1. NDJSON was selected for simple streaming and low client complexity.
2. Base64 chunk payloads were selected to preserve exact bytes for binary output.
3. Explicit env forwarding via `--keep-env` minimizes accidental secret forwarding.
4. Reusing existing invocation schema (`executable`, `args`, `cwd`, `env`) avoids model drift.
5. Full URL requirement for `RUN_REMOTE_SERVER` keeps v1 parsing rules explicit.

## Technical Considerations
1. Add a dedicated Axum handler for `/raw` while reusing existing shared policy/execution modules.
2. Introduce a streaming execution path that emits stdout/stderr chunks incrementally instead of accumulating full buffers.
3. Ensure child process cleanup on client disconnect (cancel stream, terminate child process, avoid zombies).
4. Keep memory bounded by streaming chunk-by-chunk; do not buffer entire outputs in server memory for `/raw`.
5. Define a stable NDJSON event schema in Rust types (`serde`), including terminal events.
6. Client transport can use HTTP streaming with line-delimited JSON decode; implementation should tolerate partial line boundaries between TCP chunks.
7. Maintain separation of concerns:
- policy validation (shared)
- command spawning (shared primitives)
- response rendering (`/mcp` aggregate vs `/raw` streaming)
8. Prioritize low-latency streaming over batch efficiency: treat chunk size as a maximum read unit, and forward each available read immediately instead of waiting to fill a target chunk.

## Success Metrics
1. Feature correctness:
- `/raw` successfully executes allowlisted commands and streams output in real time.
2. Policy safety parity:
- disallowed commands/args/env are blocked consistently with existing policy behavior.
3. Replay fidelity:
- binary output round-trips correctly via base64 chunking and client decode.
4. Scriptability:
- `run-remote` exit codes match remote process exit codes on normal completion.
5. Stability:
- long-running/high-output commands complete without server-side truncation or unbounded memory growth.
6. Regression safety:
- `/mcp` existing behavior remains unchanged.

## Open Questions
1. Should `/raw` emit an explicit request/run identifier in all events for cross-system correlation in logs?
2. Should `run-remote` add optional retry/backoff behavior for initial connection failures in a later phase?
3. Should we introduce optional output rate limits or hard output caps for operational safety in later phases?
4. Should future iterations add a `--cwd` override flag (and potentially `--no-cwd`) for client control beyond default current directory forwarding?
