---
name: uv-pip-offline-download
description: How to use uv, pip and Python to download and install dependencies in this environment. If there's an error installing python dependencies, you need this!
---

# UV/Pip Offline Download Workflow

Use these commands to create an offline cache and install from it.

## Download packages into cache

For requirements file input:

```bash
run-with-network -- uv run --python /usr/bin/python3 pip download -r requirements.txt -d /home/user/pip-cache
```

For explicit package specs:

```bash
run-with-network -- uv run --python /usr/bin/python3 pip download pandas==2.0.3 -d /home/user/pip-cache
```

For multiple packages, place all specs before `-d`:

```bash
run-with-network -- uv run --python /usr/bin/python3 pip download pandas==2.0.3 numpy==1.26.4 -d /home/user/pip-cache
```

## Install only from local cache

Use `--no-index` to block index access and `--find-links` to point pip to the local cache:

```bash
uv pip install --no-index --find-links /home/user/pip-cache -r requirements.txt
```

Or install direct specs from cache:

```bash
uv pip install --no-index --find-links /home/user/pip-cache pandas==2.0.3
```

For project dependency workflows, pass offline index flags directly:

```bash
uv add --no-index --find-links /home/user/pip-cache pandas==2.0.3
```

```bash
uv sync --no-index --find-links /home/user/pip-cache
```

## Verify cache contents

List downloaded artifacts:

```bash
ls -1 /home/user/pip-cache
```

Optionally test network-isolated install by repeating install commands with `--no-index --find-links` only.

## Guardrails

- Pin the interpreter with `--python /usr/bin/python3` for `uv run pip download` to match command policy exactly.
- Use `-d /home/user/pip-cache` for downloads; policy requires this cache location.
- Use `run-with-network --` for download commands that access package indexes.
- Use `uv run pip download` for downloading (download is not available via `uv pip`).
- Keep offline installs local with `--no-index --find-links` for `uv pip install`, `uv add`, and `uv sync`, and run them without the network wrapper.
- Prefer pinned versions for reproducible offline installs.
- Avoid URL/path requirements when the goal is a portable offline cache.
