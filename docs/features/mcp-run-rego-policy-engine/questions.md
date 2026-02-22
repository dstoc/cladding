# Questions: mcp-run Rego Policy Engine

## Q1 (resolved)
- Question: For policy matching and authorization, should the primary key be `input.command` (user-requested token like `curl`) or `input.path` (resolved absolute path like `/usr/bin/curl`)?
- Why it matters: This decision changes security posture and portability. Path-based matching is stricter against PATH shadowing, while command-token matching is simpler and closer to current behavior/documentation.
- Options:
  1. `input.command` as primary key, with optional path checks inside policies.
  2. `input.path` as primary key, with optional command checks.
  3. Require both to match (strict mode).
- Answer: `input.command` as primary key.
- Decision/Impact: Router/command policy lookup will be centered on command token; path will still be provided in input so policies can add stricter path constraints when desired.

## Q2 (resolved)
- Question: Should forwarded environment variables be part of the Rego policy decision input in v1?
- Why it matters: This determines whether current env allowlist behavior can be represented in policy or must remain hardcoded outside Rego.
- Options:
  1. Include `input.env` so policies can allow/deny by env keys/values.
  2. Keep env validation outside Rego and pass only `command/path/args`.
- Answer: Include `input.env` in Rego input.
- Decision/Impact: Rego becomes the single policy source for env authorization logic, enabling parity with current env-gating and future richer constraints.

## Q3 (resolved)
- Question: If a policy file change is detected but the updated Rego set fails to compile/evaluate, what should runtime do?
- Why it matters: This is a key safety/availability tradeoff for hot reload behavior.
- Options:
  1. Keep last-known-good policy and reject reload.
  2. Fail closed immediately (deny all commands until fixed).
  3. Crash/restart server.
- Answer: Fail closed immediately.
- Decision/Impact: On reload failure, effective policy state becomes deny-all until a valid policy set is successfully loaded.

## Q4 (resolved)
- Question: What should `POLICY_FILE` point to after this change?
- Why it matters: It sets config contract, loader behavior, and watcher scope.
- Options:
  1. Policy directory with multiple `.rego` files.
  2. Single entrypoint file.
  3. Support both.
- Answer: A folder (for example, `config/sandbox_commands`) containing `.rego` files.
- Decision/Impact: Superseded by Q7. Final contract uses `POLICY_DIR` for Rego directory policies, while `POLICY_FILE` remains for legacy JSON during transition.

## Q5 (resolved)
- Question: During rollout, should `mcp-run` still accept legacy JSON policies (`sandbox_commands.json`) as a fallback, or should Rego directory policy be the only supported format?
- Why it matters: This affects migration risk, implementation complexity, and deprecation timeline.
- Options:
  1. Rego-only hard cutover.
  2. Temporary support for both JSON and Rego.
  3. Rego-only by default with JSON compatibility flag.
- Answer: Support both temporarily; JSON will be removed later.
- Decision/Impact: PRD includes explicit transitional dual-engine support with a deprecation/removal follow-up.

## Q6 (resolved)
- Question: On server startup, if policy loading is invalid (e.g., bad Rego and no valid JSON fallback), should `mcp-run` fail to start, or start in deny-all mode?
- Why it matters: This determines outage profile, operator visibility, and recovery flow.
- Options:
  1. Fail startup immediately.
  2. Start in deny-all mode and keep watching for fixes.
- Answer: Start in deny-all mode and keep watching for fixes.
- Decision/Impact: Startup can proceed with a fail-closed engine state, preserving service availability while preventing command execution until policy becomes valid.

## Q7 (resolved)
- Question: Should Rego and JSON use separate config variables, specifically `POLICY_DIR` for Rego and `POLICY_FILE` for legacy JSON?
- Why it matters: Separate variables remove ambiguity, simplify detection logic, and clarify migration behavior.
- Answer: Yes. Use `POLICY_DIR` for Rego, and keep `POLICY_FILE` for JSON during transition.
- Decision/Impact: Config contract is explicit by format; implementation defines deterministic precedence when both are present.
