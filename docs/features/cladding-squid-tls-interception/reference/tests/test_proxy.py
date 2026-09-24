#!/usr/bin/env python3
"""Adversarial checks for the reference Squid listener and credential ACLs."""

import base64
import http.client
import json
import os
import socket
import ssl
import subprocess
import unittest


PROXY = "http://127.0.0.1:3128"
SANDBOX_PROXY = "http://127.0.0.1:3129"
ORIGIN_CA = "/test-certs/origin-ca.crt"
INTERCEPT_CA = "/test-certs/interception-ca.crt"
TRUST_BUNDLE = "/test-certs/client-trust-bundle.crt"
API_TOKEN = "TEST_API_TOKEN_123"
GIT_TOKEN = "TEST_GIT_TOKEN_456"
DELEGATED_API = f"Bearer {API_TOKEN}"
DELEGATED_GIT = "Basic " + base64.b64encode(f"x-access-token:{GIT_TOKEN}".encode()).decode()


def origin_request(path):
    connection = http.client.HTTPConnection("origin", 8080, timeout=5)
    connection.request("GET", path)
    response = connection.getresponse()
    result = response.read()
    connection.close()
    return json.loads(result)


def reset_origin():
    origin_request("/__clear")


def requests_seen():
    return origin_request("/__records")


def curl(url, proxy=PROXY, ca=TRUST_BUNDLE, extra=(), follow=False):
    command = [
        "curl",
        "--silent",
        "--show-error",
        "--proxy",
        proxy,
        "--noproxy",
        "",
        "--cacert",
        ca,
        "--max-time",
        "8",
    ]
    if follow:
        command.append("--location")
    command.extend(extra)
    command.append(url)
    return subprocess.run(command, capture_output=True, text=True)


def record_for(path):
    matches = [item for item in requests_seen() if item["path"].startswith(path)]
    if len(matches) != 1:
        raise AssertionError(f"expected one origin request for {path}, got {matches!r}")
    return matches[0]


class ProxyTests(unittest.TestCase):
    def setUp(self):
        reset_origin()

    def test_agent_api_replaces_all_client_authorization_fields(self):
        result = curl(
            "https://api.github.com/__api",
            extra=("--header", "Authorization: Bearer client-one", "--header", "Authorization: Bearer client-two"),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/__api")["authorization"], [DELEGATED_API])

    def test_agent_git_smart_http_replaces_client_authorization(self):
        result = curl(
            "https://github.com/owner/repo.git/info/refs?service=git-upload-pack",
            extra=("--header", "Authorization: Basic client-supplied"),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/owner/repo.git/info/refs")["authorization"], [DELEGATED_GIT])

    def test_agent_git_client_trust_works_with_real_git(self):
        env = dict(os.environ)
        env.update({"GIT_SSL_CAINFO": TRUST_BUNDLE, "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1"})
        result = subprocess.run(
            [
                "git",
                "-c",
                f"http.proxy={PROXY}",
                "-c",
                "protocol.version=0",
                "-c",
                "http.extraHeader=Authorization: Basic client-supplied",
                "ls-remote",
                "https://github.com/owner/repo.git",
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=12,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/owner/repo.git/info/refs")["authorization"], [DELEGATED_GIT])

    def test_network_sandbox_git_trust_works_without_injection(self):
        env = dict(os.environ)
        env.update({"GIT_SSL_CAINFO": TRUST_BUNDLE, "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1"})
        result = subprocess.run(
            [
                "git",
                "-c",
                f"http.proxy={SANDBOX_PROXY}",
                "-c",
                "protocol.version=0",
                "ls-remote",
                "https://github.com/owner/repo.git",
            ],
            env=env,
            capture_output=True,
            text=True,
            timeout=12,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/owner/repo.git/info/refs")["authorization"], [])

    def test_unrelated_allowed_https_is_spliced(self):
        result = curl("https://allowed.test/__splice", ca=ORIGIN_CA)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/__splice")["authorization"], [])

    def test_denied_domain_remains_denied(self):
        result = curl("https://not-allowlisted.invalid/", extra=("--fail",))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(requests_seen(), [])

    def test_network_sandbox_https_is_tunneled_without_injection(self):
        result = curl("https://api.github.com/__sandbox", proxy=SANDBOX_PROXY, ca=ORIGIN_CA)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/__sandbox")["authorization"], [])

    def test_plain_http_does_not_receive_a_credential(self):
        result = curl("http://api.github.com/__plain")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record_for("/__plain")["authorization"], [])

    def test_connect_and_sni_mismatch_cannot_redirect_delegated_credential(self):
        sock = socket.create_connection(("127.0.0.1", 3128), timeout=5)
        sock.sendall(b"CONNECT evil.test:443 HTTP/1.1\r\nHost: evil.test:443\r\n\r\n")
        response = b""
        while b"\r\n\r\n" not in response:
            response += sock.recv(1)
        self.assertIn(b"200", response.split(b"\r\n", 1)[0])

        context = ssl.create_default_context(cafile=TRUST_BUNDLE)
        tls = context.wrap_socket(sock, server_hostname="github.com")
        tls.sendall(
            b"GET /__connect_sni_mismatch HTTP/1.1\r\n"
            b"Host: github.com\r\nConnection: close\r\n\r\n"
        )
        while tls.recv(4096):
            pass
        tls.close()
        self.assertEqual(record_for("/__connect_sni_mismatch")["authorization"], [])

    def test_connect_sni_and_host_all_disagree(self):
        sock = socket.create_connection(("127.0.0.1", 3128), timeout=5)
        sock.sendall(b"CONNECT evil.test:443 HTTP/1.1\r\nHost: evil.test:443\r\n\r\n")
        response = b""
        while b"\r\n\r\n" not in response:
            response += sock.recv(1)
        self.assertIn(b"200", response.split(b"\r\n", 1)[0])

        context = ssl.create_default_context(cafile=TRUST_BUNDLE)
        tls = context.wrap_socket(sock, server_hostname="github.com")
        tls.sendall(
            b"GET /__all_names_disagree HTTP/1.1\r\n"
            b"Host: api.github.com\r\nConnection: close\r\n\r\n"
        )
        while tls.recv(4096):
            pass
        tls.close()
        self.assertEqual(record_for("/__all_names_disagree")["authorization"], [])

    def test_decrypted_host_mismatch_cannot_receive_delegated_credential(self):
        context = ssl.create_default_context(cafile=TRUST_BUNDLE)
        proxy = socket.create_connection(("127.0.0.1", 3128), timeout=5)
        proxy.sendall(b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n")
        response = b""
        while b"\r\n\r\n" not in response:
            response += proxy.recv(1)
        self.assertIn(b"200", response.split(b"\r\n", 1)[0])
        tls = context.wrap_socket(proxy, server_hostname="github.com")
        tls.sendall(
            b"GET /__host_mismatch HTTP/1.1\r\n"
            b"Host: evil.test\r\nConnection: close\r\n\r\n"
        )
        while tls.recv(4096):
            pass
        tls.close()
        self.assertEqual(record_for("/__host_mismatch")["authorization"], [])

    def test_redirect_to_other_allowed_host_does_not_receive_credential(self):
        result = curl("https://api.github.com/redirect", follow=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        records = requests_seen()
        api = [entry for entry in records if entry["host"].startswith("api.github.com")]
        redirected = [entry for entry in records if entry["host"].startswith("allowed.test")]
        self.assertEqual(len(api), 1)
        self.assertEqual(api[0]["authorization"], [DELEGATED_API])
        self.assertEqual(len(redirected), 1)
        self.assertEqual(redirected[0]["authorization"], [])

    def test_invalid_upstream_certificate_fails(self):
        origin_request("/__invalid?kind=untrusted")
        result = curl("https://api.github.com/__invalid_upstream", extra=("--fail",))
        origin_request("/__invalid?kind=none")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(requests_seen(), [])

    def test_upstream_hostname_mismatch_fails(self):
        origin_request("/__invalid?kind=hostname")
        result = curl("https://api.github.com/__wrong_hostname", extra=("--fail",))
        origin_request("/__invalid?kind=none")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(requests_seen(), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
