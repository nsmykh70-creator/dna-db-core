#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""End-to-end smoke test for v3.4 per-table ACL and trusted-proxy SSO.

Starts a real TLS server with a trusted loopback proxy, creates a restricted
viewer through the admin API, and verifies that SQL, DnaQL, panel browse and
raw KV all honour the allowlist. Then exercises proxy-header login, including
the shared-secret failure path.
"""

import json
import os
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
BIN = PROJECT / ("target/release/dna-db-core.exe" if os.name == "nt" else "target/release/dna-db-core")
PROXY_SECRET = "proxy-secret-0123456789"
ADMIN_PASS = "acl-admin-pass"
RESTRICTED_PASS = "alice-pass-123"

passed = 0
failed = 0


def check(name, condition, detail=""):
    global passed, failed
    if condition:
        passed += 1
        print("PASS:", name)
    else:
        failed += 1
        print("FAIL:", name, detail)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def tls_socket(port):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection(("127.0.0.1", port), timeout=10)
    sock = ctx.wrap_socket(raw, server_hostname="localhost")
    sock.settimeout(10)
    return sock


def recv_exact(sock, n):
    out = b""
    while len(out) < n:
        chunk = sock.recv(n - len(out))
        if not chunk:
            raise EOFError("connection closed")
        out += chunk
    return out


def recv_frame(sock):
    n = int.from_bytes(recv_exact(sock, 4), "little")
    return recv_exact(sock, n) if n else b""


def frame(payload):
    return len(payload).to_bytes(4, "little") + payload


def raw_login(port, user, password):
    sock = tls_socket(port)
    sock.sendall(frame(b"\x01" + (user + ":" + password).encode()))
    status = recv_exact(sock, 2)
    if status != b"OK":
        sock.close()
        return None
    return sock


def raw_query(sock, opcode, query):
    sock.sendall(frame(bytes([opcode]) + query.encode()))
    return recv_frame(sock).decode("utf-8", "replace")


def raw_put(sock, key, value):
    payload = bytes([0x03]) + len(key).to_bytes(2, "little") + key + value
    sock.sendall(frame(payload))
    first = recv_exact(sock, 2)
    if first == b"OK":
        return first
    # Errors are length-framed, unlike the raw OK used by successful PUTs.
    rest = recv_exact(sock, 2)
    n = int.from_bytes(first + rest, "little")
    return recv_exact(sock, n) if n else b""


def http(port, method, path, token=None, body=None, headers=None):
    sock = tls_socket(port)
    lines = ["%s %s HTTP/1.1" % (method, path), "Host: localhost"]
    if token:
        lines.append("Authorization: Bearer " + token)
    for key, value in (headers or {}).items():
        lines.append("%s: %s" % (key, value))
    payload = b""
    if body is not None:
        payload = json.dumps(body).encode()
        lines.append("Content-Type: application/json")
        lines.append("Content-Length: %d" % len(payload))
    sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode() + payload)
    out = b""
    while True:
        try:
            chunk = sock.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        out += chunk
    sock.close()
    head, _, body_bytes = out.partition(b"\r\n\r\n")
    parts = head.decode("latin-1").split("\r\n")[0].split()
    status = int(parts[1]) if len(parts) > 1 else 0
    try:
        parsed = json.loads(body_bytes.decode("utf-8"))
    except Exception:
        parsed = {"raw": body_bytes.decode("utf-8", "replace")}
    return status, parsed


def wait_ready(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            tls_socket(port).close()
            return True
        except Exception:
            time.sleep(0.15)
    return False


def main():
    data_dir = tempfile.mkdtemp(prefix="dna_acl_")
    port = free_port()
    admin_port = free_port()
    log = open(os.path.join(tempfile.gettempdir(), "dna_acl_proxy.log"), "wb")
    cmd = [
        str(BIN), "serve",
        "--data-dir", data_dir,
        "--port", str(port),
        "--admin-port", str(admin_port),
        "--bind", "127.0.0.1",
        "--admin-pass", ADMIN_PASS,
        "--trusted-proxy", "127.0.0.1",
        "--proxy-auth-secret", PROXY_SECRET,
        "--no-scrub",
    ]
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, cwd=str(PROJECT))
    try:
        if not wait_ready(port) or not wait_ready(admin_port):
            print("FAIL: server did not start")
            return 1

        admin = raw_login(port, "admin", ADMIN_PASS)
        check("admin raw login", admin is not None)
        if admin is None:
            return 1
        for ddl in (
            "CREATE TABLE chr_users (id INT, name TEXT, PRIMARY KEY (id))",
            "CREATE TABLE chr_orders (id INT, user_id INT, total FLOAT, PRIMARY KEY (id))",
        ):
            res = raw_query(admin, 0x11, ddl)
            check("admin DDL: " + ddl.split()[2], "error" not in res.lower(), res)
        raw_query(admin, 0x11, "INSERT INTO chr_users (id = 1, name = 'alice')")
        raw_query(admin, 0x11, "INSERT INTO chr_users (id = 2, name = 'bob')")
        raw_query(admin, 0x11, "INSERT INTO chr_orders (id = 10, user_id = 1, total = 9.5)")

        status, body = http(admin_port, "POST", "/api/login", body={"user": "admin", "pass": ADMIN_PASS})
        check("admin panel login", status == 200 and "token" in body, (status, body))
        admin_token = body.get("token", "")

        status, body = http(
            admin_port, "POST", "/api/users", token=admin_token,
            body={
                "user": "alice",
                "pass": RESTRICTED_PASS,
                "role": "viewer",
                "must_change": False,
                "allowed_tables": ["chr_users"],
            },
        )
        check("create restricted user", status == 201, (status, body))

        status, body = http(
            admin_port, "POST", "/api/login",
            body={"user": "alice", "pass": RESTRICTED_PASS},
        )
        check("restricted panel login", status == 200 and "token" in body, (status, body))
        alice_token = body.get("token", "")

        status, body = http(admin_port, "GET", "/api/tables", token=alice_token)
        names = [item["name"] for item in body.get("tables", [])]
        check("panel lists only allowed table", status == 200 and names == ["chr_users"], (status, body))

        status, body = http(admin_port, "GET", "/api/browse?table=chr_orders", token=alice_token)
        check("panel browse denied", status == 403, (status, body))

        status, body = http(
            admin_port, "POST", "/api/query", token=alice_token,
            body={"kind": "sql", "query": "SELECT * FROM chr_orders"},
        )
        check("panel SQL denied", status == 403, (status, body))

        status, body = http(
            admin_port, "POST", "/api/query", token=alice_token,
            body={"kind": "sql", "query": "SELECT * FROM chr_users"},
        )
        check("panel SQL allowed", status == 200, (status, body))

        alice = raw_login(port, "alice", RESTRICTED_PASS)
        check("restricted raw login", alice is not None)
        if alice is not None:
            allowed = raw_query(alice, 0x11, "SELECT * FROM chr_users")
            check("TCP SQL allowed", "alice" in allowed and "error" not in allowed.lower(), allowed)
            denied = raw_query(alice, 0x11, "SELECT * FROM chr_orders")
            check("TCP SQL denied", "access denied" in denied.lower(), denied)
            denied_join = raw_query(
                alice, 0x11,
                "SELECT * FROM chr_users u JOIN chr_orders o ON u.id = o.user_id",
            )
            check("TCP SQL join denied", "access denied" in denied_join.lower(), denied_join)
            denied_sub = raw_query(
                alice, 0x11,
                "SELECT * FROM chr_users WHERE id IN (SELECT user_id FROM chr_orders)",
            )
            check("TCP SQL subquery denied", "access denied" in denied_sub.lower(), denied_sub)
            check("TCP raw PUT denied", raw_put(alice, b"chr_orders:10", b"tamper") != b"OK")
            check(
                "TCP DnaQL denied",
                "access denied" in raw_query(alice, 0x10, "FROM chr_orders |> WHERE total > 0").lower(),
            )

        status, body = http(
            admin_port, "POST", "/api/login/proxy",
            headers={"X-Auth-Request-User": "alice", "X-Proxy-Auth-Secret": PROXY_SECRET},
        )
        check("trusted proxy login", status == 200 and body.get("user") == "alice", (status, body))
        proxy_token = body.get("token", "")
        status, body = http(admin_port, "GET", "/api/me", token=proxy_token)
        check(
            "proxy session carries ACL",
            status == 200 and body.get("allowed_tables") == ["chr_users"],
            (status, body),
        )

        status, body = http(
            admin_port, "POST", "/api/login/proxy",
            headers={"X-Auth-Request-User": "alice", "X-Proxy-Auth-Secret": "wrong-secret"},
        )
        check("proxy bad secret rejected", status == 403, (status, body))

        status, body = http(
            admin_port, "POST", "/api/login/proxy",
            headers={"X-Auth-Request-User": "ghost", "X-Proxy-Auth-Secret": PROXY_SECRET},
        )
        check("proxy unknown user rejected", status == 401, (status, body))

        status, body = http(
            admin_port, "POST", "/api/users/access", token=admin_token,
            body={"user": "alice", "allowed_tables": ["chr_users", "chr_orders"]},
        )
        check("admin updates ACL", status == 200, (status, body))
        status, body = http(admin_port, "GET", "/api/me", token=alice_token)
        check("old session revoked after ACL update", status == 401, (status, body))

        status, body = http(
            admin_port, "POST", "/api/login",
            body={"user": "alice", "pass": RESTRICTED_PASS},
        )
        alice_token = body.get("token", "")
        status, body = http(
            admin_port, "POST", "/api/query", token=alice_token,
            body={"kind": "sql", "query": "SELECT * FROM chr_orders"},
        )
        check("new ACL grants second table", status == 200, (status, body))

        print("\n%d passed, %d failed" % (passed, failed))
        return 0 if failed == 0 else 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        log.close()


if __name__ == "__main__":
    sys.exit(main())
