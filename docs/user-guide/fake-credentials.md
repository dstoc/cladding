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

## Override a mapping for one run

Use `--fake-credential-env NAME=CREDENTIAL` to add or replace one mapping.
Repeat the option to set more than one mapping:

```sh
cladding run \
  --secret github-api=env:REAL_GITHUB_TOKEN \
  --fake-credential-env GITHUB_TOKEN=github-api \
  --fake-credential-env GH_TOKEN=github-api \
  -- codex
```

Use `NAME=none` to suppress the configured mapping for that variable:

```sh
cladding run --fake-credential-env GITHUB_TOKEN=none -- codex
```

Suppression removes only the fake mapping. It does not remove a value supplied
with `--env` or inherited by the agent container.

## Environment precedence

Cladding applies values in this order:

1. A value supplied with `cladding run --env`.
2. A per-run `--fake-credential-env` mapping or `none` suppression.
3. A mapping in `fake_credential_env` in `cladding.json`.
4. The environment already present in the agent container.

When `--env` names a mapped variable, Cladding does not derive or inject a fake
for that variable. `--env` keeps its existing behavior. If you set it to a real
secret, that value is available to the user command and is outside Baffle's
secret isolation.

## Session and upgrade requirements

Cladding embeds the published Baffle v2.1.0 Linux binary and uses the published
`baffle-client` v2.1.0 API. Run `cladding build` after upgrading Cladding to
refresh `.cladding/tools/bin/baffle`; run `cladding check` to confirm the
embedded tool is current. Existing version 2 Baffle session files remain in
the required format. Set the mapped credential's effective Baffle mode to
`substitute`; the default remains `inject`.

Baffle creates a new random seed for each session. Cladding keeps that seed in
proxy-private state, derives fake values after it creates the agent session,
and removes the seed after derivation. The seed is not passed to the agent or
written to routine logs. Real secrets stay in Baffle's credential directory or
in the proxy-only secret mount for a run. The mapped values in the user command
environment are fakes; values passed explicitly with `--env` keep their normal
behavior.

Variables mapped to the same credential receive the same fake value within a
session. Different sessions receive different values. A fake works only
through its matching Baffle session. Cladding maps environment variables only;
it does not discover variables, write fake credentials to files, or substitute
arbitrary text.
