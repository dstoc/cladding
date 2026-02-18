# Notes: Network MCP Rust Reimplementation

## Research Scope
- Feature request: Reimplement `clawmini/docs/03_network_mcp` server in Rust, based on existing TS implementation in `clawmini/mcp-servers/network`, using initialized Cargo project `mcp-run`.
- Constraint from request: target hardened container already provides `http_proxy`, `https_proxy`, `no_proxy`, etc. Do not include proxy setup/configuration, and do not keep existing env filtering behavior.

## Existing Product Findings

### Existing TS MCP server behavior (`clawmini/mcp-servers/network`)
- Main tool: `run_network_tool(executable, args, cwd?, env?)` in `clawmini/mcp-servers/network/src/tools/run_network_tool.ts`.
- Server registration: tool is registered via MCP SDK in `clawmini/mcp-servers/network/src/server.ts`.
- Execution model: uses `spawn(..., shell: false)` and returns `{ stdout, stderr, exitCode }`.
- Policy enforcement: `commandPolicy.validate(executable, args, env)` blocks disallowed commands/env.
- Policy source: JSON policy file (`POLICY_FILE`) loaded by `clawmini/mcp-servers/network/src/policy/loader.ts`.
- Policy validator supports:
  - `exact` match
  - `regex` match
  - `hash` file hash check (sha256)
  - optional positional/required semantics.
- Output limit: truncates stdout/stderr at 1MB each.
- Current TS implementation additionally injects proxy env vars and filters sensitive env keys; this is now explicitly out-of-scope per request.

### Existing networking/proxy architecture (outside Rust scope per request)
- TS server includes internal proxy server (`clawmini/mcp-servers/network/src/proxy/*`) and `start-proxy.ts`.
- Startup scripts orchestrate proxy + MCP socket (`clawmini/scripts/start-network-mcp.sh`).
- Current infra uses separate Squid proxy pod (`pods.yaml`, `config/squid.conf`, `scripts/proxy_startup.sh`).
- Request explicitly excludes proxy setup/configuration for Rust rewrite.

### Runtime/container integration context
- Sandbox container image currently built from `Containerfile.sandbox` using Node artifact and `supergateway`.
- `scripts/start_network_gateway.sh` launches `supergateway --stdio "node /opt/network-mcp/dist/index.js"` exposing HTTP endpoints.
- `pods.yaml` runs `sandbox-app` with `POLICY_FILE` and proxy env vars pre-set in container env.
- `mcp-run` cargo project exists but is currently scaffold-only (`mcp-run/src/main.rs` hello world).

## Clarified Decisions From Q&A
- Keep command policy configuration/features aligned with TS implementation (arg checks, positional/required semantics, hash checks, env allowlist).
- Remove proxy-domain policy from Rust implementation and configuration; `allowedHosts` is not supported.
- Reject legacy `allowedHosts` in policy as invalid configuration (fail validation/startup).
- Use Rust `rmcp` server exposing MCP over HTTP/SSE directly.
- Implement entirely in `mcp-run`; defer sandbox/pods/container wiring changes to a later feature.
- API breaking changes are acceptable, but functional parity remains the primary goal.
- Match current implementation behavior for timeout semantics (no newly introduced default timeout).

## Artifacts Produced
- `docs/features/network-mcp-rust-reimplementation/questions.md`: Q&A log with resolved scope decisions.
- `docs/features/network-mcp-rust-reimplementation/prd.md`: Draft PRD for Rust reimplementation scoped to command running + configuration/policy parity.

### Legacy/Reference product docs
- Source PRD (`clawmini/docs/03_network_mcp/prd.md`) includes proxy + sandbox orchestration and strict network egress controls.
- Implementation docs in same folder (`notes.md`, `questions.md`, `tickets.md`, `development_log.md`) reflect finalized TS behavior and testing.

## Gaps / Ambiguities To Resolve
- Exact compatibility target for Rust tool and policy schema:
  - Resolved: keep full TS semantics for command policy and env allowlist.
  - Adjustment: drop proxy domain-list behavior/config (`allowedHosts`).
- Expected transport in Rust:
  - Resolved: expose HTTP/SSE directly (rmcp crate).
- Path inconsistency in user instructions:
  - Step 1 says `./docs/features/FEATURE`
  - Step 4 says `./docs/FEATURE/prd.md`
  - Resolved: canonical path is `./docs/features/FEATURE/prd.md`.
- Policy backward compatibility detail:
  - Resolved: `allowedHosts` must be invalid and fail validation in Rust.
- Whether PRD should include migration/deprecation plan for Node-based `Containerfile.sandbox` and startup command.
  - Resolved: no migration work in this feature; implement Rust server in new directory only.
- API compatibility:
  - Resolved: breaking API changes are acceptable for Rust implementation.

## Working Assumptions (to validate with user)
- Keep tool name `run_network_tool` and input/output shape stable to reduce client changes.
- Keep policy-file-driven command allowlisting as core configuration part.
- Drop built-in proxy process, proxy host filtering, and TS env sanitization/injection logic.
- Keep TS-equivalent command policy rule capabilities (`exact|regex|hash`, position, required, env allowlist); remove host/domain policy.
- Use Rust `rmcp` server exposing HTTP/SSE directly instead of stdio + supergateway.
- Enforce strict config validation: reject legacy `allowedHosts`.
- Limit scope to `mcp-run` implementation only; defer sandbox/pods/container integration updates.
- Tool API does not need strict TS wire-compatibility.
