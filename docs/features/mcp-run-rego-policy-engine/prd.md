# PRD: Rego Policy Engine for `mcp-run`

## Objective
Replace the current JSON rule-validation implementation in `mcp-run` with a Rego-based policy engine using the `regorus` crate, while preserving execution safety and enabling live policy reloads from a policy directory.

This feature introduces:
- Rego module evaluation for command authorization.
- Directory-based policy loading and file-watch reload.
- A fail-closed runtime model (deny all when policy state is invalid).
- Temporary dual-format compatibility (Rego + legacy JSON), with JSON removal deferred to a follow-up feature.

## Use Cases
1. An operator defines command policy in Rego modules and updates policy files without restarting `mcp-run`.
2. A command request is authorized by a router policy (`sandbox.main`) delegating to command-specific modules.
3. Policy authors enforce command, path, args, and env constraints directly in Rego.
4. If policy files are malformed or invalid during edits, command execution is automatically denied until policies are valid again.
5. Existing installations using `sandbox_commands.json` continue to work during migration while Rego policy rollout is completed.

## Functional Requirements

### 1. Scope and Placement
1. Implementation is in `mcp-run`.
2. Shared policy enforcement applies consistently to both `/mcp` and `/raw` execution paths.
3. No changes to auth/trust model are introduced in this feature.

### 2. Policy Sources and Format
1. `POLICY_DIR` points to a directory containing `.rego` files (for example `config/sandbox_commands`).
2. Rego directory mode is the new preferred/default policy model.
3. `POLICY_FILE` continues to represent the legacy JSON policy file during transition.
4. JSON compatibility is explicitly transitional and slated for later removal.
5. Source selection precedence is deterministic:
- if `POLICY_DIR` is set, Rego mode is active;
- otherwise if `POLICY_FILE` is set, legacy JSON mode is active;
- if neither yields a valid policy state, runtime is deny-all.

### 3. Rego Decision Contract
1. Authorization is keyed primarily by `input.command`.
2. Rego input object includes at minimum:
- `command`: requested command token.
- `path`: resolved executable path.
- `hash`: SHA-256 hash of the resolved executable file, encoded as lowercase hex.
- `args`: requested args.
- `env`: requested environment map.
3. Router-style policy support is required, with a model equivalent to:
- `package sandbox.main`
- `default allow = false`
- `allow { data.sandbox[input.command].allow }`
4. The engine must evaluate an `allow` decision as boolean authorization output.
5. If decision is false or evaluation fails, invocation is denied.

### 4. Command Execution Integration
1. Process spawning remains no-shell (`tokio::process::Command` direct execution).
2. Policy validation remains a pre-execution gate for all invocations.
3. Existing execution semantics (cwd handling, output behavior per endpoint) remain unchanged unless required for policy input enrichment.

### 5. Live Reload via File Watching
1. `mcp-run` watches the policy directory for file changes (create/modify/remove/rename).
2. On change, it reloads/recompiles policies and atomically swaps active policy engine state.
3. Reload behavior is fail-closed:
- if reload succeeds, new policy state becomes active;
- if reload fails, active policy state becomes deny-all.
4. Reload failures must be logged with actionable diagnostics.

### 6. Startup Behavior
1. Server starts even if policy load is invalid.
2. Invalid startup policy state results in deny-all behavior until a valid policy set is loaded.
3. Startup logs clearly indicate whether policy state is valid or deny-all fallback.

### 7. Transitional Legacy JSON Support
1. During migration, legacy JSON policy loading is supported temporarily.
2. Rego and JSON source selection must follow the `POLICY_DIR`/`POLICY_FILE` precedence contract.
3. If neither Rego nor JSON policy can produce a valid state, engine remains deny-all.
4. Logs must indicate which policy engine/format is active.

### 8. Error Handling and Observability
1. Policy-denied invocations continue returning user-actionable errors.
2. Distinguish in logs:
- explicit deny decisions;
- policy evaluation/load errors;
- reload transition events.
3. Include enough context for debugging without leaking sensitive env values.

### 9. Documentation and Config Updates
1. Update templates and docs to introduce Rego directory policy as the primary configuration model.
2. Document the temporary JSON compatibility period and planned removal.
3. Provide a minimal example policy bundle (router + one command module) matching production contract.

### 10. Testing Requirements
1. Unit tests for Rego evaluator integration and decision mapping.
2. Unit tests for input shape correctness (`command`, `path`, `hash`, `args`, `env`).
3. Integration tests for allow/deny behavior across `/mcp` and `/raw`.
4. Watcher/reload tests:
- valid policy update becomes active;
- invalid update transitions to deny-all;
- subsequent valid update recovers from deny-all.
5. Startup tests:
- valid policy startup;
- invalid policy startup with deny-all active.
6. Transitional tests for JSON fallback behavior and deterministic selection.

## Example Rego Policy Bundle
Example directory layout:

```text
config/sandbox_commands/
  main.rego
  curl.rego
  python.rego
  date.rego
  build.rego
  echo.rego
  toolx.rego
```

Example `config/sandbox_commands/main.rego`:

```rego
package sandbox.main

default allow = false

allow {
    command_allowed
    env_allowed
}

command_allowed {
    data.sandbox[input.command].allow
}

# Default-deny for env: only empty env is allowed unless command policy
# explicitly exposes allow_env.
env_allowed {
    count(object.keys(input.env)) == 0
}

env_allowed {
    data.sandbox[input.command].allow_env
}
```

Example `config/sandbox_commands/curl.rego`:

```rego
package sandbox.curl

default allow = false
default allow_env = false

# Allow: curl -I https://example.com
allow {
    input.args[0] == "-I"
    input.args[1] == "https://example.com"
    startswith(input.path, "/usr/bin/")
}

# This command intentionally allows no forwarded env vars.
```

Example `config/sandbox_commands/python.rego`:

```rego
package sandbox.python

default allow = false
default allow_env = false

# Allow: python -m http.server
allow {
    input.args[0] == "-m"
    input.args[1] == "http.server"
}

# Optional override example:
# allow_env {
#     input.env.PYTHONUNBUFFERED == "1"
# }
```

Example `config/sandbox_commands/date.rego` (no args allowed):

```rego
package sandbox.date

default allow = false
default allow_env = false

allow {
    count(input.args) == 0
}
```

Example `config/sandbox_commands/build.rego` (must contain `["--target", "x86"]` in order, anywhere):

```rego
package sandbox.build

default allow = false
default allow_env = false

allow {
    some i
    i + 1 < count(input.args)
    input.args[i] == "--target"
    input.args[i + 1] == "x86"
}
```

Example `config/sandbox_commands/echo.rego` (ban a specific argument):

```rego
package sandbox.echo

default allow = false
default allow_env = false

allow {
    not banned_arg_present
}

banned_arg_present {
    some i
    input.args[i] == "--unsafe"
}
```

Example `config/sandbox_commands/toolx.rego` (allowlist of exactly 3 arguments, any order):

```rego
package sandbox.toolx

default allow = false
default allow_env = false

allowed_args := {"--fast", "--verbose", "--dry-run"}

allow {
    count(input.args) == 3
    every a in input.args {
        allowed_args[a]
    }
}
```

Example `config/sandbox_commands/toolx_flexible.rego` (allowlist of up to 3 arguments, any order, including none):

```rego
package sandbox.toolx_flexible

default allow = false
default allow_env = false

allowed_args := {"--fast", "--verbose", "--dry-run"}

allow {
    count(input.args) <= 3
    every a in input.args {
        allowed_args[a]
    }
}
```

Example `config/sandbox_commands/toolx_segments.rego` (0 or more args from allowlist, where `["--target", "x86"]` is an allowed pair):

```rego
package sandbox.toolx_segments

default allow = false
default allow_env = false

single_args := {"--fast", "--verbose", "--dry-run"}

allow {
    valid_from(0)
}

valid_from(i) {
    i == count(input.args)
}

valid_from(i) {
    i < count(input.args)
    single_args[input.args[i]]
    valid_from(i + 1)
}

valid_from(i) {
    i + 1 < count(input.args)
    input.args[i] == "--target"
    input.args[i + 1] == "x86"
    valid_from(i + 2)
}
```

## Non-Goals
1. Removing legacy JSON support in this feature (handled in a later feature).
2. Introducing endpoint authentication/authorization layers beyond existing trust model.
3. Changing command execution transport protocols (`/mcp`, `/raw`) beyond policy integration needs.
4. Reworking container/pod wiring or deployment rollout process outside `mcp-run` code/docs scope.
5. Adding a separate policy management API (upload/versioning) in this phase.

## Design Considerations
1. Safety-first policy runtime: fail-closed on invalid policy states.
2. Operational continuity: service remains up even when policy is temporarily broken.
3. Shared enforcement path prevents drift between MCP and raw endpoint behavior.
4. Rego policy model should remain simple and composable (router + command modules).
5. Transitional compatibility reduces migration risk for existing users.

## Technical Considerations
1. Introduce a policy engine abstraction to support both Rego and legacy JSON validators during transition.
2. Maintain thread-safe, atomically swappable policy state for concurrent request handling.
3. Resolve command path before policy evaluation so `input.path` is reliable for optional stricter rules.
4. Keep decision evaluation bounded and deterministic for request latency predictability.
5. Add a filesystem watcher (e.g., `notify`) with debouncing/coalescing to handle editor write bursts.
6. Ensure watcher lifecycle and shutdown are tied to application runtime cleanly.
7. Use structured logging for policy state transitions, reload attempts, and failure causes.
8. Avoid logging raw env values; log only keys/metadata as needed.

## Success Metrics
1. Correctness:
- Rego policies correctly allow/deny based on command/path/args/env input.
2. Safety:
- Invalid policy states consistently enforce deny-all behavior.
3. Reliability:
- Policy reloads apply without process restart and recover after policy fixes.
4. Parity:
- Both `/mcp` and `/raw` enforce the same active policy state.
5. Migration readiness:
- Existing JSON users remain functional during transition with clear upgrade path.
6. Operability:
- Logs make active policy mode/state and reload failures immediately diagnosable.

## Open Questions
1. Deprecation timeline: what release/milestone should remove JSON compatibility?
2. Rego module conventions: should we standardize naming/layout requirements beyond package naming (for example required router filename)?
