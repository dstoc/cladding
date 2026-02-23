# Rego Pattern Examples (mcp-run)

## Router Contract

- Query: `data.sandbox.main.allow`
- Router expects `package sandbox.main` with `allow` boolean.
- Typical router delegates to `data.sandbox[input.command].allow` and enforces env constraints.

## Example Policies

### Exact Args (curl)

Allow only a single exact invocation:

```rego
package sandbox.curl

default allow = false

default allow_env = false

# Allow: curl -I https://example.com
allow if {
    input.args == ["-I", "https://example.com"]
    startswith(input.path, "/usr/bin/")
}
```

### Fixed Subcommand (python)

Allow one exact module invocation:

```rego
package sandbox.python

default allow = false

default allow_env = false

# Allow: python -m http.server
allow if {
    input.args == ["-m", "http.server"]
}
```

### No-Args (date)

Allow only the bare command:

```rego
package sandbox.date

default allow = false

default allow_env = false

allow if {
    count(input.args) == 0
}
```

### Must Contain Flag Pair (build)

Require a flag + value segment:

```rego
package sandbox.build

default allow = false

default allow_env = false

# Allow only if args contain: --target x86
allow if {
    some i
    i + 1 < count(input.args)
    input.args[i] == "--target"
    input.args[i + 1] == "x86"
}
```

### Ban Specific Arg (echo)

Deny a known unsafe flag:

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

### Allowlist (exact count)

Require exactly 3 args from an allowlist:

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

### Allowlist (up to N)

Allow up to 3 args from an allowlist:

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

### Segmented Args (mix singles and pairs)

Allow mixed segments like `--fast --target x86`:

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
