---
name: npm-dependencies
description: Manage npm package dependencies in this environment. Use when adding or updating npm packages, editing package.json/package-lock.json, or when network access must be constrained to `run-with-network -- npm install --no-scripts [--save | --save-dev] -- package-spec ...` followed by `npm rebuild` outside the sandbox.
---

# Npm Dependencies

## Overview

Add or update npm dependencies using the approved network-safe workflow, then run install scripts locally via `npm rebuild`.

## Workflow

1. Decide dependency type and package specs.
2. Run the only approved network command:

```bash
run-with-network -- npm install --no-scripts -- <package-spec> ...
```

To install from `package.json` (no explicit package specs), run:

```bash
run-with-network -- npm install --no-scripts
```

3. For dependencies or devDependencies, include exactly one of `--save` or `--save-dev` before the `--` delimiter:

```bash
run-with-network -- npm install --no-scripts --save -- <package-spec> ...
run-with-network -- npm install --no-scripts --save-dev -- <package-spec> ...
```

4. After the networked install completes, run install scripts locally outside the sandbox:

```bash
npm rebuild
```

5. Continue with normal local builds/tests (no network required).

## Guardrails

- Always pass `--no-scripts` and the `--` delimiter.
- Package specs must appear only after `--`.
- If no package specs are provided, `npm install --no-scripts` is allowed without `--`.
- Do not use other npm network commands unless the user explicitly approves a policy change.
