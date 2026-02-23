---
name: run-with-network-tutor
description: How to use the run-with-network helper to run commands with network access  
---

## Core Usage Rules

- Always include the `--` delimiter between helper options and the command.
- Forward variables with `--keep-env` and ensure they exist in the current shell.
- Keep environment variables only when explicitly forwarded.

## Required Syntax

Use:

```bash
run-with-network [--keep-env=VAR] -- <command> <args>
```

E.g.

```bash
run-with-network -- cargo fetch
```

Do not omit the `--` delimiter.

## Environment Forwarding

- Use `--keep-env=VAR` or `--keep-env=VAR1,VAR2`.
- Ensure each variable is set before forwarding (for example, via `export VAR=value`).

## Troubleshooting Flow

1. Identify whether the error is syntax, policy, or network.
2. If the error mentions a missing `--` delimiter, add it.
3. If the error mentions missing local env vars, set them and retry.
4. If the error indicates a policy violation, inspect the Rego policy directory.
5. If the error indicates a network block, inspect the domain allowlist.

## Policy and Allowlist Checks

- Inspect Rego policy at `/opt/config/sandbox_commands/` using shell commands (`ls`, `rg`, or `less`).
- The decision query is `data.sandbox.main.allow`, so check `main.rego` and the command package (for example `package sandbox.curl`).
- Inspect allowed domains at `/opt/config/sandbox_domains.lst`.
- Match arguments against Rego rules in the command package (for example `package sandbox.curl`) and adjust commands accordingly.

## Error Patterns and Fixes

- "missing required '--' delimiter": insert `--` before the command.
- "local environment variable(s) are not set": export the missing variable before forwarding.
- "Command not allowed": executable not in policy.
- "Policy deny-all is active" or "Policy evaluation failed": fix Rego syntax or rule logic in `/opt/config/sandbox_commands/`.
- Network timeouts or connection failures: check domain allowlist and use only listed domains.
