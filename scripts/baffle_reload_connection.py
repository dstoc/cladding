#!/usr/bin/env python3
"""Hold one intercepted TLS connection open across a Baffle policy reload."""

import time
from pathlib import Path

import requests


READY = Path("/tmp/baffle-reload-ready")
CONTINUE = Path("/tmp/baffle-reload-continue")
DONE = Path("/tmp/baffle-reload-done")
FAILED = Path("/tmp/baffle-reload-failed")
LOG = Path("/tmp/baffle-reload-client.log")
BASE_URL = "https://localhost:8443"


def log(message):
    with LOG.open("a", encoding="utf-8") as output:
        output.write(message + "\n")


def request(session, path, expected_credential):
    response = session.get(BASE_URL + path, timeout=20)
    response.raise_for_status()
    result = response.json()
    response.close()
    if result.get("authorization") != expected_credential:
        raise AssertionError(
            f"{path} returned authorization category {result.get('authorization')!r}; "
            f"expected {expected_credential!r}"
        )


def main():
    LOG.unlink(missing_ok=True)
    existing = requests.Session()
    request(existing, "/authorized/before-reload", "old")
    READY.touch()

    deadline = time.monotonic() + 60
    while not CONTINUE.exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("timed out waiting for the test driver")
        time.sleep(0.05)

    # This session must reuse the CONNECT/TLS connection accepted before reload.
    request(existing, "/authorized/after-reload", "old")

    replacement = requests.Session()
    request(replacement, "/replacement/new-policy", "new")

    rejected = requests.Session()
    response = rejected.get(BASE_URL + "/authorized/rejected-after-reload", timeout=20)
    if response.ok:
        response.close()
        raise AssertionError("a new connection used the replaced path policy")
    response.close()

    DONE.touch()
    log("existing and new connections kept their expected policy and credential snapshots")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # Keep diagnostics free of request headers and token values.
        log(f"{type(error).__name__}: {error}")
        FAILED.touch()
        raise
