#!/usr/bin/env python3
"""v3.5.0 security smoke: random bootstrap password, deep-query guard,
slowloris deadline, doctor-vs-server lock, second-instance lock."""
import socket, struct, subprocess, tempfile, time, os, re, signal, sys, json, threading

BIN = "/home/z/my-project/workspace/dna-db-core-main/target/release/dna-db-core"
PORT = 19821
results = []
def check(name, ok, detail=""):
    results.append((name, bool(ok), str(detail)))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name} {detail}")

class Client:
    def __init__(self, port=PORT, user="admin", password=None):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=15)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        if user is not None:
            self._send(b"\x01" + user.encode() + b":" + password.encode())
            if self._recv_exact(2) != b"OK":
                raise RuntimeError("auth failed")
    def _send(self, payload):
        self.sock.sendall(struct.pack("<I", len(payload)) + payload)
    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk: raise ConnectionError("EOF")
            buf += chunk
        return buf
    def _recv_frame(self):
        (length,) = struct.unpack("<I", self._recv_exact(4))
        return self._recv_exact(length) if length else b""
    def op(self, payload):
        self._send(payload)
        if payload[:1] == b"\x01":
            return self._recv_exact(2)
        return self._recv_frame()
    def close(self):
        try: self.sock.close()
        except OSError: pass

def start_server(d, port, capture_stderr=False):
    err = subprocess.PIPE if capture_stderr else subprocess.DEVNULL
    proc = subprocess.Popen([BIN, "serve", "--data-dir", d, "--port", str(port),
                             "--bind", "127.0.0.1", "--no-tls", "--no-scrub"],
                            stderr=err, stdout=subprocess.DEVNULL)
    deadline = time.time() + 20
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("server died on startup: " + (proc.stderr.read().decode() if capture_stderr else ""))
        try:
            Client(port=port, user=None).close()
            return proc
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("server not ready")

d = tempfile.mkdtemp(prefix="dna_sec_")
proc = start_server(d, PORT, capture_stderr=True)
time.sleep(0.3)
os.set_blocking(proc.stderr.fileno(), False)
err_out = proc.stderr.read() or b""

m = re.search(rb"admin / ([A-Za-z0-9]{20,})", err_out)
pw = m.group(1).decode() if m else None
check("random_bootstrap_password_printed", pw is not None, pw or "not found")
check("static_default_gone", b"admin / password" not in err_out)
check("must_change_seeded", b"must_change = true" in err_out)

c = Client(port=PORT, password=pw)
check("auth_with_generated_password", True)

# deep query -> ERR, connection + server survive
deep = b"\x11" + ("SELECT * FROM chr_t WHERE " + "(" * 200000 + "1=1" + ")" * 200000).encode()
resp = c.op(deep)
check("deep_query_rejected_not_crash", b"nested too deeply" in resp, resp[:100])
resp = c.op(b"\x00")
check("server_alive_after_deep_query", json.loads(resp).get("status") == "ok")

# bare DROP INDEX: viewer account is refused (read-only); table-restricted
# accounts get the ACL error via referenced_tables — covered by unit tests.
# Here: admin still may use the legacy bare form (no ACL check for admin).
c.op(b"\x11" + b"CREATE TABLE chr_pub (id INT, PRIMARY KEY (id))")
resp = c.op(b"\x11" + b"DROP INDEX no_such_idx")
# admin is not ACL-blocked on the bare form; it reaches the executor, which
# answers with the normal if-exists error.
check("admin_bare_drop_index_reaches_executor", b"does not exist" in resp, resp[:90])

# slowloris: open a socket, send nothing, must be dropped by ~30s deadline
slow = socket.create_connection(("127.0.0.1", PORT), timeout=40)
t0 = time.time()
dropped = False
try:
    slow.settimeout(40)
    data = slow.recv(64)  # blocks until server drops us
    if data == b"": dropped = True
except (ConnectionResetError, socket.timeout):
    dropped = time.time() - t0 < 40
elapsed = time.time() - t0
slow.close()
check("slowloris_deadline", dropped and elapsed < 45, f"dropped after {elapsed:.1f}s")

# second instance on the same data dir -> refused
second = subprocess.run([BIN, "serve", "--data-dir", d, "--port", str(PORT + 1),
                         "--bind", "127.0.0.1", "--no-tls"], capture_output=True, timeout=60)
check("second_instance_refused", b"locked by another dna-db-core" in second.stderr, second.stderr.decode()[:100])

# doctor on a LIVE database -> refuses
doc = subprocess.run([BIN, "doctor", "--data-dir", d], capture_output=True, timeout=120)
check("doctor_refuses_live_db", b"refusing to run" in doc.stderr, doc.stderr.decode()[:110])

proc.send_signal(signal.SIGKILL); proc.wait()
doc = subprocess.run([BIN, "doctor", "--data-dir", d], capture_output=True, timeout=120)
check("doctor_works_after_stop", doc.returncode == 0 and b"Doctor report" in doc.stdout, doc.stdout.decode()[:80])

proc2 = start_server(d, PORT)
c2 = Client(port=PORT, password=pw)
check("password_survives_restart", True)
proc2.send_signal(signal.SIGKILL); proc2.wait()

passed = sum(1 for _, ok, _ in results if ok)
print(json.dumps({"passed": passed, "total": len(results)}))
sys.exit(0 if passed == len(results) else 1)
