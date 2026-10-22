# Fake credentials for `cladding run`

Cladding can give a run fake credential values in environment variables. Baffle
replaces a matching fake with the real secret only in the Baffle session that
created it and only for an authorized session rule.

## Configure the mapping

Add a `fake_credential_env` object to `.cladding/cladding.json`. Each key is an
environment variable name. Each value is the exact Baffle credential
identifier, including its case.

```json
{
  "name": "demo",
  "agent": { "image": "localhost/my-agent:latest" },
  "fake_credential_env": {
    "GITHUB_TOKEN": "github-api",
    "GH_TOKEN": "github-api",
    "OPENAI_API_KEY": "openai-api"
  }
}
```

The Baffle secret must be available under
`.cladding/credentials/baffle/secrets/<credential-id>` or supplied for that run
with `--secret`. The selected agent session file must reference the same
credential in a rule and set its effective credential mode to `substitute`.
For example:

```toml
[credentials.overrides.github-api]
mode = "substitute"

[rules."api.github.com"]
mode = "intercept"
ports = [443]
paths = ["/repos/**"]

[[rules."api.github.com".inject]]
header = "Authorization"
secret = "github-api"
format = "bearer"
```

Baffle still enforces the daemon's allowed-secret list, destination, port, and
path rules. A mapping does not grant access by itself.

## Select a real credential for one run

The configured mapping always selects the Baffle credential identifier. Use
`--secret` to select a different real secret file or host environment variable
for that identifier during one run:

```sh
# Use the project's read-only github-api secret.
cladding run -- codex

# Use a read-write value for github-api during this run.
cladding run \
  --secret github-api=env:GITHUB_WRITE_TOKEN \
  -- codex
```

Both commands give `codex` the fake value configured for `github-api`. The
second command changes the real secret that Baffle uses for authorized
requests. It does not change the mapping or the Baffle session policy.

## Environment precedence

Cladding applies values in this order:

1. A value supplied with `cladding run --env`.
2. A mapping in `fake_credential_env` in `cladding.json`.
3. The environment already present in the agent container.

When `--env` names a mapped variable, Cladding does not derive or inject a fake
for that variable. `--env` keeps its existing behavior. An explicitly supplied
value is available to the user command and is outside Baffle's secret
substitution.

## Session and upgrade requirements

Cladding embeds the published Baffle v2.2.0 Linux binary and uses the published
`baffle-client` v2.2.0 crate for session policy validation and credential
identifier checks. Baffle v2.2.0 returns session creation details as JSON,
including the session seed. For `cladding run`, Cladding creates the agent
session and reads this response in its trusted host process. Run
`cladding build` after upgrading Cladding to refresh
`.cladding/tools/bin/baffle`; run `cladding check` to confirm the embedded tool
is current. Existing version 2 Baffle session files remain in the required
format. Set the mapped credential's effective Baffle mode to `substitute`; the
default remains `inject`.

Baffle creates a new random seed for each session. Cladding derives the fake
values from the session seed in memory. It does not write the seed or derived
values to routine logs. It does not pass the seed to the agent, network
sandbox, or filesystem sandbox. The proxy keeps real secrets in Baffle's
credential directory or in the proxy-only secret mount for a run. The user
command receives only the derived fake values for configured mappings.

Baffle derives each fake from the session seed with its versioned,
domain-separated HMAC contract. Cladding passes the exact configured Baffle
credential identifier; the environment-variable name does not affect the
derived value.

Variables mapped to the same credential receive the same fake value within a
session. Different sessions receive different values. A fake works only
through its matching Baffle session. Cladding maps environment variables only;
it does not discover variables, write fake credentials to files, or substitute
arbitrary text.
