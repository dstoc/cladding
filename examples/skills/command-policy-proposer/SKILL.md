---
name: command-policy-proposer
description: Propose or modify Rego sandbox command policies for mcp-run (PROPOSAL_DIR ~/proposed-sandbox-commands; runtime POLICY_DIR /opt/config/sandbox_commands). Use when asked to draft new .rego files, update existing policy logic, or review command allowlists with safe argument constraints and network/data-exfiltration safeguards.
---

# Command Policy Proposer

## Workflow

1. Confirm target command(s), minimal allowed behavior, and where the policy will live.
2. Design **strict** argument constraints that only allow the intended safe use.
3. Explicitly prevent untrusted code execution and data exfiltration.
4. Draft or modify `.rego` files and explain the safety reasoning.
5. Call out any remaining risks or needed follow-ups.

## Safety-First Design Rules

- **Prefer exact argument lists** for narrowly scoped operations.
- Allow **only the minimal subcommand** needed (for example `fetch`, not `run`).
- Avoid any args that can **execute code**, **load plugins**, **spawn shells**, or **read arbitrary files**.
- Avoid args that can **send data to arbitrary URLs/domains**, unless the domain is fixed and whitelisted.
- Disallow or tightly constrain **flags that accept paths**, **user-supplied scripts**, or **network endpoints**.
- Keep `allow_env` **false** by default; allow only specific, justified env keys.
- If the command supports **scripting or config files**, disallow those flags or require exact safe paths.

### Common high-risk argument patterns (avoid or hard-restrict)

- `--config`, `-c`, `--rc`, `--init`, `--profile`, `--load`, `--plugin`
- `-e`, `-m`, `-c` (language execution flags)
- `--eval`, `--exec`, `--script`
- `--output`/`-o` when it can write arbitrary paths
- Any arg that takes a **URL** or **domain** unless hard-coded

## Rego Contract (mcp-run)

- Write proposal files under `~/proposed-sandbox-commands` (not `/opt/config/sandbox_commands`, which may be read-only).
- Treat `/opt/config/sandbox_commands` as the runtime reference location to inspect existing effective policies.
- Decision query: `data.sandbox.main.allow`
- Rego input shape:
  - `input.command`: executable token
  - `input.path`: resolved absolute path
  - `input.args`: argument list
  - `input.env`: forwarded environment map
- Deny-by-default: `default allow = false`

## Templates

### Minimal router (`main.rego`)

Use only if asked to author or update the router:

```rego
package sandbox.main

default allow = false

default env_allowed = false

env_allowed if {
    count(object.keys(input.env)) == 0
}

env_allowed if {
    data.sandbox[input.command].allow_env
}

allow if {
    data.sandbox[input.command].allow
    env_allowed
}
```

### Single command policy

```rego
package sandbox.<command>

default allow = false

default allow_env = false

# Allow: <command> <exact args>
allow if {
    input.args == ["<arg1>", "<arg2>"]
}
```

## Examples Reference

Read `references/patterns.md` when you need example patterns (exact match, allowlists, segmented args, etc.).

## Output Expectations

- Provide the proposed `.rego` file(s).
- Explain the safety reasoning for allowed args and explicitly call out disallowed risky args.
- If the request is ambiguous, ask for clarification on **exact arguments** or **allowed domains**.
