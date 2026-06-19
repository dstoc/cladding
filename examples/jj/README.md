# Protected jj workspaces for agents

This example protects canonical project repositories under `code/` from direct
mutation by the agent while still letting the agent do useful version-control
work.

The main threat model is an agent that acts maliciously, follows prompt
injection, or simply makes a bad tool call. In the normal agent environment,
the trusted project repositories are read-only, so the agent cannot directly
corrupt or destroy repository state. Mutation commands go through a separate
`fs-sandbox` and are allowed only when they match a narrow Rego policy.

## Model

The example uses two [`workspace-portal`](https://github.com/dstoc/workspace-portal)
views mounted at the same path inside different cladding components:

- the agent view exposes canonical repos under `code` as read-only and exposes
  agent workspaces under `code-agent` as writable
- the `fs-sandbox` view exposes both `code` and `code-agent` as writable, but is
  reachable only through policy-checked commands

The `jj` executable visible to the agent is a wrapper. It first asks the
`fs-sandbox` whether the requested `real_jj` invocation is allowed. Allowlisted
mutation commands run inside the `fs-sandbox`. Other commands fall back to local
`real_jj --ignore-working-copy` in the agent environment after a required
sandbox snapshot. If the sandbox snapshot fails, the fallback command fails
instead of showing stale working-copy state.

The wrapper is not a complete read-only command classifier by itself. The
boundary comes from the combination of the wrapper policy, the portal mounts,
and the `jj` immutable-heads config. In the intended layout, each agent
workspace has its own writable working copy under `code-agent`, while its
repository store still points back to the protected project under `code`.
Denied `jj` mutations therefore do not get the `fs-sandbox`'s writable view of
the canonical repository state.

Agent workspaces have immutable local `.jj` metadata in the normal agent view.
This prevents the agent from rewriting workspace metadata such as `.jj/repo`.
The wrapper snapshots the working copy through the `fs-sandbox` first, then
uses `--ignore-working-copy` for local fallback commands so read-only
inspection does not need to create lock files or update `.jj` locally.

The current policy allows only these mutations:

- `jj workspace add ../../code-agent/<repo> --name agent-<repo>`, from
  `/home/user/workspace/code/<repo>`
- `jj workspace update-stale`, from an agent workspace under
  `/home/user/workspace/code-agent`
- `jj commit -m <message> [<fileset>...]`, from an agent workspace under
  `/home/user/workspace/code-agent`
- `jj squash [--from <revset>] [--to <revset>] [<fileset>...]`, from an
  agent workspace under `/home/user/workspace/code-agent`
- `jj edit <revset>`, from an agent workspace under
  `/home/user/workspace/code-agent`
- `jj abandon [<revset>...]`, from an agent workspace under
  `/home/user/workspace/code-agent`
- `jj util snapshot`, from an agent workspace under
  `/home/user/workspace/code-agent`

Other read-only `jj` commands should keep working through the local fallback.
Additional mutation commands can be added over time by extending the Rego policy
with constrained arguments.

The writable `fs-sandbox` portal sets `readlink = false`. This is deliberate:
the sandbox runs as the same uid, so following symlinks through the writable view
would create an avoidable cross-mount escape risk. Disabling readlink keeps path
resolution inside the portal policy instead of trusting same-uid symlink targets.
For example, if the agent can create a symlink in its writable workspace while
`jj` is writing working-copy files, a same-uid sandbox that follows that symlink
could be tricked into writing through to a protected project under `code/`.

## Setup

```sh
cladding init
```

Define the agent workspace view:

```sh
mkdir workspace
workspace-portal start --bg --allow-other workspace
workspace-portal edit --workspace workspace
```

```toml
version = 1
readlink = true
immutable_segments = [".jj"]

[entries."code"]
target = "/home/YOU/dev/code"
mode = "ro"

[entries."code-agent"]
target = "/home/YOU/dev/code-agent"
mode = "rw"
```

Define the `fs-sandbox` workspace view:

```sh
mkdir workspace-fs-sandbox
workspace-portal start --bg --allow-other workspace-fs-sandbox
workspace-portal edit --workspace workspace-fs-sandbox
```

```toml
version = 1
readlink = false
immutable_segments = []

[entries."code"]
target = "/home/YOU/dev/code"
mode = "rw"

[entries."code-agent"]
target = "/home/YOU/dev/code-agent"
mode = "rw"
```

Place the [`jj` wrapper](./jj) in `.cladding/tools/bin` and the real `jj` as
`/opt/tools/bin/real_jj`.

Copy the Rego policy bundle in [`policy/`](./policy) into
`.cladding/config/fs_sandbox/`:

```sh
cp -R examples/jj/policy/. .cladding/config/fs_sandbox/
```

The bundle follows the same router pattern as the default sandbox policies.
`main.rego` routes by executable, `real_jj.rego` routes by `jj` subcommand, and
each file under `policy/jj/` allows one `jj` command shape. To add another
mutation later, add or copy in another command file under `policy/jj/`.

Define a `jj` config in `jj-config/config.toml`:

```toml
[user]
name = "..."
email = "...@....com"

[ui]
default-command = "log"

[revset-aliases]
'immutable_heads()' = '''
builtin_immutable_heads()
| bookmarks()
| remote_bookmarks(remote="*")
| present(default@)
'''
```

The `immutable_heads()` alias makes the ancestry of `default@`, bookmarks, and
remote bookmarks immutable to `jj` operations. This gives the agent a narrow
area to change: it can create and commit work in its own workspace, but it
cannot rewrite the user's default workspace ancestry or any bookmarked change.

That boundary is part of the review model. Agent-produced changes remain local
until the user decides to accept them, for example by reviewing the diff,
building or running the code locally, moving bookmarks, or pushing to a remote.
Within that boundary, non-immutable revisions are considered mutable working
area. Commands that accept revsets, such as `jj squash`, `jj edit`, and
`jj abandon`, can target unbookmarked mutable revisions selected by those
revsets. Users should bookmark or otherwise make important local work immutable
before exposing it to this model.

Configure cladding to enable `fs-sandbox` and bind the workspace mounts and jj-config in `.cladding/cladding.json`:

```json
{
  "name": "dev",
  "use_runsc": true,
  "nw_sandbox": {
    "enabled": true
  },
  "fs_sandbox": {
    "enabled": true
  },
  "agent": {},
  "mounts": [
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace"
    },
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace-fs-sandbox",
      "targets": [
        "fs-sandbox"
      ]
    },
    {
      "mount": "/home/user/workspace/.cladding",
      "ignore": true
    },
    {
      "mount": "/home/user/.config/jj",
      "hostPath": "../jj-config",
      "targets": [
        "agent",
        "nw-sandbox",
        "fs-sandbox"
      ],
      "readOnly": true
    }
  ]
}
```

## Usage

Create an agent workspace from one of the canonical project repositories:

```sh
cd /home/user/workspace/code/<repo>
jj workspace add ../../code-agent/<repo> --name agent-<repo>
```

Then work from the agent workspace:

```sh
cd /home/user/workspace/code-agent/<repo>
jj status
jj commit -m "describe the change"
```
