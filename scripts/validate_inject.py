#!/usr/bin/env python3
"""Exercise cladding inject through the active local or Machine Podman connection."""

from __future__ import annotations

import collections
import json
import os
import secrets
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path


AGENT_CLIENT = r"""
import json
import os
import socket
import sys
import time

port = int(sys.argv[1])
config = json.loads(sys.argv[2])

def receive_exact(connection, size):
    result = bytearray()
    while len(result) < size:
        chunk = connection.recv(size - len(result))
        if not chunk:
            raise RuntimeError("host-side target closed before returning a complete response")
        result.extend(chunk)
    return bytes(result)

deadline = time.monotonic() + 12
last_error = None
while True:
    try:
        connection = socket.create_connection(("127.0.0.1", port), timeout=1)
        break
    except OSError as error:
        last_error = error
        if time.monotonic() >= deadline:
            raise RuntimeError(f"agent listener did not become ready: {last_error}")
        time.sleep(0.1)

with connection:
    connection.settimeout(12)
    for index, encoded in enumerate(config["frames"]):
        payload = bytes.fromhex(encoded)
        connection.sendall(len(payload).to_bytes(4, "big") + payload)
        response_size = int.from_bytes(receive_exact(connection, 4), "big")
        response = receive_exact(connection, response_size)
        if response != payload:
            raise RuntimeError(f"byte mismatch in response frame {index}")
        if index + 1 < len(config["frames"]) and config.get("wait_file"):
            deadline = time.monotonic() + 20
            while not os.path.exists(config["wait_file"]):
                if time.monotonic() >= deadline:
                    raise RuntimeError("host did not release the active test streams")
                time.sleep(0.05)
        elif index + 1 < len(config["frames"]):
            time.sleep(config.get("delay", 0))
    if config.get("wait_eof", False):
        if connection.recv(1) != b"":
            raise RuntimeError("listener sent data after the final response")
"""


OCCUPY_PORT = r"""
import os
import socket
import sys
import time

listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen()
open(sys.argv[1], "w", encoding="ascii").write(str(listener.getsockname()[1]))
open(sys.argv[2], "w", encoding="ascii").write(str(os.getpid()))
while True:
    time.sleep(60)
"""


CHECK_PORT_CLOSED = r"""
import socket
import sys

try:
    socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.3)
except OSError:
    raise SystemExit(0)
raise SystemExit("agent listener is still accepting connections")
"""


CHECK_NO_BRIDGE = r"""
for comm in /proc/[0-9]*/comm; do
    [ -r "$comm" ] || continue
    IFS= read -r name < "$comm"
    if [ "$name" = cladding-bridge ]; then
        echo "bridge process remains: $comm"
        exit 1
    fi
done
"""


class ValidationFailure(RuntimeError):
    pass


def receive_exact(connection: socket.socket, size: int) -> bytes | None:
    result = bytearray()
    while len(result) < size:
        chunk = connection.recv(size - len(result))
        if not chunk:
            if not result:
                return None
            raise ConnectionError("peer closed a partial frame")
        result.extend(chunk)
    return bytes(result)


class EchoHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        self.request.settimeout(20)
        while True:
            header = receive_exact(self.request, 4)
            if header is None:
                return
            size = int.from_bytes(header, "big")
            payload = receive_exact(self.request, size)
            if payload is None:
                raise ConnectionError("peer closed before sending its payload")
            self.server.record(payload)
            self.request.sendall(header + payload)


class EchoServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    block_on_close = False

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), EchoHandler)
        self.condition = threading.Condition()
        self.requests: list[bytes] = []

    def record(self, payload: bytes) -> None:
        with self.condition:
            self.requests.append(payload)
            self.condition.notify_all()

    def count(self, payload: bytes) -> int:
        with self.condition:
            return collections.Counter(self.requests)[payload]

    def wait_for(self, expected: collections.Counter[bytes], timeout: float, phase: str) -> None:
        deadline = time.monotonic() + timeout
        with self.condition:
            while any(
                collections.Counter(self.requests)[payload] < count
                for payload, count in expected.items()
            ):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    observed = collections.Counter(self.requests)
                    raise ValidationFailure(
                        f"{phase}: host-side echo service did not receive expected bytes; "
                        f"expected={expected!r} observed={observed!r}"
                    )
                self.condition.wait(remaining)


class InjectValidator:
    def __init__(self, cladding: str, cladding_dir: str, agent: str) -> None:
        self.cladding = cladding
        self.cladding_dir = cladding_dir
        self.agent = agent
        self.phase = "initialize"
        self.inject: subprocess.Popen[str] | None = None
        self.inject_clients: list[subprocess.Popen[str]] = []
        self.occupier_pid: str | None = None

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

    def inspect_agent_mounts(self) -> None:
        raw = self.checked_podman("inspect", self.agent)
        mounts = json.loads(raw)[0].get("Mounts", [])
        stale = [
            mount
            for mount in mounts
            if mount.get("Destination") == "/run/cladding/agent/inject"
            or mount.get("Source", "").endswith("/runtime/sockets/agent/inject")
        ]
        if stale:
            raise ValidationFailure(
                "agent container still has the old host-backed inject socket mount: "
                f"{stale!r}"
            )

    def allocate_agent_port(self) -> int:
        code = (
            "import socket; s=socket.socket(); s.bind(('127.0.0.1', 0)); "
            "print(s.getsockname()[1]); s.close()"
        )
        output = self.checked_podman("exec", self.agent, "python3", "-c", code)
        return int(output)

    def agent_client(
        self,
        port: int,
        frames: list[bytes],
        *,
        delay: float = 0,
        wait_eof: bool = False,
        wait_file: str | None = None,
    ) -> subprocess.Popen[str]:
        config = json.dumps(
            {
                "frames": [frame.hex() for frame in frames],
                "delay": delay,
                "wait_eof": wait_eof,
                "wait_file": wait_file,
            }
        )
        process = subprocess.Popen(
            [
                "podman",
                "exec",
                self.agent,
                "python3",
                "-c",
                AGENT_CLIENT,
                str(port),
                config,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.inject_clients.append(process)
        return process

    def collect(
        self, process: subprocess.Popen[str], timeout: float, phase: str
    ) -> tuple[int, str, str]:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            process.kill()
            stdout, stderr = process.communicate(timeout=3)
            raise ValidationFailure(
                f"{phase}: process timed out after {timeout:g}s; "
                f"stdout={stdout!r} stderr={stderr!r}"
            ) from error
        return process.returncode or 0, stdout, stderr

    def successful_client(
        self,
        server: EchoServer,
        port: int,
        frames: list[bytes],
        *,
        delay: float = 0,
        wait_eof: bool = False,
        wait_file: str | None = None,
        phase: str,
    ) -> subprocess.Popen[str]:
        process = self.agent_client(
            port, frames, delay=delay, wait_eof=wait_eof, wait_file=wait_file
        )
        status, stdout, stderr = self.collect(process, 20, phase)
        if status != 0:
            raise ValidationFailure(
                f"{phase}: agent-side client failed (exit {status}); "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        server.wait_for(collections.Counter(frames), 5, phase)
        return process

    def start_inject(self, host_port: int, agent_port: int) -> subprocess.Popen[str]:
        if self.inject is not None:
            raise ValidationFailure("attempted to start a second inject command")
        self.inject = subprocess.Popen(
            [
                self.cladding,
                "--cladding-dir",
                self.cladding_dir,
                "inject",
                f"localhost:{host_port}",
                str(agent_port),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        return self.inject

    def collect_inject(self, timeout: float, phase: str) -> tuple[int, str, str]:
        if self.inject is None:
            raise ValidationFailure(f"{phase}: no inject command is running")
        process = self.inject
        self.inject = None
        return self.collect(process, timeout, phase)

    def signal_inject(self, sig: signal.Signals, phase: str) -> tuple[int, str, str]:
        if self.inject is None or self.inject.poll() is not None:
            raise ValidationFailure(f"{phase}: inject exited before receiving {sig.name}")
        self.inject.send_signal(sig)
        return self.collect_inject(10, phase)

    def assert_inject_running(self, phase: str) -> None:
        if self.inject is None or self.inject.poll() is not None:
            details = "no inject process"
            if self.inject is not None:
                _, stdout, stderr = self.collect_inject(1, phase)
                details = f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            raise ValidationFailure(f"{phase}: inject stopped unexpectedly; {details}")

    def assert_listener_closed(self, port: int, phase: str) -> None:
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            result = self.podman(
                "exec", self.agent, "python3", "-c", CHECK_PORT_CLOSED, str(port), timeout=5
            )
            if result.returncode == 0:
                return
            time.sleep(0.2)
        raise ValidationFailure(
            f"{phase}: agent-side listener 127.0.0.1:{port} still accepts connections; "
            f"stdout={result.stdout.strip()!r} stderr={result.stderr.strip()!r}"
        )

    def assert_no_bridge_process(self, phase: str) -> None:
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            result = self.podman(
                "exec", self.agent, "sh", "-c", CHECK_NO_BRIDGE, timeout=5
            )
            if result.returncode == 0:
                return
            time.sleep(0.2)
        raise ValidationFailure(
            f"{phase}: agent-side bridge process remains after inject exit; "
            f"stdout={result.stdout.strip()!r} stderr={result.stderr.strip()!r}"
        )

    def stop_occupier(self) -> None:
        if self.occupier_pid is None:
            return
        self.podman("exec", self.agent, "sh", "-c", f"kill {self.occupier_pid}")
        self.occupier_pid = None

    def cleanup(self) -> None:
        for client in self.inject_clients:
            if client.poll() is None:
                client.kill()
                try:
                    client.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
        self.stop_occupier()
        if self.inject is not None:
            if self.inject.poll() is None:
                self.inject.send_signal(signal.SIGTERM)
                try:
                    self.inject.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    self.inject.kill()
                    self.inject.communicate(timeout=3)
            self.inject = None

    def validate_occupied_listener(self, server: EchoServer) -> None:
        self.phase = "agent-side listener port already occupied"
        suffix = secrets.token_hex(4)
        port_file = f"/tmp/cladding-inject-{suffix}.port"
        pid_file = f"/tmp/cladding-inject-{suffix}.pid"
        self.checked_podman(
            "exec", "--detach", self.agent, "python3", "-c", OCCUPY_PORT, port_file, pid_file
        )
        deadline = time.monotonic() + 8
        port = None
        while time.monotonic() < deadline:
            port_result = self.podman("exec", self.agent, "cat", port_file)
            pid_result = self.podman("exec", self.agent, "cat", pid_file)
            if port_result.returncode == 0 and pid_result.returncode == 0:
                port = int(port_result.stdout.strip())
                self.occupier_pid = pid_result.stdout.strip()
                break
            time.sleep(0.1)
        if port is None:
            raise ValidationFailure(f"{self.phase}: agent-side listener did not start")

        self.start_inject(server.server_address[1], port)
        status, stdout, stderr = self.collect_inject(10, self.phase)
        output = f"{stdout}\n{stderr}".lower()
        if status == 0 or "listener failed to bind" not in output:
            raise ValidationFailure(
                f"{self.phase}: expected a listener-bind error, got exit={status}; "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.stop_occupier()
        self.assert_no_bridge_process(self.phase)

    def validate_echo_transport(self, server: EchoServer) -> None:
        self.phase = "physical-host localhost transport and byte correctness"
        port = self.allocate_agent_port()
        self.start_inject(server.server_address[1], port)

        probe = bytes([0, 1, 2, 10, 13, 127, 128, 255]) + b"host-loopback\x00probe"
        self.successful_client(server, port, [probe], phase=self.phase)

        self.phase = "multiple simultaneous agent connections"
        simultaneous = [
            bytes([index, 0, 255, 10]) + f"parallel-{index}".encode("ascii")
            for index in range(6)
        ]
        release_file = f"/tmp/cladding-inject-release-{secrets.token_hex(6)}"
        clients = [
            self.agent_client(port, [payload, payload[::-1]], wait_file=release_file)
            for payload in simultaneous
        ]
        server.wait_for(collections.Counter(simultaneous), 15, self.phase)
        if any(client.poll() is not None for client in clients):
            raise ValidationFailure(
                f"{self.phase}: an agent-side stream closed before all parallel streams were open"
            )
        self.checked_podman("exec", self.agent, "touch", release_file)
        expected = collections.Counter(
            payload for first in simultaneous for payload in (first, first[::-1])
        )
        deadline = time.monotonic() + 20
        for client in clients:
            remaining = max(0.1, deadline - time.monotonic())
            status, stdout, stderr = self.collect(client, remaining, self.phase)
            if status != 0:
                raise ValidationFailure(
                    f"{self.phase}: agent-side client failed (exit {status}); "
                    f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
                )
        server.wait_for(expected, 5, self.phase)

        self.phase = "long-lived connection remains usable"
        long_lived = [b"long-lived-first\x00\xff", b"long-lived-after-idle\r\n"]
        client = self.agent_client(port, long_lived, delay=2.5)
        server.wait_for(collections.Counter([long_lived[0]]), 5, self.phase)
        if client.poll() is not None:
            status, stdout, stderr = self.collect(client, 1, self.phase)
            raise ValidationFailure(
                f"{self.phase}: client closed during its idle interval; exit={status}; "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        status, stdout, stderr = self.collect(client, 10, self.phase)
        if status != 0:
            raise ValidationFailure(
                f"{self.phase}: agent-side client failed (exit {status}); "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        server.wait_for(collections.Counter(long_lived), 5, self.phase)

        self.phase = "closing one connection leaves another connection usable"
        survivor_frames = [b"survivor-open\x00", b"survivor-after-peer-close\xff"]
        release_file = f"/tmp/cladding-inject-survivor-{secrets.token_hex(6)}"
        survivor = self.agent_client(port, survivor_frames, wait_file=release_file)
        server.wait_for(collections.Counter([survivor_frames[0]]), 5, self.phase)
        closing_frames = [b"close-only-this-stream\r\n"]
        self.successful_client(server, port, closing_frames, phase=self.phase)
        if survivor.poll() is not None:
            status, stdout, stderr = self.collect(survivor, 1, self.phase)
            raise ValidationFailure(
                f"{self.phase}: surviving agent-side stream ended early (exit {status}); "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.checked_podman("exec", self.agent, "touch", release_file)
        status, stdout, stderr = self.collect(survivor, 10, self.phase)
        if status != 0:
            raise ValidationFailure(
                f"{self.phase}: surviving agent-side stream failed (exit {status}); "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        server.wait_for(collections.Counter(survivor_frames), 5, self.phase)

        self.phase = "stop foreground inject and remove the agent listener"
        status, stdout, stderr = self.signal_inject(signal.SIGTERM, self.phase)
        if status != 0:
            raise ValidationFailure(
                f"{self.phase}: inject did not stop cleanly after SIGTERM; "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.assert_listener_closed(port, self.phase)
        self.assert_no_bridge_process(self.phase)

    def validate_host_refusal(self, server: EchoServer) -> None:
        self.phase = "host destination refuses connections"
        with socket.socket() as unused:
            unused.bind(("127.0.0.1", 0))
            refused_port = unused.getsockname()[1]
        agent_port = self.allocate_agent_port()
        marker = b"host-refusal-must-not-reach-echo-service"
        before = server.count(marker)
        self.start_inject(refused_port, agent_port)
        failed_client = self.agent_client(agent_port, [marker])
        status, stdout, stderr = self.collect(failed_client, 18, self.phase)
        if status == 0 or "host-side target closed" not in stderr:
            raise ValidationFailure(
                f"{self.phase}: expected agent-side EOF after the host target refused; "
                f"exit={status} stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        if server.count(marker) != before:
            raise ValidationFailure(
                f"{self.phase}: refused request unexpectedly reached the physical-host echo service"
            )
        self.assert_inject_running(self.phase)
        status, stdout, stderr = self.signal_inject(signal.SIGTERM, self.phase)
        if status != 0:
            raise ValidationFailure(
                f"{self.phase}: inject did not stop cleanly after SIGTERM; "
                f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.assert_listener_closed(agent_port, self.phase)
        self.assert_no_bridge_process(self.phase)

    def active_stream(self, server: EchoServer, host_port: int, phase: str) -> tuple[int, subprocess.Popen[str]]:
        self.phase = phase
        port = self.allocate_agent_port()
        self.start_inject(host_port, port)
        payload = f"active-{secrets.token_hex(8)}".encode("ascii") + b"\x00\xff"
        client = self.agent_client(port, [payload], wait_eof=True)
        server.wait_for(collections.Counter([payload]), 12, phase)
        if client.poll() is not None:
            status, stdout, stderr = self.collect(client, 1, phase)
            raise ValidationFailure(
                f"{phase}: agent-side stream was not active before lifecycle action; "
                f"exit={status} stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        return port, client

    def validate_signals(self, server: EchoServer) -> None:
        for sig in (signal.SIGINT, signal.SIGTERM):
            phase = f"{sig.name} during an active stream"
            port, client = self.active_stream(server, server.server_address[1], phase)
            status, stdout, stderr = self.signal_inject(sig, phase)
            if status != 0:
                raise ValidationFailure(
                    f"{phase}: inject did not stop cleanly (exit {status}); "
                    f"stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
                )
            client_status, client_stdout, client_stderr = self.collect(client, 8, phase)
            if client_status != 0:
                raise ValidationFailure(
                    f"{phase}: agent-side stream did not observe tunnel shutdown; "
                    f"exit={client_status} stdout={client_stdout.strip()!r} "
                    f"stderr={client_stderr.strip()!r}"
                )
            self.assert_listener_closed(port, phase)
            self.assert_no_bridge_process(phase)

    def validate_agent_exit(
        self, server: EchoServer, host_port: int
    ) -> None:
        phase = "agent container exits during an active stream"
        port, client = self.active_stream(server, host_port, phase)
        self.checked_podman("stop", "--time", "1", self.agent, timeout=12)
        status, stdout, stderr = self.collect_inject(12, phase)
        if status == 0 or not any(
            marker in f"{stdout}\n{stderr}".lower()
            for marker in ("bridge", "transport", "podman exec")
        ):
            raise ValidationFailure(
                f"{phase}: expected an identified transport failure after agent exit; "
                f"exit={status} stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.collect(client, 8, phase)
        self.checked_podman("start", self.agent, timeout=12)
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            state = self.podman("inspect", "--format", "{{.State.Running}}", self.agent)
            if state.returncode == 0 and state.stdout.strip() == "true":
                break
            time.sleep(0.2)
        else:
            raise ValidationFailure(f"{phase}: agent container did not restart")
        self.assert_listener_closed(port, phase)
        self.assert_no_bridge_process(phase)

    def find_exec_child(self, phase: str) -> int:
        if self.inject is None:
            raise ValidationFailure(f"{phase}: inject command is not running")
        parent = self.inject.pid
        children_file = Path(f"/proc/{parent}/task/{parent}/children")
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            try:
                children = children_file.read_text(encoding="ascii").split()
            except FileNotFoundError:
                raise ValidationFailure(f"{phase}: inject exited before its exec transport")
            for child in children:
                try:
                    command = Path(f"/proc/{child}/cmdline").read_bytes()
                except FileNotFoundError:
                    continue
                if b"/opt/tools/bin/cladding-bridge" in command:
                    return int(child)
            if self.inject.poll() is not None:
                break
            time.sleep(0.1)
        raise ValidationFailure(f"{phase}: could not identify the podman exec transport child")

    def validate_exec_transport_close(self, server: EchoServer, host_port: int) -> None:
        phase = "podman exec mux transport closes unexpectedly"
        port, client = self.active_stream(server, host_port, phase)
        child = self.find_exec_child(phase)
        os.kill(child, signal.SIGKILL)
        status, stdout, stderr = self.collect_inject(12, phase)
        if status == 0 or not any(
            marker in f"{stdout}\n{stderr}".lower()
            for marker in ("bridge transport", "transport closed unexpectedly", "podman exec")
        ):
            raise ValidationFailure(
                f"{phase}: expected an identified transport failure after podman exec exited; "
                f"exit={status} stdout={stdout.strip()!r} stderr={stderr.strip()!r}"
            )
        self.collect(client, 8, phase)
        self.assert_listener_closed(port, phase)
        self.assert_no_bridge_process(phase)

    def run(self) -> None:
        self.phase = "check that inject needs no host socket bind"
        self.inspect_agent_mounts()
        server = EchoServer()
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        try:
            connection = os.environ.get("PODMAN_MACHINE_CONNECTION", "local Podman")
            print(
                f"Inject topology: {connection}; physical-host echo service on "
                f"127.0.0.1:{server.server_address[1]} (not mounted into the agent)"
            )
            self.validate_occupied_listener(server)

            # The successful localhost probe is the topology assertion: this
            # service binds only on the host running Cladding, outside the VM.
            self.validate_echo_transport(server)
            self.validate_host_refusal(server)
            self.validate_signals(server)
            self.validate_agent_exit(server, server.server_address[1])
            self.validate_exec_transport_close(server, server.server_address[1])
            print("Inject integration checks passed: transport, concurrency, and lifecycle")
        finally:
            self.cleanup()
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=3)


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cladding-bin", required=True)
    parser.add_argument("--cladding-dir", required=True)
    parser.add_argument("--agent", required=True)
    args = parser.parse_args()
    validator = InjectValidator(args.cladding_bin, args.cladding_dir, args.agent)
    try:
        validator.run()
        return 0
    except Exception as error:
        message = f"{validator.phase}: {error}"
        annotation = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"inject integration validation failed: {message}", file=sys.stderr)
        print(f"::error title=Inject integration phase::{annotation}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
