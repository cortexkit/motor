#!/usr/bin/env python3
"""Outside TLS echo probe. Stdin is public IP/port/certificate JSON, never an API key."""
from __future__ import annotations

import ipaddress
import json
import socket
import ssl
import sys
import time
import uuid


def tcp_probe(address, port, certificate, timeout=4):
    start = time.monotonic()
    result = {"address": address, "port": port, "tls": True}
    try:
        ipaddress.ip_address(address)
        context = ssl.create_default_context(cadata=certificate)
        context.check_hostname = False
        challenge = (uuid.uuid4().hex + "\n").encode()
        with socket.create_connection((address, port), timeout=timeout) as raw:
            result["tcp_seconds"] = time.monotonic() - start
            with context.wrap_socket(raw, server_hostname="boat-probe") as stream:
                result["tls_seconds"] = time.monotonic() - start
                stream.sendall(challenge)
                received = b""
                expected = b"boat-probe:" + challenge
                while len(received) < len(expected):
                    chunk = stream.recv(256)
                    if not chunk:
                        break
                    received += chunk
                result["verified_echo"] = received == expected
                result["tls_version"] = stream.version()
    except (OSError, ValueError) as error:
        result["error"] = str(error)
    result["seconds"] = time.monotonic() - start
    return result


def main():
    request = json.load(sys.stdin)
    result = tcp_probe(request["address"], request["port"], request["certificate_pem"])
    print(json.dumps(result))
    return 0 if result.get("verified_echo") else 1


if __name__ == "__main__":
    raise SystemExit(main())
