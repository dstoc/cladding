---
name: jj-protected-workspaces
description: Use when working on a project within /home/user/workspace/code or code-agent.
---

# Protected jj Workspaces

This environment separates canonical project repositories from agent working
copies:

- `/home/user/workspace/code/<repo>` contains canonical project repositories.
  Treat these as read-only source-of-truth projects.
- `/home/user/workspace/code-agent/<workspace>` contains writable agent workspaces.
  Make file edits and run normal development commands from here.

When a task names a repository, prefer the matching workspace under
`code-agent/`. If you are in `code/`, do not edit files there directly; create
or switch to the corresponding `code-agent/` workspace.

## Allowed jj Mutations

Only a narrow set of `jj` mutations is currently allowed:

Use these exact argument forms. The allowlist is strict about argument order and
shape.

- Create an agent workspace from a canonical project:

  ```sh
  cd /home/user/workspace/code/<repo>
  jj workspace add ../../code-agent/<workspace> --name agent-<workspace>
  ```

- Update stale workspace metadata from an agent workspace:

  ```sh
  cd /home/user/workspace/code-agent/<workspace>
  jj workspace update-stale
  ```

- Commit the current agent workspace change:

  ```sh
  cd /home/user/workspace/code-agent/<workspace>
  jj commit -m "message"
  ```

Do not use other mutating `jj` commands.

## Read-Only jj Operations

Read-only inspection commands are permitted and should be used as needed:

```sh
jj status
jj log
jj show
jj diff
```

Use these commands to inspect history, review diffs, and verify the working
copy before committing.
