# mcp-run

`mcp-run` is a policy-enforced command runner for constrained environments.

It exposes two HTTP endpoints on one server:

- `/mcp`: MCP Streamable HTTP endpoint with tool `run_network_tool`
- `/raw`: NDJSON streaming endpoint for direct command execution

Policy decisions are made by [Rego](https://www.openpolicyagent.org/docs/policy-reference) modules loaded from `POLICY_DIR`.

## Security Model

- Commands are executed without shell wrappers (`tokio::process::Command`), so no shell interpolation is used.
- Policy is always checked before process spawn.
- Rego input includes:
  - `input.command`: executable token requested by client
  - `input.path`: resolved absolute executable path
  - `input.hash`: SHA-256 hash of the resolved executable file (lowercase hex)
  - `input.args`: argument list
  - `input.env`: forwarded environment map
- Runtime is fail-closed:
  - if policy load fails at startup, server still starts but denies all requests
  - if policy reload fails, engine switches to deny-all until a valid policy set is loaded

## Configuration

Environment variables:

- `MCP_BIND_ADDR` (optional): bind address, default `127.0.0.1:8000`
- `POLICY_DIR` (recommended): directory containing `.rego` policy files

Example:

```bash
export MCP_BIND_ADDR=0.0.0.0:3000
export POLICY_DIR=/opt/config/sandbox_commands
mcp-run
```

## Build and Run

From this folder:

```bash
cargo run
```

Or build binaries:

```bash
cargo build --release
# server
./target/release/mcp-run
# helper client
./target/release/run-remote
```

## Policy Directory Layout

Minimal layout:

```text
sandbox_commands/
  main.rego
  curl.rego
```

Larger layout (examples below):

```text
sandbox_commands/
  main.rego
  curl.rego
  python.rego
  date.rego
  build.rego
  echo.rego
  toolx.rego
  toolx_flexible.rego
  toolx_segments.rego
```

## Decision Contract

`mcp-run` evaluates this Rego query:

```text
data.sandbox.main.allow
```

Your modules should produce a single boolean `allow` decision.

Router pattern (recommended):

```rego
package sandbox.main

default allow = false

allow if {
    data.sandbox[input.command].allow
    env_allowed
}

env_allowed if {
    count(object.keys(input.env)) == 0
}

env_allowed if {
    data.sandbox[input.command].allow_env
}
```

## Rego Examples

### `curl.rego`

```rego
package sandbox.curl

default allow = false
default allow_env = false

# Allow: curl -I https://example.com
allow if {
    input.args == ["-I", "https://example.com"]
    startswith(input.path, "/usr/bin/")
}

# This command intentionally allows no forwarded env vars.
```

### `curl_pinned.rego` (pin executable hash)

```rego
package sandbox.curl

default allow = false
default allow_env = false

# Allow only the expected curl binary and invocation.
allow if {
    input.args == ["-I", "https://example.com"]
    input.path == "/usr/bin/curl"
    input.hash == "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

### `python.rego`

```rego
package sandbox.python

default allow = false
default allow_env = false

# Allow: python -m http.server
allow if {
    input.args == ["-m", "http.server"]
}

# Optional override example:
# allow_env if {
#     input.env.PYTHONUNBUFFERED == "1"
# }
```

### `date.rego` (no args)

```rego
package sandbox.date

default allow = false
default allow_env = false

allow if {
    count(input.args) == 0
}
```

### `build.rego` (must contain `--target x86`)

```rego
package sandbox.build

default allow = false
default allow_env = false

allow if {
    some i
    i + 1 < count(input.args)
    input.args[i] == "--target"
    input.args[i + 1] == "x86"
}
```

### `echo.rego` (ban a specific arg)

```rego
package sandbox.echo

default allow = false
default allow_env = false

allow if {
    not banned_arg_present
}

banned_arg_present if {
    some i
    input.args[i] == "--unsafe"
}
```

### `toolx.rego` (exactly 3 args from allowlist)

```rego
package sandbox.toolx

default allow = false
default allow_env = false

allowed_args := {"--fast", "--verbose", "--dry-run"}

allow if {
    count(input.args) == 3
    every a in input.args {
        allowed_args[a]
    }
}
```

### `toolx_flexible.rego` (up to 3 args from allowlist)

```rego
package sandbox.toolx_flexible

default allow = false
default allow_env = false

allowed_args := {"--fast", "--verbose", "--dry-run"}

allow if {
    count(input.args) <= 3
    every a in input.args {
        allowed_args[a]
    }
}
```

### `toolx_segments.rego` (mix singles and pair segments)

```rego
package sandbox.toolx_segments

default allow = false
default allow_env = false

single_args := {"--fast", "--verbose", "--dry-run"}

allow if {
    valid_from(0)
}

valid_from(i) if {
    i == count(input.args)
}

valid_from(i) if {
    i < count(input.args)
    single_args[input.args[i]]
    valid_from(i + 1)
}

valid_from(i) if {
    i + 1 < count(input.args)
    input.args[i] == "--target"
    input.args[i + 1] == "x86"
    valid_from(i + 2)
}
```

## Raw Endpoint (`/raw`) Usage

Request:

```bash
curl -sS -N -X POST http://127.0.0.1:8000/raw \
  -H 'content-type: application/json' \
  -d '{
    "executable": "curl",
    "args": ["-I", "https://example.com"],
    "cwd": "/tmp",
    "env": {}
  }'
```

Response is NDJSON events:

- `{ "event": "start" }`
- `{ "event": "stdout", "data_b64": "..." }`
- `{ "event": "stderr", "data_b64": "..." }`
- `{ "event": "exit", "exitCode": 0 }`
- or `{ "event": "error", "message": "..." }`

## MCP Tool Contract (`/mcp`)

Tool name: `run_network_tool`

Input schema:

- `executable: string`
- `args: string[]` (optional)
- `cwd: string | null` (optional)
- `env: object<string,string> | null` (optional)

Output schema:

- `stdout: string`
- `stderr: string`
- `exitCode: number | null`

Output from MCP tool calls is capped at 1 MiB per stream; truncated output appends `...truncated...`.

## `run-remote` Helper

`run-remote` calls `/raw` and streams stdout/stderr locally.

- Requires `RUN_REMOTE_SERVER` (full URL, usually `http://127.0.0.1:8000/raw`)
- Requires `--` delimiter before executable
- Supports env forwarding with `--keep-env`

Examples:

```bash
export RUN_REMOTE_SERVER=http://127.0.0.1:8000/raw

# simplest
run-remote -- curl -I https://example.com

# forward selected env vars
run-remote --keep-env=API_TOKEN,CI -- curl -I https://example.com

# equivalent two-arg keep-env form
run-remote --keep-env API_TOKEN -- curl -I https://example.com
```

## Live Reload Behavior

When `POLICY_DIR` is set, `mcp-run` watches the directory recursively.

- valid edit -> new policy set becomes active
- invalid edit -> deny-all becomes active
- subsequent valid edit -> service recovers automatically

This lets operators update policy without restarting the process.

## Troubleshooting

- `Command not allowed: <cmd>`
  - `data.sandbox.main.allow` evaluated to `false`
  - verify router and command package names match `input.command`
- `Policy deny-all is active: ...`
  - policy set failed to compile/load
  - fix Rego syntax or policy directory contents
- `Policy evaluation failed for '<cmd>': ...`
  - query returned an evaluation error
  - inspect rule logic and data shape assumptions
- `Failed to resolve executable path for '<cmd>': ...`
  - executable not found on `PATH` or not executable

## Development

Run checks:

```bash
cargo check
cargo test --lib
```

Entrypoints:

- server: `src/main.rs`
- MCP/HTTP wiring: `src/mcp.rs`
- raw endpoint: `src/raw.rs`
- policy engine: `src/policy.rs`
- helper client: `src/bin/run-remote.rs`, `src/remote.rs`
