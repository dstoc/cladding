#!/usr/bin/env python3
"""Verify cladding expose forwards to the selected running agent."""

from __future__ import annotations

import os
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


AGENT_ECHO_SERVER = r"""
import os
import socket
import sys
from pathlib import Path

port_file = Path(sys.argv[1])
pid_file = Path(sys.argv[2])
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen()
port_file.write_text(str(listener.getsockname()[1]), encoding="ascii")
pid_file.write_text(str(os.getpid()), encoding="ascii")
while True:
    connection, _ = listener.accept()
    with connection:
        while True:
            payload = connection.recv(4096)
            if not payload:
                break
            connection.sendall(payload)
"""


class ValidationFailure(RuntimeError):
    pass


class ExposeValidator:
    def __init__(self, cladding: str, cladding_dir: str, agent: str) -> None:
        self.cladding = cladding
        self.cladding_dir = Path(cladding_dir)
        self.agent = agent
        self.phase = "initialize"
        self.expose: subprocess.Popen[str] | None = None
        self.marker = f"/tmp/cladding-expose-{secrets.token_hex(8)}"

    def podman(self, *args: str, timeout: float = 12) -> subprocess.CompletedProcess[str]:
        try:
            return subprocess.run(
                ["podman", *args],
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise ValidationFailure(
                f"{self.phase}: podman {' '.join(args)} timed out after {timeout:g}s; "
                f"stdout={error.stdout!r} stderr={error.stderr!r}"
            ) from error

    def checked_podman(self, *args: str, timeout: float = 12) -> str:
        result = self.podman(*args, timeout=timeout)
        if result.returncode != 0:
            raise ValidationFailure(
                f"{self.phase}: podman {' '.join(args)} exited {result.returncode}; "
                f"stdout={result.stdout.strip()!r} stderr={result.stderr.strip()!r}"
            )
        return result.stdout.strip()

    def start_agent_echo_server(self) -> int:
        self.phase = "start loopback echo service inside the existing agent"
        port_file = f"{self.marker}.port"
        pid_file = f"{self.marker}.pid"
        self.checked_podman(
            "exec",
            "--detach",
            self.agent,
            "python3",
            "-c",
            AGENT_ECHO_SERVER,
            port_file,
            pid_file,
        )
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            result = self.podman("exec", self.agent, "test", "-f", port_file, timeout=4)
            if result.returncode == 0:
                port = int(self.checked_podman("exec", self.agent, "cat", port_file))
                return port
            time.sleep(0.1)
        raise ValidationFailure("agent echo service did not publish its loopback port")

    def project_resources(self) -> tuple[str, str]:
        root = str(self.cladding_dir)
        containers = self.checked_podman(
            "ps", "--all", "--filter", f"label=project_root={root}", "--format", "{{.Names}}"
        )
        volumes = self.checked_podman(
            "volume",
            "ls",
            "--filter",
            f"label=project_root={root}",
            "--format",
            "{{.Name}}",
        )
        return "\n".join(sorted(containers.splitlines())), "\n".join(sorted(volumes.splitlines()))

    def wait_for_host_listener(self, host_port: int) -> None:
        deadline = time.monotonic() + 8
        last_error: OSError | None = None
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", host_port), timeout=0.5):
                    return
            except OSError as error:
                last_error = error
                if self.expose is not None and self.expose.poll() is not None:
                    break
                time.sleep(0.1)
        stdout, stderr = self.finish_expose(timeout=1)
        raise ValidationFailure(
            f"{self.phase}: host listener did not become ready; "
            f"last_error={last_error!r} stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
        )

    def start_expose(
        self, agent_port: int, host_port: int, config_path: Path, working_directory: Path
    ) -> None:
        self.phase = "start expose with explicit Cladding directory and config"
        self.expose = subprocess.Popen(
            [
                self.cladding,
                "--cladding-dir",
                str(self.cladding_dir),
                "--config",
                str(config_path),
                "expose",
                str(agent_port),
                str(host_port),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
            cwd=working_directory,
        )

    def finish_expose(self, timeout: float = 5) -> tuple[str, str]:
        if self.expose is None:
            return "", ""
        process = self.expose
        self.expose = None
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate(timeout=3)
            raise ValidationFailure(
                f"{self.phase}: expose did not stop after SIGKILL; "
                f"stdout={stdout!r} stderr={stderr!r}"
            ) from error
        return stdout, stderr

    def assert_expose_running(self) -> None:
        if self.expose is None or self.expose.poll() is not None:
            details = "no expose process"
            if self.expose is not None:
                stdout, stderr = self.finish_expose(timeout=1)
                details = f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            raise ValidationFailure(f"{self.phase}: expose exited unexpectedly; {details}")

    def validate_forwarding_and_selection(self) -> None:
        config_path = self.cladding_dir / "expose-selection.json"
        default_config = self.cladding_dir / "cladding.json"
        original_config = default_config.read_bytes()
        config_path.write_bytes(original_config)
        config_path.chmod(0o600)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            host_port = reservation.getsockname()[1]

        # The selected config copy remains valid and names the running agent.
        # If expose ignores --config, the invalid default config makes it fail.
        default_config.write_text("not a valid Cladding config\n", encoding="ascii")
        try:
            agent_port = self.start_agent_echo_server()
            baseline_resources = self.project_resources()
            with tempfile.TemporaryDirectory(
                prefix="cladding-expose-decoy-", dir=self.cladding_dir.parent
            ) as decoy_workspace:
                decoy_cladding_dir = Path(decoy_workspace) / ".cladding"
                decoy_cladding_dir.mkdir()
                (decoy_cladding_dir / "cladding.json").write_text(
                    "not a valid Cladding config\n", encoding="ascii"
                )
                self.start_expose(
                    agent_port, host_port, config_path, Path(decoy_workspace)
                )
                self.wait_for_host_listener(host_port)

                self.phase = "forward a host connection to the selected running agent"
                payload = b"cladding expose reaches the existing agent\x00\xff"
                with socket.create_connection(("127.0.0.1", host_port), timeout=8) as client:
                    client.settimeout(8)
                    client.sendall(payload)
                    client.shutdown(socket.SHUT_WR)
                    response = bytearray()
                    while True:
                        chunk = client.recv(4096)
                        if not chunk:
                            break
                        response.extend(chunk)

                if bytes(response) != payload:
                    raise ValidationFailure(
                        f"host received incorrect agent response: {bytes(response)!r}"
                    )
                self.assert_expose_running()
                if self.project_resources() != baseline_resources:
                    raise ValidationFailure(
                        "expose changed project containers or volumes while forwarding; "
                        "it must not create a one-off runtime"
                    )

                print(
                    "Expose integration passed: explicit project and config selection "
                    "reached the active agent, forwarded bytes, and created no runtime resources"
                )
        finally:
            try:
                self.finish_expose()
            finally:
                default_config.write_bytes(original_config)
                config_path.unlink(missing_ok=True)
                self.stop_agent_echo_server()

    def stop_agent_echo_server(self) -> None:
        pid_file = f"{self.marker}.pid"
        try:
            pid = self.checked_podman("exec", self.agent, "cat", pid_file, timeout=4)
        except Exception:
            return
        code = "import os, signal, sys; os.kill(int(sys.argv[1]), signal.SIGTERM)"
        self.podman("exec", self.agent, "python3", "-c", code, pid, timeout=4)
        self.podman("exec", self.agent, "rm", "-f", f"{self.marker}.port", pid_file, timeout=4)

    def run(self) -> None:
        try:
            self.validate_forwarding_and_selection()
        finally:
            try:
                self.finish_expose()
            finally:
                self.stop_agent_echo_server()


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cladding-bin", required=True)
    parser.add_argument("--cladding-dir", required=True)
    parser.add_argument("--agent", required=True)
    args = parser.parse_args()
    validator = ExposeValidator(args.cladding_bin, args.cladding_dir, args.agent)
    try:
        validator.run()
        return 0
    except Exception as error:
        message = f"{validator.phase}: {error}"
        annotation = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"expose integration validation failed: {message}", file=sys.stderr)
        print(f"::error title=Expose integration phase::{annotation}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
