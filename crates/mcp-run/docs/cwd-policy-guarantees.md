# CWD Policy Guarantees

This document defines the security and policy guarantees for `mcp-run`'s trusted cwd policy input.

`mcp-run` exposes a Rego field, `input.cwd`, so policies can authorize commands based on the effective working directory where the command would run. The field is not copied from user input. It is produced by trusted Rust code during the shared pre-spawn authorization path.

The core invariant is:

> Rego receives the trusted cwd label produced by the fd-based resolver, and execution uses the same opened directory fd that produced that label.

## Terminology

### Raw cwd input

The cwd value supplied by the caller in the command request.

This value is untrusted. It is used only as resolver input and for diagnostics. It must not be passed to Rego as the authoritative cwd.

### Effective cwd input

The cwd value selected for a request after applying defaulting rules:

- if the request supplies `cwd`, that value is the effective cwd input;
- otherwise, `default_cwd` is the effective cwd input.

The effective cwd input must be absolute. Relative request cwd values are rejected. `default_cwd` is validated as absolute during server startup.

### Trusted cwd label

The normalized absolute lexical label produced by the trusted resolver.

This is the value passed to Rego as `input.cwd`.

The trusted cwd label is a policy label. It is not a globally unique filesystem identity and is not guaranteed to remain a live path after authorization.

### Authorized cwd fd

The opened directory fd produced by the resolver for the final cwd.

If Rego authorizes the command, execution changes cwd using this fd, not by re-resolving the raw path.

### Attacker-immutable parent directory

A parent directory is attacker-immutable when the attacker cannot create, delete, rename, or replace entries inside it.

For a path such as:

```text
/foo/bar/baz
```

the relevant parent directories are:

```text
/        controls the name `foo`
/foo     controls the name `bar`
/foo/bar controls the name `baz`
```

The final directory's contents matter for command behavior, but the stability of the final directory's name is controlled by its parent.

## Resolver Behavior

The implemented resolver is strict:

1. The effective cwd input must be absolute.
2. Relative cwd inputs are rejected.
3. Leading `//` is rejected.
4. `.` and `..` components are rejected.
5. Symlink components are rejected.
6. Magic-link components are rejected.
7. Repeated slashes and trailing slashes are normalized away, except for leading `//`.
8. Every accepted component is opened as a real directory relative to the already-open fd for its parent.
9. The resolver keeps the final opened directory fd alive through Rego validation and process spawn.
10. The resolver does not call `canonicalize` and then later rely on that string for enforcement.

## Positive Guarantees

When cwd resolution succeeds, `mcp-run` can guarantee the following.

1. `input.cwd` is produced by trusted host code, not copied from user input.
2. `input.cwd` is absolute and normalized:
   - it starts with `/`;
   - it has no empty components;
   - it has no `.` components;
   - it has no `..` components.
3. `input.cwd` is the trusted resolved lexical cwd label produced by the resolver without following symlinks or magic links.
4. At resolution time, every component in `input.cwd` was opened as a real directory relative to the already-open fd for its parent directory.
5. At resolution time, the final cwd fd was reached by walking the components represented by `input.cwd`.
6. The final cwd used for execution is the same opened directory fd that produced `input.cwd`.
7. If a symlink or magic link appears in the input path, the request is rejected before Rego.
8. If any component is `.`, `..`, or cannot be opened as a directory without following symlinks, the request is rejected before Rego.
9. If the parent directories along the path are attacker-immutable, then the corresponding path components cannot be attacker-retargeted during resolution.
10. Once resolution enters an attacker-mutable directory, later components are attacker-controlled namespace, even though they are still opened safely and represented accurately in the point-in-time cwd label.

## Namespace Immutability and Prefix Semantics

The trusted cwd label is a resolved lexical label, not a durable proof of global filesystem object identity.

Prefix policies over `input.cwd` are meaningful as policies over the namespace walked by the resolver at authorization time.

For an absolute path such as:

```text
/foo/bar/baz
```

each path component is controlled by its parent directory:

```text
/        controls the name `foo`
/foo     controls the name `bar`
/foo/bar controls the name `baz`
```

Therefore, the strength of a prefix policy depends on the immutability of the parent directories along that prefix.

When symlinks, magic links, `.`, `..`, relative paths, and mount aliases are rejected or out of scope:

1. If every parent directory needed to resolve the input path is attacker-immutable, then the resolved cwd label is the normalized input path, and the opened cwd fd corresponds to that label at resolution time.
2. If resolution enters an attacker-mutable directory, then later path components are part of attacker-controlled namespace. The resolver still opens each component safely and execution still uses the final opened fd, but policy should not treat later lexical components as belonging to an attacker-immutable tree.
3. If a mutable parent directory is renamed or modified after resolution, the held cwd fd still identifies the directory used for execution, but the `input.cwd` string may no longer be a live pathname for that fd.
4. A Rego prefix check such as `startswith(input.cwd, "/foo/bar/")` should only be treated as a trusted workspace/tree check when the deployment guarantees that the relevant parent directories for `/foo/bar` cannot be manipulated by the attacker.

In short: `input.cwd` records the trusted lexical path walked by `mcp-run` at authorization time. The held fd provides execution stability. The deployment's filesystem immutability assumptions determine how much security meaning a lexical prefix policy carries.

## Non-Guarantees

The resolver does not guarantee:

1. That the path string in `input.cwd` will continue to name the same directory object after resolution.
2. That parent path components cannot be renamed or replaced after resolution, unless the deployment separately guarantees those parent directories are attacker-immutable.
3. That lexical path prefixes are security boundaries by themselves. Prefix policies are only as strong as the deployment's immutability guarantees for the corresponding namespace.
4. That a bind-mounted directory, mount alias, or other platform-specific alias has only one possible trusted lexical label.
5. That `input.cwd` is a globally unique or durable canonical filesystem path.
6. That a `/check` result can be reused for later execution without a fresh resolution and authorization.
7. That command execution is filesystem-sandboxed. The cwd guarantee only controls the initial cwd used for process spawn.

## Execution Guarantee

For execution, `mcp-run` mitigates the final-cwd TOCTOU issue by holding the opened cwd fd and using that same fd for spawn.

On Unix, execution changes cwd in the child process with `fchdir(fd)` from a minimal `pre_exec` hook. Execution must not call `Command::current_dir` with the raw request cwd after authorization.

If the raw path is renamed or replaced after authorization, the spawned process still starts in the opened directory object that Rego evaluated.

## Check Freshness

A standalone `/check`, MCP `check_command`, or `run-remote --check` result is advisory and point-in-time.

A successful check means:

> The command would pass policy for the reported `cwd` at the time of the check.

It does not grant a reusable authorization token. A later execution must perform fresh cwd resolution, executable resolution, hashing, and policy evaluation.

## Policy Guidance

Policies may authorize directly against `input.cwd`, but should treat it as a trusted lexical label, not as a durable object identity.

For exact path checks:

```rego
allow if {
    input.command == "git"
    input.args == ["status"]
    input.cwd == "/home/user/workspace"
}
```

For subtree checks, include both the root and descendants:

```rego
allow if {
    input.command == "git"
    input.args == ["status"]
    input.cwd == "/home/user/workspace"
}

allow if {
    input.command == "git"
    input.args == ["status"]
    startswith(input.cwd, "/home/user/workspace/")
}
```

A prefix policy such as:

```rego
startswith(input.cwd, "/home/user/workspace/")
```

is only as strong as the deployment's guarantee that the namespace for `/home/user/workspace` and the relevant parent directories cannot be manipulated by the attacker.

## Summary

`input.cwd` is safe to use as a Rego policy input because it is produced by trusted Rust resolver code and tied to an opened directory fd.

It is not safe to over-interpret `input.cwd` as a durable canonical path or unique filesystem identity.

The intended guarantee is:

> `input.cwd` is the trusted lexical cwd label that `mcp-run` resolved at authorization time, and the command executes with the same opened cwd fd that produced that label.
