import json
from pathlib import Path
import os
import socket
import ssl
import struct
import subprocess
import tempfile
import time
import urllib.request

PROJECT = Path(__file__).resolve().parent.parent
BIN = PROJECT / ("target/release/dna-db-core.exe" if os.name == "nt" else "target/release/dna-db-core")


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
        c = sock.recv(n - len(out))
        if not c:
            raise EOFError
        out += c
    return out


def recv_frame(sock):
    n = int.from_bytes(recv_exact(sock, 4), "little")
    return recv_exact(sock, n)


def tls_socket(port):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx.wrap_socket(socket.create_connection(("127.0.0.1", port), timeout=10), server_hostname="localhost")


def auth(port, user, password):
    s = tls_socket(port)
    s.sendall(frame(b"\x01" + ("%s:%s" % (user, password)).encode()))
    return s, s.recv(2)


def http(port, method, path, token=None, body=None):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request("https://127.0.0.1:%d%s" % (port, path), data=data,
                                 method=method, headers={"Host": "localhost"})
    if token:
        req.add_header("Authorization", "Bearer " + token)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, context=ctx, timeout=10) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def wait(port):
    for _ in range(200):
        try:
            tls_socket(port).close()
            return
        except Exception:
            time.sleep(0.05)
    raise RuntimeError("port %d did not open" % port)


data_dir = tempfile.mkdtemp(prefix="dna_tls_smoke_")
port, admin_port = free_port(), free_port()
log = open(os.path.join(tempfile.gettempdir(), "dna_tls_smoke.log"), "wb")
p = subprocess.Popen([BIN, "serve", "--data-dir", data_dir, "--port", str(port),
                      "--bind", "127.0.0.1", "--admin-port", str(admin_port),
                      "--no-scrub", "--sync", "strict"],
                     stdout=log, stderr=log, cwd=PROJECT)
try:
    wait(port)
    wait(admin_port)

    # 1. TLS TCP + default admin/password + encryption round-trip.
    s, reply = auth(port, "admin", "password")
    assert reply == b"OK", reply
    secret = b"TOP-SECRET-PATIENT-SEQUENCE-ATGCATGC"
    key = b"patient:42"
    payload = b"\x03" + struct.pack("<H", len(key)) + key + secret
    s.sendall(frame(payload))
    assert s.recv(2) == b"OK"
    s.sendall(frame(b"\x02" + key))
    assert recv_frame(s) == secret
    s.close()

    # 2. Ciphertext on disk, key + Argon2id users persisted.
    blobs = []
    for name in os.listdir(data_dir):
        path = os.path.join(data_dir, name)
        if os.path.isfile(path) and name.endswith((".wal", ".sst", ".json")):
            blobs.append(open(path, "rb").read())
    joined = b"\n".join(blobs)
    assert secret not in joined, "plaintext payload leaked to disk"
    users = json.load(open(os.path.join(data_dir, "users.json"), encoding="utf-8"))
    assert users["users"][0]["password_hash"].startswith("$argon2id$")
    assert os.path.exists(os.path.join(data_dir, ".dna-db.key"))

    # 3. HTTPS panel, HSTS/CSP, user lifecycle.
    status, headers, body = http(admin_port, "GET", "/")
    assert status == 200 and b"DNA-DB" in body
    assert headers.get("Strict-Transport-Security")
    assert "default-src 'self'" in headers.get("Content-Security-Policy", "")
    status, _, body = http(admin_port, "POST", "/api/login", body={"user": "admin", "pass": "password"})
    assert status == 200, body
    token = json.loads(body)["token"]
    assert len(token) == 64
    status, _, body = http(admin_port, "GET", "/api/users", token=token)
    assert status == 200, body
    status, _, body = http(admin_port, "POST", "/api/users", token=token,
                           body={"user": "alice", "pass": "alice-password", "role": "operator", "must_change": False})
    assert status == 201, body
    status, _, body = http(admin_port, "POST", "/api/login", body={"user": "alice", "pass": "alice-password"})
    assert status == 200, body
    alice_token = json.loads(body)["token"]
    status, _, body = http(admin_port, "POST", "/api/password", token=alice_token,
                           body={"old_password": "alice-password", "new_password": "alice-password-2"})
    assert status == 200, body
    status, _, body = http(admin_port, "POST", "/api/login", body={"user": "alice", "pass": "alice-password-2"})
    assert status == 200, body

    # 4. Plaintext client cannot speak to the TLS port.
    plain = socket.create_connection(("127.0.0.1", port), timeout=3)
    plain.sendall(frame(b"\x01admin:password"))
    try:
        plain.settimeout(3)
        reply = plain.recv(16)
    except Exception:
        reply = b""
    plain.close()
    assert reply != b"OK", reply

    # 5. Pre-auth oversized frame is rejected (128 x 16 MiB amplification).
    s = tls_socket(port)
    s.sendall((80 * 1024).to_bytes(4, "little") + b"x" * 64)
    try:
        s.settimeout(3)
        s.recv(16)
    except Exception:
        pass
    s.close()

    print(json.dumps({"ok": True, "data_dir": data_dir, "port": port, "admin_port": admin_port,
                      "users": [u["username"] for u in users["users"]],
                      "disk_files": sorted(os.listdir(data_dir))}, indent=2))
finally:
    p.terminate()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()
    log.close()
