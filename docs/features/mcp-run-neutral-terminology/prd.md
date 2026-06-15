# PRD: Neutral `mcp-run` Terminology

## Objective
Remove network-specific terminology from `mcp-run` public protocols, public Rust API names, docs, and internal identifiers.

`mcp-run` is now used by both `nw-sandbox` and `fs-sandbox`. The crate should describe what it actually does: policy-enforced command execution. Network access is a property of one caller/container, not a property of the server, MCP tool, request type, or execution engine.

This is a breaking rename. No backwards-compatible aliases are required.

## Use Cases
1. An agent calls the MCP tool exposed by an `fs-sandbox` and should not see a tool named `run_network_tool`.
2. A developer reading `mcp-run` code should not infer that policy-enforced execution is network-specific.
3. A future policy-check API should use neutral naming from the start and share neutral request/authorization primitives.

## Functional Requirements

### 1. Scope and Placement
1. Implementation is limited to the `crates/mcp-run` crate plus repository docs/tests that mention its public API.
2. Runtime behavior must not change:
- command execution remains no-shell;
- policy validation remains pre-execution;
- `/raw` request and stream semantics remain unchanged;
- `run-remote` behavior remains unchanged.
3. This change is a rename only, except where tests/docs need wording updates.

### 2. MCP Protocol Rename
1. Rename the MCP tool from `run_network_tool` to `run_command`.
2. Remove the old `run_network_tool` tool name entirely.
3. Update MCP server instructions and metadata to use neutral language:
- server name should be `mcp-run`;
- title should describe policy-enforced command execution;
- description should not say "network-capable".
4. Existing MCP clients that call `run_network_tool` should fail with normal unknown-tool behavior after this change.

### 3. Public Rust API Rename
1. Rename public request/response types:
- `RunNetworkToolInput` to `RunCommandInput`
- `RunNetworkToolOutput` to `RunCommandOutput`
2. Rename public execution functions:
- `run_network_tool_impl` to `run_command_impl`
- `spawn_network_tool_process` to `spawn_command_process`
3. Rename public server type:
- `NetworkMcpServer` to `McpRunServer`
4. Update `crates/mcp-run/src/lib.rs` exports to expose only the new names.
5. Do not keep deprecated type aliases or wrapper functions.

### 4. Internal Naming Cleanup
1. Update internal imports, tests, local variables, and helper function names that include `network` only because of the old tool name.
2. Keep references to actual network errors or HTTP/network transport failures where they are semantically correct.
3. Keep `nw-sandbox` terminology outside `mcp-run`; this proposal does not rename cladding component names.

### 5. HTTP API Compatibility
1. Keep `POST /raw` unchanged.
2. Keep the raw JSON request body fields unchanged:
- `executable`
- `args`
- `cwd`
- `env`
3. Keep NDJSON stream events unchanged.
4. The old Rust type name `RunNetworkToolInput` is internal to the current implementation of `/raw`; after this change `/raw` should use `RunCommandInput` while preserving the same wire contract.

### 6. Documentation Updates
1. Update `crates/mcp-run/README.md`:
- `/mcp` exposes `run_command`;
- examples and schema section use neutral names;
- remove wording that frames the crate as network-specific.
2. Update feature docs and notes that refer to current code behavior where useful for current implementation accuracy.
3. Do not rewrite historical design documents solely to erase old context unless those docs are used as current guidance.

### 7. Testing Requirements
1. Update MCP integration tests to expect `run_command`.
2. Update crate unit tests to compile against renamed public types/functions.
3. Add or update a regression assertion that the MCP tool list does not include `run_network_tool`.
4. Existing `/raw`, policy, and `run-remote` behavior tests must continue passing.

## Non-Goals
1. No compatibility alias for `run_network_tool`.
2. No change to Rego input shape or policy package/query names.
3. No change to `/raw` endpoint path or JSON field names.
4. No change to `run-remote` CLI syntax.
5. No cladding pod/container/config rename.

## Suggested Implementation Shape
1. Start in `crates/mcp-run/src/executor.rs` by renaming request/output structs and execution functions.
2. Update `crates/mcp-run/src/raw.rs` and `crates/mcp-run/src/remote.rs` imports to use `RunCommandInput`.
3. Update `crates/mcp-run/src/mcp.rs`:
- rename `NetworkMcpServer` to `McpRunServer`;
- rename the tool method to `run_command`;
- update tool metadata and server info.
4. Update `crates/mcp-run/src/lib.rs` public exports.
5. Run `cargo fmt` and then fix compile/test fallout across the workspace.

## Verification
1. `cargo fmt --check` succeeds.
2. `cargo test -p mcp-run` succeeds.
3. `cargo test --workspace` succeeds.
4. MCP tool listing includes `run_command`.
5. MCP tool listing does not include `run_network_tool`.
6. `POST /raw` accepts the same JSON body as before and streams the same event format.

## Success Criteria
1. No `run_network_tool`, `RunNetworkTool`, `spawn_network_tool_process`, `run_network_tool_impl`, or `NetworkMcpServer` references remain in active crate code.
2. The MCP protocol exposes `run_command` as the command execution tool.
3. Existing non-MCP execution behavior remains unchanged.
4. No backwards-compatibility aliases are present.
