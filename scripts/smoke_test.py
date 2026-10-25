#!/usr/bin/env python3
"""Живой smoke-тест продакшен-бинарника DNA-DB.

Поднимает реальный сервер (release-сборка, strict fsync), прогоняет все
opcode протокола и проверяет recovery после жёсткой остановки процесса.

Checks:
  1. auth_ok / auth_bad      — 0x01 AUTH (верный и неверный пароль)
  2. preauth_guard           — неавторизованный запрос отклоняется
  3. ping                    — 0x00 PING, JSON stats
  4. put_get / get_miss      — 0x03/0x02 roundtrip + отсутствие ключа
  5. delete                  — 0x04 DELETE
  6. big_value               — значение 1 MiB (границы фрейма)
  7. sql_crud                — 0x11 SQL: CREATE/INSERT/SELECT WHERE
  8. dnaql_analyze           — 0x10 DnaQL: ANALYZE SEQUENCE |> GC_CONTENT()
  9. checkpoint              — 0x12 CHECKPOINT
 10. bad_opcode              — неизвестный опкод -> ERR, соединение живо
 11. restart_recovery        — жёсткая остановка + рестарт: данные на месте
"""

import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = os.path.dirname(HERE)
PORT = 19755
PASS = "smoke-secret"
RESULTS = []


def find_binary():
    name = "dna-db-core.exe" if os.name == "nt" else "dna-db-core"
    path = os.path.join(PROJECT, "target", "release", name)
    if not os.path.exists(path):
        raise RuntimeError("release binary not found: %s" % path)
    return path


class Client:
    def __init__(self, port=PORT, user="admin", password=PASS):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
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
            if not chunk:
                raise ConnectionError("EOF")
            buf += chunk
        return buf

    def _recv_frame(self):
        (length,) = struct.unpack("<I", self._recv_exact(4))
        return self._recv_exact(length) if length else b""

    def op(self, payload, framed_response=True):
        self._send(payload)
        if framed_response:
            return self._recv_frame()
        return self._recv_exact(2)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def start_server(data_dir, log):
    proc = subprocess.Popen(
        [find_binary(), "serve",
         "--data-dir", data_dir, "--port", str(PORT),
         "--bind", "127.0.0.1", "--admin-pass", PASS,
         "--sync", "strict", "--no-scrub"],
        stdout=log, stderr=log, cwd=PROJECT)
    deadline = time.time() + 20
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError("server died on startup")
        try:
            c = Client(user=None)
            c.close()
            return proc
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("server did not become ready")


def kill_server(proc):
    """Hard kill (SIGKILL/TerminateProcess) — crash semantics."""
    if proc.poll() is None:
        os.kill(proc.pid, 9)
        proc.wait()


def check(name, ok, detail=""):
    RESULTS.append({"check": name, "status": "PASS" if ok else "FAIL",
                    "detail": detail})
    print("  [%s] %s%s" % ("PASS" if ok else "FAIL", name,
                           (" — " + detail) if detail else ""))
    return ok


def main():
    data_dir = tempfile.mkdtemp(prefix="dna_smoke_")
    log_path = os.path.join(tempfile.gettempdir(), "dna_smoke_srv.log")
    log = open(log_path, "wb")
    all_ok = True
    proc = start_server(data_dir, log)
    try:
        # 1. AUTH
        c = Client()
        check("auth_ok", True)
        bad = Client(user=None)
        bad._send(b"\x01admin:wrong-pass")
        resp = bad._recv_exact(2)
        check("auth_bad", resp == b"NO", "wrong password -> %r" % resp)
        bad.close()

        # 2. pre-auth guard (fresh connection, no AUTH)
        un = Client(user=None)
        un._send(b"\x03" + struct.pack("<H", 1) + b"k" + b"v")
        resp = un._recv_frame()
        un.close()
        check("preauth_guard", resp.startswith(b"ERR"), "%r" % resp[:60])

        # 3. PING
        resp = c.op(b"\x00")
        try:
            data = json.loads(resp)
            ok = data.get("status") == "ok" and "stats" in data
            check("ping", ok, "status=%s" % data.get("status"))
        except Exception as e:
            check("ping", False, "bad JSON: %s" % e)

        # 4. PUT/GET + miss
        value = b"ATGCGTACGTTAGC" * 10
        resp = c.op(b"\x03" + struct.pack("<H", 6) + b"chr1:1" + value,
                    framed_response=False)
        got = c.op(b"\x02" + b"chr1:1")
        check("put_get", resp == b"OK" and got == value,
              "ack=%r len=%d" % (resp, len(got)))
        miss = c.op(b"\x02" + b"no-such-key")
        check("get_miss", miss == b"", "empty frame on miss")

        # 5. DELETE
        resp = c.op(b"\x04" + b"chr1:1", framed_response=False)
        after = c.op(b"\x02" + b"chr1:1")
        check("delete", resp == b"OK" and after == b"")

        # 6. big value (1 MiB)
        big = os.urandom(1024 * 1024)
        resp = c.op(b"\x03" + struct.pack("<H", 6) + b"chr1:9" + big,
                    framed_response=False)
        got = c.op(b"\x02" + b"chr1:9")
        check("big_value", resp == b"OK" and got == big,
              "1 MiB roundtrip, %d bytes" % len(got))

        # 7. SQL CRUD
        r = c.op(b"\x11" + b"CREATE TABLE smoke_t (id INT, name TEXT, "
                           b"score FLOAT, PRIMARY KEY (id))")
        check("sql_create", not r.startswith(b"ERR"), "%r" % r[:80])
        r = c.op(b"\x11" + b"INSERT INTO smoke_t (id, name, score) VALUES "
                           b"(1, 'watson', 99.5)")
        check("sql_insert", not r.startswith(b"ERR"), "%r" % r[:80])
        r = c.op(b"\x11" + b"SELECT name, score FROM smoke_t WHERE id = 1")
        try:
            rows = json.loads(r).get("rows", [])
            ok = rows and rows[0].get("name") == "watson" \
                and abs(float(rows[0].get("score")) - 99.5) < 0.001
            check("sql_select", bool(ok), "%d row(s)" % len(rows))
        except Exception as e:
            check("sql_select", False, "bad JSON: %s / %r" % (e, r[:120]))

        # 8. DnaQL bio-analysis
        r = c.op(b"\x10" + b'ANALYZE SEQUENCE "ATGCGCATGC" |> GC_CONTENT()')
        try:
            data = json.loads(r)
            ok = not r.startswith(b"ERR")
            check("dnaql_analyze", ok, "%r" % r[:100])
        except Exception:
            check("dnaql_analyze", not r.startswith(b"ERR"), "%r" % r[:100])

        # 9. CHECKPOINT
        resp = c.op(b"\x12", framed_response=False)
        check("checkpoint", resp == b"OK")

        # 10. bad opcode -> ERR, connection stays alive
        resp = c.op(b"\x7f")
        alive = c.op(b"\x00")
        check("bad_opcode", resp.startswith(b"ERR") and
              alive.startswith(b"{"), "%r" % resp[:60])

        # durable key for restart test (before hard kill)
        c.op(b"\x03" + struct.pack("<H", 7) + b"chr1:77" + b"DO-NOT-LOSE",
             framed_response=False)
        c.close()
    finally:
        # 11. hard kill + restart + recovery
        kill_server(proc)
        proc2 = start_server(data_dir, log)
        try:
            c = Client()
            got = c.op(b"\x02" + b"chr1:77")
            check("restart_recovery", got == b"DO-NOT-LOSE",
                  "after hard kill + WAL replay: %r" % got[:30])
            c.close()
        finally:
            kill_server(proc2)
    log.close()
    shutil.rmtree(data_dir, ignore_errors=True)

    out = os.path.join(PROJECT, "docs", "smoke_test_results.json")
    with open(out, "w") as f:
        json.dump({"results": RESULTS,
                   "summary": "PASS" if all(r["status"] == "PASS"
                                            for r in RESULTS) else "FAIL"},
                  f, indent=2, ensure_ascii=False)
    failed = [r for r in RESULTS if r["status"] == "FAIL"]
    print("\n%d checks, %d failed" % (len(RESULTS), len(failed)))
    print("JSON: %s" % out)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
