# Questions: mcp-run `/raw` Endpoint + `run-remote`

## Q1 (resolved)
- Question: For `/raw`, which streaming protocol should we standardize on for v1: HTTP `text/event-stream` (SSE), newline-delimited JSON over chunked HTTP (`application/x-ndjson`), or another format?
- Why it matters: This determines server endpoint shape, client parser complexity, reconnection semantics, and debuggability for simple clients.
- Answer: `application/x-ndjson` over chunked HTTP.
- Decision/Impact: `/raw` will stream newline-delimited JSON event objects and `run-remote` will parse line-by-line.

## Q2 (resolved)
- Question: For `/raw` request bodies, should v1 use a plain JSON object matching `RunNetworkToolInput` (`executable`, `args`, `cwd`, `env`), or a JSON-RPC-style envelope (`jsonrpc`, `method`, `params`, `id`)?
- Why it matters: This defines protocol complexity, server/client implementation effort, and forward-compatibility strategy for additional operations.
- Answer: Plain JSON matching current tool input shape.
- Decision/Impact: `/raw` v1 will accept direct `RunNetworkToolInput`-compatible JSON with no RPC envelope.

## Q3 (resolved)
- Question: Should `/raw` preserve global output ordering between stdout and stderr chunks (single merged event timeline), or is preserving order within each stream independently sufficient?
- Why it matters: Global ordering requires timestamp/sequence coordination across both pipes and adds complexity; per-stream ordering is simpler but may interleave differently than local execution display.
- Answer: Preserve per-stream order only.
- Decision/Impact: Server/client requirements only guarantee ordered chunks within each stream (`stdout` and `stderr` independently), with no strict merged-order guarantee across streams.

## Q4 (resolved)
- Question: Should the new client flag remain exactly `--keep-end` as shown, or should we rename to `--keep-env` (while optionally supporting `--keep-end` as an alias)?
- Why it matters: This is user-facing CLI contract; changing after release creates compatibility churn.
- Answer: Use only `--keep-env`; `--keep-end` was a typo.
- Decision/Impact: The client interface will standardize on `--keep-env` and not expose `--keep-end`.

## Q5 (resolved)
- Question: What exact format should `RUN_REMOTE_SERVER` accept in v1?
- Why it matters: This defines URL parsing rules and defaults for `run-remote`.
- Options:
  1. Host:port only, with client auto-using `http://` and appending `/raw`.
  2. Full URL required (e.g., `http://host:port/raw`).
  3. Both accepted (host:port shorthand and full URL) with clear precedence.
- Answer: Full URL required.
- Decision/Impact: `run-remote` will require `RUN_REMOTE_SERVER` to be a full URL; no host:port shorthand expansion in v1.

## Q6 (resolved)
- Question: If a `/raw` request is rejected before process start (policy violation, invalid input), should the server return:
  1. Standard HTTP error status + JSON body (no stream), or
  2. `200 OK` with NDJSON stream containing an immediate `error` event?
- Why it matters: This affects client error handling model and observability, and whether all outcomes are normalized into the event stream.
- Answer: Return standard HTTP error status + JSON body (no stream).
- Decision/Impact: `run-remote` must handle both streaming success responses and non-streaming HTTP error responses for rejected requests.

## Q7 (resolved)
- Question: On successful `/raw` execution, should `run-remote` exit with the same exit code reported by the remote process?
- Why it matters: Exit-code parity determines scriptability and CI behavior when replacing local command execution with `run-remote`.
- Answer: Yes, mirror remote exit code exactly.
- Decision/Impact: `run-remote` will propagate remote process exit status for normal completion; client/network/protocol failures can use distinct local error codes.

## Q8 (resolved)
- Question: For `/raw` request JSON, should we reuse the existing field name `executable` (same as MCP tool input), or introduce `command` and map it internally?
- Why it matters: Reusing `executable` keeps parity with existing server/tool schema and avoids duplicate request models.
- Answer: Reuse `executable`.
- Decision/Impact: `/raw` request contract will align with `RunNetworkToolInput` naming and parsing.

## Q9 (resolved)
- Question: For this feature, should `/raw` use the same trust boundary as existing `/mcp` (no new auth layer), or should we add endpoint-level authentication now?
- Why it matters: Auth requirements significantly change protocol design, rollout complexity, and client configuration.
- Answer: Same trust model as current `/mcp` (no new auth in this feature).
- Decision/Impact: `/raw` is introduced without endpoint-level auth changes; security posture remains policy-based execution within existing network/container boundaries.

## Q10 (resolved)
- Question: What should `run-remote` forward by default for environment variables?
- Why it matters: Default env forwarding affects secret exposure risk and convenience.
- Options:
  1. Forward none by default; only variables listed in `--keep-env`.
  2. Forward all local env by default; `--keep-env` only narrows when set.
  3. Forward a fixed safe baseline (for example `HOME`, `LANG`) plus `--keep-env` additions.
- Answer: Forward none by default; only `--keep-env` variables.
- Decision/Impact: `run-remote` defaults to no caller env forwarding, reducing accidental secret leakage and making forwarded env explicit.

## Q11 (resolved)
- Question: Should each streamed output chunk carry UTF-8 text (`data` string) or raw bytes encoded as base64?
- Why it matters: Text is simpler and matches existing tool behavior; base64 preserves exact bytes for binary output at added complexity.
- Answer: Base64-encoded raw bytes chunks.
- Decision/Impact: `/raw` event schema will carry output payloads as base64 to preserve exact stream bytes end-to-end.

## Q12 (resolved)
- Question: Should `/raw` keep the same output cap as existing tool behavior (truncate each stream after 1 MiB), or stream full output without that cap?
- Why it matters: This changes resource usage and parity guarantees with current `run_network_tool`.
- Answer: Stream full output with no 1 MiB cap.
- Decision/Impact: `/raw` execution path will not apply current per-stream truncation limits, enabling full replay completeness.

## Q13 (resolved)
- Question: Should `run-remote` send the caller's current working directory as `cwd` by default?
- Why it matters: Default `cwd` affects command behavior parity with local execution and remote reproducibility.
- Answer: Yes, always send local `cwd` by default.
- Decision/Impact: `run-remote` will include the caller's current working directory in each request unless an explicit override behavior is introduced later.

## Q14 (resolved)
- Question: If `--keep-env` includes a variable that is not set locally, should `run-remote` fail fast or silently skip it?
- Why it matters: This affects UX predictability and script robustness.
- Answer: Fail fast with a clear error listing missing vars.
- Decision/Impact: `run-remote` performs preflight validation of all `--keep-env` names and aborts before issuing the request if any are unset.

## Q15 (resolved)
- Question: Should `run-remote` require `--` to separate client options from remote command/args?
- Why it matters: This determines CLI parsing clarity and avoids ambiguity between client flags and command flags.
- Answer: Yes, require `--` in v1.
- Decision/Impact: CLI usage is standardized as `run-remote [client-options] -- <executable> [args...]`.
