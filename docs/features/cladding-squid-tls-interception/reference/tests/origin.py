#!/usr/bin/env python3
"""Controlled HTTPS/HTTP origin for the local Squid reference tests."""

import http.server
import json
import os
import ssl
import threading
import urllib.parse


records = []
records_lock = threading.Lock()
certificate_fault = "none"


class OriginHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format, *_args):
        return

    def send_body(self, status, body, content_type="application/json"):
        encoded = body if isinstance(body, bytes) else body.encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self):
        global certificate_fault
        parsed = urllib.parse.urlsplit(self.path)
        if self.server.server_port == 8080:
            if parsed.path == "/__records":
                with records_lock:
                    body = json.dumps(records)
                self.send_body(200, body)
                return
            if parsed.path == "/__clear":
                with records_lock:
                    records.clear()
                self.send_body(200, "{}")
                return
            if parsed.path == "/__invalid":
                certificate_fault = urllib.parse.parse_qs(parsed.query).get("kind", ["none"])[0]
                self.send_body(200, json.dumps({"fault": certificate_fault}))
                return

        entry = {
            "method": "GET",
            "path": self.path,
            "host": self.headers.get("Host", ""),
            "authorization": self.headers.get_all("Authorization", []),
            "port": self.server.server_port,
        }
        with records_lock:
            records.append(entry)

        if parsed.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "https://allowed.test/redirected")
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            return

        if parsed.path.endswith("/info/refs"):
            announcement = b"# service=git-upload-pack\n"
            packet = f"{len(announcement) + 4:04x}".encode() + announcement + b"0000"
            self.send_body(200, packet, "application/x-git-upload-pack-advertisement")
            return

        self.send_body(200, json.dumps(entry))


def make_server(port):
    server = http.server.ThreadingHTTPServer(("0.0.0.0", port), OriginHandler)
    server.daemon_threads = True
    return server


http_server = make_server(80)
control_server = make_server(8080)
tls_server = make_server(443)

good_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
good_context.load_cert_chain("/certs/origin.crt", "/certs/origin.key")
invalid_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
invalid_context.load_cert_chain("/certs/invalid.crt", "/certs/invalid.key")
wrong_host_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
wrong_host_context.load_cert_chain("/certs/wrong-host.crt", "/certs/wrong-host.key")


def choose_certificate(ssl_socket, _server_name, initial_context):
    if certificate_fault == "untrusted":
        ssl_socket.context = invalid_context
    elif certificate_fault == "hostname":
        ssl_socket.context = wrong_host_context
    else:
        ssl_socket.context = initial_context


good_context.set_servername_callback(choose_certificate)
tls_server.socket = good_context.wrap_socket(tls_server.socket, server_side=True)

for server in (http_server, control_server, tls_server):
    threading.Thread(target=server.serve_forever, daemon=True).start()

threading.Event().wait()
