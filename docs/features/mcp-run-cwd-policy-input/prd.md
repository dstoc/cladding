# PRD: `mcp-run` CWD-Aware Policy Input

## Objective
Allow `mcp-run` Rego policies to make authorization decisions based on the effective working directory where a command would run.

The effective cwd must be resolved by trusted Rust code during the shared pre-spawn authorization path. Rego receives `input.cwd` as a trusted resolved lexical cwd label, never as the raw user-provided cwd string. Unresolved or nonexistent cwd values must fail closed before policy evaluation or process spawn.

This proposal assumes the shared policy-check authorization path exists, so execution, `/check`, MCP `check_command`, and `run-remote --check` all use the same cwd decision semantics.

## Use Cases
1. A policy allows `git status` only inside a workspace tree.
2. A policy allows build tools only from project-owned directories and denies invocations from `/tmp` or other writable scratch paths.
3. A user runs `run-remote --check -- <cmd>` and sees whether the command would be allowed in the forwarded cwd before executing it.
4. Tests verify directory-sensitive policy without relying on command side effects.

## Functional Requirements

### 1. Scope and Placement
1. Implementation is inside `crates/mcp-run`.
2. The existing command request schema remains unchanged:
- `executable`
- `args`
- `cwd`
- `env`
3. The Rego policy input object gains a new field:
- `cwd: string`
4. The check decision object should also report the effective cwd policy label:
- `cwd: string | null`
5. Existing callers that do not set `cwd` continue using the server default cwd.
6. Internally, authorization should normalize this immediately: if request `cwd` is absent, treat it as if `default_cwd` had been provided.
7. Rust is responsible for resolving a trusted cwd label and holding the final cwd fd.
8. Rego is responsible for deciding whether the trusted cwd label is allowed.
9. `mcp-run` validates `default_cwd` is absolute at startup. It still resolves `default_cwd` fresh per request when request `cwd` is omitted.
10. The first implementation is Unix-only. Non-Unix platforms must fail closed for this feature path rather than silently falling back to path-based cwd execution.

### 2. Effective CWD Model
1. Authorization first computes one effective cwd input:
- if `RunCommandInput.cwd` is set, use it;
- otherwise use `default_cwd`.
2. All later cwd handling uses this effective cwd input; no later branch should distinguish whether it came from the request or from the default.
3. The resolver walks the effective cwd input with directory fds and produces a trusted resolved lexical cwd label.
4. The raw request cwd is never passed to Rego and is never authoritative for authorization.
5. `mcp-run` does not maintain a separate allowed-cwd list in Rust.
6. Rego decides whether the trusted `input.cwd` label is allowed.
7. If cwd resolution fails, authorization fails with a validation error.
8. `/check` and MCP `check_command` represent cwd resolution failure as `allowed: false` with a user-actionable `reason`.
9. `/raw`, MCP `run_command`, and default `run-remote -- ...` preserve fail-closed execution behavior by rejecting before spawn.

### 3. Resolution Algorithm
1. At request time, compute the effective cwd input from request/default cwd.
2. The effective cwd input must be absolute.
3. If request `cwd` is relative, reject with `CwdResolutionFailed`.
4. If request `cwd` is omitted, `default_cwd` is used and must already be absolute.
5. Open `/` once as the resolver root fd.
6. Walk the absolute candidate from the root fd using directory fds.
7. Maintain parallel stacks:
- opened directory fds;
- policy-label components.
8. For each component:
- skip empty components produced by repeated `/`;
- reject `.` and `..` components with `CwdResolutionFailed`;
- open the next component relative to the current directory fd with no symlink following for that component;
- if the component is a directory, advance to that opened directory fd and push the component onto the policy-label stack;
- if the component is a symlink, magic link, or any non-directory, reject with `CwdResolutionFailed`.
9. Repeated slashes are allowed and normalize away because empty components are skipped.
10. Trailing slashes are allowed and normalize away.
11. Leading `//` is rejected with `CwdResolutionFailed`.
12. If the effective cwd is `/`, `AuthorizedCwd` uses a duplicated resolver root fd and `cwd_label` is `/`.
13. Otherwise, the final component must resolve to an opened directory fd.
14. The trusted cwd label is `"/" + policy-label components joined by "/"`, or `/` for the root directory.
15. Construct `AuthorizedCwd` containing:
- the opened final cwd fd;
- the trusted cwd label;
- the raw/effective input path for logs and error messages only.
16. Keep `AuthorizedCwd` alive through Rego validation and process spawn.
17. Pass only the trusted cwd label to Rego as `input.cwd`.
18. On Unix, execution changes cwd with `fchdir` on the authorized fd in `pre_exec`.
19. The first implementation should use Linux `openat2` for component opening.
20. Component opens must reject symlinks and magic-link-style resolution using `RESOLVE_NO_SYMLINKS`.
21. The implementation must not call `canonicalize` and then later rely on the canonicalized string for enforcement; the resolver must produce the label while retaining the final fd it resolved.
22. Resolver failure classification for error messages may be best-effort. Authorization must fail closed based on the fd-opening result, not on a later diagnostic lookup.
23. This resolver is security-critical and must be tested as such.

### 4. Effective CWD Guarantees
When cwd resolution succeeds, `mcp-run` can make these statements about `input.cwd` and the authorized cwd:

1. `input.cwd` is produced by trusted host code, not copied from user input.
2. `input.cwd` is absolute and normalized:
- it starts with `/`;
- it has no empty components;
- it has no `.` components;
- it has no `..` components.
3. `input.cwd` is the trusted resolved lexical label produced by the resolver without following symlinks or magic links.
4. At resolution time, every component in the final `input.cwd` label was opened as a real directory, not as a symlink or magic link.
5. The final cwd used for execution is the same opened directory fd that produced `input.cwd`.
6. If a symlink or magic link appears in the input path, the request is rejected before Rego.
7. The final component is an opened directory fd held through Rego validation and process spawn.
8. If any component is `.`, `..`, or cannot be opened as a directory without following symlinks, the request is rejected before Rego.

The resolver does not guarantee:

1. That the path string in `input.cwd` will continue to name the same directory object after resolution.
2. That parent path components cannot be renamed or replaced after resolution.
3. That a bind-mounted directory has only one possible trusted lexical label.
4. That a `/check` result can be reused for later execution without a fresh resolution and authorization.
5. That `input.cwd` is a globally unique or durable canonical filesystem path.

For execution, the held fd mitigates the final-cwd TOCTOU issue: the process runs in the opened directory object even if the path is changed after authorization.

### 5. Rego Input Contract
1. Rego input includes:
- `input.command`
- `input.path`
- `input.hash`
- `input.args`
- `input.env`
- `input.cwd`
2. `input.cwd` is the trusted resolved lexical cwd label, not the raw request value.
3. Policies may treat `input.cwd` as the cwd label produced by `mcp-run`'s resolver at authorization time.
4. Policies must not assume that this string remains a live pathname after authorization.
5. Policy evaluation should only run after cwd resolution, executable resolution, and executable hashing succeed.
6. Existing policies that ignore `input.cwd` continue to work.
7. Rego policy may authorize directly against `input.cwd`.
8. Rego remains the only layer that decides whether a trusted cwd label is allowed.
9. A prefix policy over `input.cwd` authorizes the walked lexical label. It does not prove unique object identity in the presence of mount aliases or platform-specific path aliasing.

Example:

```rego
package sandbox.git

default allow = false
default allow_env = false

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

### 6. Check Response Contract
1. Extend `RunCommandCheckOutput` with:
- `cwd: string | null`
2. `cwd` is populated once cwd resolution succeeds.
3. `cwd` is `null` when cwd resolution fails before an effective cwd can be established.
4. `resolvedPath` and `hash` keep their existing semantics.
5. `allowed: true` means the command would pass policy for the reported cwd at the time of the check.
6. `allowed: false` for cwd failure means execution would be rejected before process spawn.
7. The check response must not expose the raw request cwd as a separate field in the decision object.
8. The denial `reason` may include the raw/effective cwd input when needed for user-actionable diagnostics.

### 7. Execution Enforcement
1. After authorization succeeds, execution must use the same opened cwd fd that was mapped to the trusted cwd label.
2. Execution must not call `Command::current_dir` with the raw request cwd after authorization.
3. On Unix, use a minimal `pre_exec` hook to call `fchdir(fd)` in the child before `exec`.
4. The opened cwd fd must stay alive until process spawn completes.
5. The first implementation is Unix-only. If the feature is built or run on a non-Unix platform, trusted-cwd execution must fail closed rather than silently falling back to path re-resolution.
6. If non-Unix support is required later, check-only behavior may be supported only if it cannot be confused with executable authorization; execution must not silently degrade.
7. The implementation must keep the `pre_exec` closure minimal and async-signal-safe.

Suggested internal type:

```rust
struct AuthorizedCwd {
    fd: OwnedFd,
    effective_input: PathBuf,
    cwd_label: String,
}
```

### 8. Symlink and Path Semantics
1. The first implementation uses trusted resolved lexical cwd labels produced by the Rust resolver.
2. Symlink and magic-link components are rejected rather than followed.
3. For example, `/tmp/link-to-repo` is rejected even if it points at `/work/repo`.
4. This intentionally avoids implementing realpath-like symlink expansion in the first version.
5. The initial implementation allows crossing mount points. Mount points are not symlinks or magic links.
6. `input.cwd` is a walked lexical label, not an object-identity uniqueness guarantee.
7. Bind mounts can present the same directory object under multiple trusted lexical labels; this proposal treats the walked lexical label as the policy value.
8. A future resolver mode may reject mount crossing, for example with Linux `openat2` `RESOLVE_NO_XDEV`.
9. The first implementation should use Linux `openat2`: validate absolute input, reject `.`, `..`, and leading `//`, then open each component relative to the current directory fd with flags such as `O_PATH`, `O_DIRECTORY`, `O_CLOEXEC`, and `RESOLVE_NO_SYMLINKS`.
10. No implementation may degrade to a raw `canonicalize`-then-spawn path flow.

### 9. Error Handling
1. Add a distinct validation error for cwd resolution failure, for example:
- `CwdResolutionFailed { cwd, details }`
2. Error text should be user-actionable, for example:
- `Failed to resolve cwd '/missing': path does not exist`
- `Failed to resolve cwd '/tmp/file': path is not a directory`
3. Cwd resolution failure is a normal policy-check denial outcome:
- HTTP `/check`: `200 OK` with `allowed: false`
- MCP `check_command`: structured successful output with `allowed: false`
- `run-remote --check`: JSON decision to stdout and exit code `1`
4. Malformed request JSON remains a protocol error and keeps existing `400 Bad Request` behavior.
5. Diagnostic classification may require a secondary metadata lookup. That lookup must not affect authorization; it is only for the error message.

### 10. TOCTOU and Freshness Semantics
1. Cwd checks are point-in-time, just like executable hash and policy checks.
2. The implementation must mitigate path-based TOCTOU for execution by authorizing an opened directory fd and using that same fd for spawn.
3. The implementation must not authorize one path and then re-resolve the raw path at spawn time.
4. A standalone `/check` remains advisory: the client does not receive a reusable fd or authorization grant, and a later execution performs a fresh authorization.
5. If a path component is renamed after resolution, execution still uses the opened final cwd fd.
6. Complete filesystem race elimination is not a goal of Rego policy input; stronger isolation still belongs in container mount policy and OS-level sandboxing.

### 11. Observability
1. Log trusted cwd policy label for check and execution authorization decisions.
2. Do not log forwarded env values.
3. Distinguish cwd resolution failure from Rego policy denial in logs and error messages.

### 12. Documentation Requirements
1. Move `docs/features/mcp-run-cwd-policy-input/cwd-policy-guarantees.md` into the `mcp-run` crate as part of implementation.
2. Suggested destination:
- `crates/mcp-run/docs/cwd-policy-guarantees.md`
3. The moved document should describe the crate's implemented behavior, not just proposal intent.
4. Update `crates/mcp-run/README.md` to link to the moved guarantees document.
5. Delete the proposal-local copy after moving it; the crate-local document should become the authoritative user/developer documentation.

### 13. Testing Requirements
1. Unit test that Rego receives trusted `input.cwd` and can allow based on it.
2. Unit test that omitted request cwd is normalized to `default_cwd` before trusted cwd resolution.
3. Unit test that raw cwd is not passed to Rego.
4. Unit test that nonexistent cwd produces `allowed: false` in `check_command`.
5. Unit test that cwd pointing to a file produces `allowed: false`.
6. Regression test that `spawn_command_process` uses the same authorized cwd fd that policy evaluated.
7. HTTP `/check` test that denied cwd resolution returns `200 OK` and `allowed: false`.
8. MCP `check_command` test that `cwd` is present in the structured decision.
9. `run-remote --check` test that forwarded caller cwd is reflected in the decision JSON when the server accepts it.
10. Unix test that symlink components are rejected.
11. Unix test that magic-link components are rejected where the platform exposes them.
12. Unix test that paths containing `.` or `..` are rejected.
13. Unit test that cwd `/` resolves to label `/`.
14. Unit test for repeated slashes according to the chosen contract, for example `/foo//bar` normalizes to `/foo/bar`.
15. Unit test that leading `//foo` is rejected.
16. Unit test that trailing slash normalizes away, for example `/foo/bar/` becomes `/foo/bar`.
17. Unit test that relative cwd is rejected even if it would point inside `default_cwd`.
18. Unit test that invalid relative `default_cwd` is rejected as a server configuration error.
19. Test ordinary mount-point traversal behavior if practical; otherwise document that it is covered by the explicit mount semantics.
20. Unix regression test that replacing the raw path after authorization does not change the directory object used by execution.

## Non-Goals
1. No change to the command request schema.
2. No cwd globbing or workspace-root abstraction in `mcp-run`.
3. No reusable authorization token from `/check`.
4. No complete elimination of all filesystem TOCTOU races.
5. No container mount or filesystem isolation changes.
6. No policy language change beyond adding trusted `input.cwd`.
7. No separate Rust-maintained allowed-cwd list.
8. No support for relative request cwd values in the first implementation.

## Suggested Implementation Shape
1. In `crates/mcp-run/src/executor.rs`, add effective cwd resolution to `authorize_command_request`.
2. Store `AuthorizedCwd` in `CommandAuthorization` so `spawn_command_process` uses the same fd that policy evaluated.
3. Add `cwd: Option<String>` to `RunCommandCheckOutput`.
4. Add helpers such as:
- `effective_cwd_input(default_cwd: &Path, requested_cwd: Option<&str>) -> PathBuf`
- `resolve_authorized_cwd(effective_cwd: &Path) -> Result<AuthorizedCwd, String>`
5. Change `check_command` to use `default_cwd` instead of ignoring it.
6. In `crates/mcp-run/src/policy.rs`, add trusted cwd input to `PolicyEvaluationInput`, `validate_invocation`, and the Rego input JSON.
7. On Unix, use `std::os::unix::process::CommandExt::pre_exec` or the Tokio equivalent to call `libc::fchdir` before exec.
8. Update policy tests that call `validate_invocation` directly to pass cwd.
9. Move `cwd-policy-guarantees.md` into `crates/mcp-run/docs/`, delete the proposal-local copy, and update `crates/mcp-run/README.md` to link to it.
10. Update `crates/mcp-run/README.md` to document trusted `input.cwd`, check output `cwd`, and TOCTOU/freshness semantics.

## Verification
1. `cargo fmt --all --check` succeeds.
2. `cargo test -p mcp-run` succeeds.
3. `cargo test --workspace` succeeds.
4. Manual `/check` request with an allowed cwd returns `allowed: true` and includes trusted `cwd`:

```bash
curl -sS -X POST http://127.0.0.1:8000/check \
  -H 'content-type: application/json' \
  --data '{"executable":"git","args":["status"],"cwd":"/home/user/workspace/project"}'
```

5. Manual `/check` request with missing cwd returns `200 OK` and `allowed: false`.
6. Manual `run-remote --check -- <cmd>` prints a decision whose `cwd` is the trusted server-side policy label.

## Success Criteria
1. Rego policies can allow or deny commands based on trusted `input.cwd`.
2. Execution and check paths use the same effective cwd resolution.
3. Missing or invalid cwd values fail closed before process spawn.
4. Check responses report the cwd used for the decision.
5. Existing policies that ignore cwd continue to work.
6. Execution uses the same opened cwd fd that was authorized.
7. Documentation clearly states cwd TOCTOU mitigations and remaining limitations.
