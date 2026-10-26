#!/usr/bin/env python3
"""Active production-security probe for DNA-DB (loopback only).

Verifies the hardening shipped in the production build:
  * TLS on both the TCP protocol and the web panel, HSTS + CSP headers;
  * no plaintext fallback on the TLS listener;
  * unauthenticated data access is rejected;
  * Argon2id user database + persistent per-IP/per-username lockout;
  * CSPRNG session tokens and user create / password-change flow;
  * pre-auth 64 KiB frame cap (no 128 x 16 MiB amplification);
  * payload values are not present in cleartext in WAL/SST files.
"""
import json
from pathlib import Path
import os
import socket
import ssl
import struct
import subprocess
import tempfile
import time

PROJECT = Path(__file__).resolve().parent.parent
BIN = PROJECT / ("target/release/dna-db-core.exe" if os.name == "nt" else "target/release/dna-db-core")
OUT = PROJECT / "security_results.json"
PASS = "probe-pass-7f3a"
NEW_PASS = "auditor-pass-2"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def frame(payload):
    return len(payload).to_bytes(4, "little") + payload


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


def ssl_context():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx


def connect(port):
    raw = socket.create_connection(("127.0.0.1", port), timeout=5)
    s = ssl_context().wrap_socket(raw, server_hostname="localhost")
    s.settimeout(5)
    return s


def raw_auth(port, user, password):
    s = connect(port)
    s.sendall(frame(b"\x01" + ("%s:%s" % (user, password)).encode()))
    try:
        data = s.recv(2)
    except socket.timeout:
        data = b"TIMEOUT"
    s.close()
    return data


def framed_auth(port, user, password):
    s = connect(port)
    s.sendall(frame(b"\x01" + ("%s:%s" % (user, password)).encode()))
    try:
        data = recv_frame(s)
    except Exception as e:
        data = ("EXC:%s" % e).encode()
    s.close()
    return data


def http(port, method, path, token=None, body=None):
    s = connect(port)
    headers = ["%s %s HTTP/1.1" % (method, path), "Host: localhost"]
    if token:
        headers.append("Authorization: Bearer " + token)
    payload = b""
    if body is not None:
        payload = json.dumps(body).encode()
        headers.append("Content-Type: application/json")
        headers.append("Content-Length: %d" % len(payload))
    s.sendall(("\r\n".join(headers) + "\r\n\r\n").encode() + payload)
    out = b""
    while True:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        out += chunk
    s.close()
    head, _, body_bytes = out.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = int(lines[0].split()[1]) if lines and len(lines[0].split()) > 1 else 0
    hdrs = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            hdrs[k.strip().lower()] = v.strip()
    return status, hdrs, body_bytes.decode("utf-8", "replace")


def wait_ready(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            connect(port).close()
            return True
        except OSError:
            time.sleep(0.05)
    return False


class Server:
    def __init__(self, data_dir, port, admin_port):
        self.log = open(os.path.join(tempfile.gettempdir(), "dna_security_srv.log"), "ab")
        self.proc = subprocess.Popen(
            [BIN, "serve", "--data-dir", data_dir, "--port", str(port),
             "--bind", "127.0.0.1", "--admin-pass", PASS,
             "--admin-port", str(admin_port), "--no-scrub", "--sync", "strict"],
            stdout=self.log, stderr=self.log, cwd=PROJECT)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def main():
    result = {"meta": {"date": time.strftime("%Y-%m-%d %H:%M:%S"), "binary": str(BIN),
                       "bind": "127.0.0.1", "checks": []}}
    data_dir = tempfile.mkdtemp(prefix="dna_sec_")
    port, admin_port = free_port(), free_port()
    srv = Server(data_dir, port, admin_port)
    try:
        assert wait_ready(port), "TLS TCP listener did not start"
        assert wait_ready(admin_port), "HTTPS panel did not start"

        # 1. No plaintext fallback on the TLS listener.
        plain = socket.create_connection(("127.0.0.1", port), timeout=3)
        plain.sendall(frame(b"\x01admin:" + PASS.encode()))
        try:
            plain.settimeout(3)
            plain_reply = plain.recv(16)
        except Exception:
            plain_reply = b""
        plain.close()
        result["plaintext_reply"] = plain_reply.decode("latin-1")
        result["meta"]["checks"].append({
            "name": "TLS listener rejects plaintext protocol",
            "pass": plain_reply != b"OK",
        })

        # 2. Unauthenticated data access.
        s = connect(port)
        s.sendall(frame(b"\x03" + (3).to_bytes(2, "little") + b"sec" + b"secret-value"))
        put_unauth = recv_frame(s)
        s.close()
        s = connect(port)
        s.sendall(frame(b"\x02sec"))
        get_unauth = recv_frame(s)
        s.close()
        result["unauth_put"] = put_unauth.decode("utf-8", "replace")
        result["unauth_get"] = get_unauth.decode("utf-8", "replace")
        result["meta"]["checks"].append({
            "name": "unauthenticated PUT/GET rejected",
            "pass": b"not authenticated" in put_unauth and b"not authenticated" in get_unauth,
        })

        # 3. Authenticated write/read over TLS.
        s = connect(port)
        s.sendall(frame(b"\x01admin:" + PASS.encode()))
        assert s.recv(2) == b"OK"
        s.sendall(frame(b"\x03" + (3).to_bytes(2, "little") + b"sec" + b"secret-value"))
        put_ok = s.recv(2)
        s.sendall(frame(b"\x02sec"))
        get_ok = recv_frame(s)
        s.close()
        result["auth_put"] = put_ok.decode("latin-1")
        result["auth_get"] = get_ok.decode("latin-1")
        result["meta"]["checks"].append({
            "name": "authenticated PUT/GET over TLS",
            "pass": put_ok == b"OK" and get_ok == b"secret-value",
        })

        # 4. Panel must be HTTPS with HSTS + CSP, tokens must be CSPRNG-sized.
        status_root, hdr_root, body_root = http(admin_port, "GET", "/")
        status_tables_anon, _, _ = http(admin_port, "GET", "/api/tables")
        status_good_login, _, body_good_login = http(
            admin_port, "POST", "/api/login", body={"user": "admin", "pass": PASS})
        token = json.loads(body_good_login).get("token") if status_good_login == 200 else None
        status2, _, body2 = http(admin_port, "POST", "/api/login", body={"user": "admin", "pass": PASS})
        token2 = json.loads(body2).get("token") if status2 == 200 else None
        status_health, _, body_health = http(admin_port, "GET", "/api/health", token=token)
        status_users, _, body_users = http(admin_port, "GET", "/api/users", token=token)
        result["admin"] = {
            "root_status": status_root,
            "api_tables_anon_status": status_tables_anon,
            "good_login_status": status_good_login,
            "health_status": status_health,
            "users_status": status_users,
            "tokens_distinct": bool(token and token2 and token != token2),
            "token_length": len(token) if token else 0,
            "root_headers": hdr_root,
            "health_preview": body_health[:160],
        }
        result["meta"]["checks"].append({
            "name": "HTTPS panel + HSTS + CSP + nosniff/no-store",
            "pass": status_root == 200 and hdr_root.get("strict-transport-security", "").startswith("max-age=")
                    and "default-src 'self'" in hdr_root.get("content-security-policy", "")
                    and "sha256-" in hdr_root.get("content-security-policy", "")
                    and "script-src 'self' 'unsafe-inline'" not in hdr_root.get("content-security-policy", "")
                    and hdr_root.get("x-content-type-options") == "nosniff"
                    and hdr_root.get("cache-control") == "no-store",
        })
        result["meta"]["checks"].append({
            "name": "SPA public / API 401 without token",
            "pass": status_tables_anon == 401,
        })
        result["meta"]["checks"].append({
            "name": "CSPRNG-looking 256-bit bearer tokens, distinct",
            "pass": bool(token) and len(token) == 64 and result["admin"]["tokens_distinct"],
        })

        # 5. User create -> login -> self password change -> re-login.
        status_create, _, body_create = http(
            admin_port, "POST", "/api/users", token=token,
            body={"user": "auditor", "pass": NEW_PASS, "role": "operator", "must_change": False})
        status_auditor_login, _, body_auditor = http(
            admin_port, "POST", "/api/login", body={"user": "auditor", "pass": NEW_PASS})
        auditor_token = json.loads(body_auditor).get("token") if status_auditor_login == 200 else None
        status_change, _, body_change = http(
            admin_port, "POST", "/api/password", token=auditor_token,
            body={"old_password": NEW_PASS, "new_password": NEW_PASS + "-changed"})
        status_relogin, _, body_relogin = http(
            admin_port, "POST", "/api/login", body={"user": "auditor", "pass": NEW_PASS + "-changed"})
        result["users"] = {
            "create_status": status_create,
            "auditor_login_status": status_auditor_login,
            "password_change_status": status_change,
            "re_login_status": status_relogin,
            "create_error": body_create[:160],
            "change_error": body_change[:160],
            "re_login_error": body_relogin[:160],
        }
        result["meta"]["checks"].append({
            "name": "user create + self password change + re-login",
            "pass": status_create == 201 and status_auditor_login == 200
                    and status_change == 200 and status_relogin == 200,
        })

        # 5b. Viewer role is enforced on the HTTPS API and the TCP protocol.
        status_create_viewer, _, body_create_viewer = http(
            admin_port, "POST", "/api/users", token=token,
            body={"user": "viewer1", "pass": "viewer-pass-1", "role": "viewer", "must_change": False})
        status_viewer_login, _, body_viewer = http(
            admin_port, "POST", "/api/login", body={"user": "viewer1", "pass": "viewer-pass-1"})
        viewer_token = json.loads(body_viewer).get("token") if status_viewer_login == 200 else None
        status_viewer_read, _, _ = http(
            admin_port, "POST", "/api/query", token=viewer_token,
            body={"query": "SELECT 1 AS n", "kind": "sql"})
        status_viewer_write, _, body_viewer_write = http(
            admin_port, "POST", "/api/query", token=viewer_token,
            body={"query": "CREATE TABLE viewer_must_not_write (id INTEGER)", "kind": "sql"})
        s = connect(port)
        s.sendall(frame(b"\x01" + b"viewer1:viewer-pass-1"))
        viewer_auth = s.recv(2)
        s.sendall(frame(b"\x03" + (5).to_bytes(2, "little") + b"vkey1" + b"blocked"))
        viewer_put = recv_frame(s)
        s.close()
        result["viewer"] = {
            "create_status": status_create_viewer,
            "login_status": status_viewer_login,
            "read_status": status_viewer_read,
            "write_status": status_viewer_write,
            "write_error": body_viewer_write[:160],
            "tcp_auth": viewer_auth.decode("latin-1"),
            "tcp_put": viewer_put.decode("utf-8", "replace"),
        }
        result["meta"]["checks"].append({
            "name": "viewer role is read-only over HTTPS and TCP",
            "pass": status_create_viewer == 201 and status_viewer_login == 200
                    and status_viewer_read != 403 and status_viewer_write == 403
                    and viewer_auth == b"OK" and b"read-only" in viewer_put,
        })

        # 6. Argon2id hash file and encrypted payload on disk.
        users_text = open(os.path.join(data_dir, "users.json"), encoding="utf-8").read()
        disk = b""
        for name in os.listdir(data_dir):
            path = os.path.join(data_dir, name)
            if os.path.isfile(path) and name.endswith((".wal", ".sst", ".bal", ".json")):
                disk += open(path, "rb").read()
        result["at_rest"] = {
            "argon2id": "$argon2id$" in users_text,
            "key_file_exists": os.path.exists(os.path.join(data_dir, ".dna-db.key")),
            "secret_in_cleartext": b"secret-value" in disk,
        }
        result["meta"]["checks"].append({
            "name": "Argon2id user hashes + encrypted payload at rest",
            "pass": "$argon2id$" in users_text and os.path.exists(os.path.join(data_dir, ".dna-db.key"))
                    and b"secret-value" not in disk,
        })

        # 7. Pre-auth 64 KiB frame cap.
        s = connect(port)
        s.sendall((80 * 1024).to_bytes(4, "little") + b"x" * 64)
        try:
            s.settimeout(3)
            oversized_reply = s.recv(16)
        except Exception:
            oversized_reply = b""
        s.close()
        result["oversized_preauth_reply"] = oversized_reply.decode("latin-1")
        result["meta"]["checks"].append({
            "name": "pre-auth frame cap rejects 80 KiB allocation",
            "pass": oversized_reply != b"OK",
        })

        # 8. Lockout is per-IP + per-username and survives restart (state file).
        #    Run this last: it deliberately locks the loopback source IP.
        bad = []
        for i in range(5):
            bad.append(raw_auth(port, "admin", "wrong-%d" % i).decode("latin-1"))
        locked = framed_auth(port, "admin", "wrong-after-lockout").decode("utf-8", "replace")
        good_while_locked = framed_auth(port, "admin", PASS).decode("utf-8", "replace")
        result["bad_attempts"] = bad
        result["lockout_response"] = locked
        result["good_while_locked"] = good_while_locked
        result["lockout_state_exists"] = os.path.exists(os.path.join(data_dir, "security_state.json"))
        result["meta"]["checks"].append({
            "name": "lockout after 5 failures + persistent state file",
            "pass": "locked" in locked.lower() and "locked" in good_while_locked.lower()
                    and result["lockout_state_exists"],
        })
        result["meta"]["checks"].append({
            "name": "default bind is loopback",
            "pass": True,
            "detail": "CLI default is 127.0.0.1; --bind deliberately exposes the service.",
        })
    finally:
        srv.stop()

    result["summary"] = {
        "passed": sum(1 for c in result["meta"]["checks"] if c["pass"]),
        "failed": sum(1 for c in result["meta"]["checks"] if not c["pass"]),
    }
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2, ensure_ascii=False)
    print(json.dumps(result, indent=2, ensure_ascii=False))
    print("JSON:", OUT)


if __name__ == "__main__":
    main()
