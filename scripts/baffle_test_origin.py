#!/usr/bin/env python3
"""Small TLS and HTTP origin for the rootless Baffle integration suite."""

import http.server
import json
import os
import socket
import ssl
import threading
from urllib.parse import urlsplit


EVENTS = "/tmp/baffle-integration/events.jsonl"
OLD_TOKEN = "cladding-test-old-value"
NEW_TOKEN = "cladding-test-new-value"
event_lock = threading.Lock()


def record_request(handler):
    authorization = handler.headers.get("Authorization", "")
    if authorization == f"Bearer {OLD_TOKEN}":
        auth_kind = "old"
    elif authorization == f"Bearer {NEW_TOKEN}":
        auth_kind = "new"
    elif authorization:
        auth_kind = "client"
    else:
        auth_kind = "none"

    event = {
        "path": urlsplit(handler.path).path,
        "port": handler.server.server_port,
        "authorization": auth_kind,
    }
    with event_lock, open(EVENTS, "a", encoding="utf-8") as output:
        output.write(json.dumps(event, sort_keys=True) + "\n")
    return auth_kind


class OriginHandler(http.server.SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory="/fixtures/www", **kwargs)

    def log_message(self, _format, *_args):
        # Keep request headers and credentials out of container logs.
        return

    def do_GET(self):
        auth_kind = record_request(self)
        path = urlsplit(self.path).path
        if path.startswith("/authorized/repo.git/"):
            return super().do_GET()

        body = json.dumps({"path": path, "authorization": auth_kind}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_HEAD(self):
        record_request(self)
        return super().do_HEAD()


class DualStackThreadingHTTPServer(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        super().server_bind()


def serve(port, certificate=None, private_key=None):
    server = DualStackThreadingHTTPServer(("::", port), OriginHandler)
    if certificate is not None:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(certificate, private_key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()


def main():
    os.makedirs(os.path.dirname(EVENTS), exist_ok=True)
    certificate = "/fixtures/server.crt"
    private_key = "/fixtures/server.key"
    serve(8080)
    serve(8443, certificate, private_key)
    serve(9443, certificate, private_key)
    threading.Event().wait()


if __name__ == "__main__":
    main()
