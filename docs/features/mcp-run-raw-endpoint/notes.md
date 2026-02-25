# Notes: mcp-run `/raw` Endpoint + `run-remote` Client

## Request Summary
- Add a second endpoint on the same `mcp-run` HTTP server at `/raw`.
- `/raw` should accept command-run requests similar to the MCP tool request (`executable/args/env/cwd` shape).
- `/raw` should stream stdout/stderr in chunks so a simple client can replay output in real time.
- Add a new CLI binary `run-remote` that sends remote run requests and replays streamed output locally.
- Example interface (corrected): `RUN_REMOTE_SERVER=http://host:port/raw TEST=1 THING=2 run-remote --keep-env=TEST,THING -- some-command --command --args`.

## Existing Product Findings

### `mcp-run` server shape (`crates/mcp-run/src/mcp.rs`)
- Existing server is Axum + `rmcp` streamable HTTP service.
- Current endpoint is only `/mcp`:
  - `Router::new().route_service("/mcp", any_service(mcp_service))`
- Config:
  - `MCP_BIND_ADDR` (default `127.0.0.1:8000`)
  - `POLICY_FILE` (required)
- Tool exposed via MCP: `run_network_tool`.

### Command execution behavior (`crates/mcp-run/src/executor.rs`)
- Request model: `RunNetworkToolInput`
  - `executable: String`
  - `args: Vec<String>`
  - `cwd: Option<String>`
  - `env: Option<BTreeMap<String, String>>`
- Output model: `RunNetworkToolOutput`
  - `stdout: String`
  - `stderr: String`
  - `exitCode: Option<i32>`
- Execution uses `tokio::process::Command` with no shell wrapping.
- Stdout/stderr are captured separately and truncated at `MAX_OUTPUT_BYTES` (1 MiB each) with marker `\n...truncated...`.
- Environment is rebuilt via `build_command_env` and currently:
  - keeps selected baseline env (`HOME`, `LANG`, `PATH` + proxy vars from host)
  - clears command env and injects sanitized/controlled values.

### Policy behavior (`crates/mcp-run/src/policy.rs`)
- Policy is list of command rules.
- Validation supports arg checks: `exact`, `regex`, `hash` with optional `position` and `required`.
- Per-rule env allowlist enforced.
- Legacy `allowedHosts` is explicitly rejected.

### Existing docs / prior PRDs
- Existing feature docs follow `docs/features/<feature>/` pattern with:
  - `notes.md`
  - `questions.md`
  - `prd.md`
- Prior PRD (`docs/features/network-mcp-rust-reimplementation/prd.md`) emphasizes parity-first behavior, no-shell execution, and policy enforcement.

## Initial Implications for This Feature
- `/raw` should likely reuse `RunNetworkToolInput` validation/execution paths to avoid policy drift.
- Streaming behavior is currently absent: executor returns final aggregated buffers only.
- `run-remote` likely needs protocol framing for at least:
  - start/ack
  - stdout chunk
  - stderr chunk
  - terminal event with exit code / error
- The env-forwarding flag (`--keep-env`) governs which local env vars are forwarded to remote.

## Clarified Decisions From Q&A
- Streaming protocol: use HTTP chunked `application/x-ndjson` for `/raw` responses.
- Request protocol: `/raw` accepts plain JSON matching `RunNetworkToolInput`; no JSON-RPC envelope in v1.
- Output ordering contract: preserve ordering within each stream independently (`stdout`, `stderr`), without global cross-stream ordering guarantees.
- CLI flag naming: use `--keep-env` (the earlier `--keep-end` mention was a typo).
- `RUN_REMOTE_SERVER` format: full URL required in v1 (no host:port shorthand).
- Pre-execution failures: return HTTP error status with JSON body (no NDJSON stream).
- Client exit behavior: `run-remote` mirrors the remote process exit code on normal completion.
- Request field naming: `/raw` uses `executable` (same as existing MCP tool input), not `command`.
- Security boundary: `/raw` uses the same trust model as `/mcp`; no new endpoint auth is added in this feature.
- Env forwarding default: `run-remote` forwards no local env vars unless explicitly listed via `--keep-env`.
- Stream payload encoding: `/raw` emits base64-encoded bytes for stdout/stderr chunks (binary-safe fidelity).
- Output size behavior: `/raw` streams full output and does not enforce the existing 1 MiB per-stream truncation cap.
- `cwd` behavior: `run-remote` includes the caller's current working directory by default.
- Missing env handling: if `--keep-env` references unset variables, `run-remote` fails fast with a clear error.
- CLI delimiter: `run-remote` requires `--` to separate client options from remote executable/args.

## Ambiguities to Resolve
- None blocking for PRD draft.
