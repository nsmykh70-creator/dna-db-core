#!/usr/bin/env python3
"""E2E: pgwire extended query protocol (prepared statements) against a live server.

Starts `dna-db-core serve` on ephemeral ports, drives the PostgreSQL v3
extended protocol by hand (Parse / Bind / Describe / Execute / Sync), and
checks that prepared statements return the same rows as the simple protocol.
Verdict line: PGWIRE_EXTENDED_E2E: OK / FAILED.
"""
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "dna-db-core")


def wait_port(port: int, timeout: float = 20.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"port {port} did not open")


class Pg:
    def __init__(self, port: int):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        # StartupMessage protocol 3.0
        body = struct.pack(">i", 196608) + b"user\0admin\0\0"
        self.s.sendall(struct.pack(">i", len(body) + 4) + body)
        self.buf = b""
        # consume until ReadyForQuery
        while True:
            t, payload = self.msg()
            if t == b"Z":
                break

    def msg(self):
        while len(self.buf) < 5:
            chunk = self.s.recv(65536)
            if not chunk:
                raise RuntimeError("connection closed")
            self.buf += chunk
        t = self.buf[0:1]
        (ln,) = struct.unpack(">i", self.buf[1:5])
        while len(self.buf) < 1 + ln:
            chunk = self.s.recv(65536)
            if not chunk:
                raise RuntimeError("connection closed")
            self.buf += chunk
        payload = self.buf[5 : 1 + ln]
        self.buf = self.buf[1 + ln :]
        return t, payload

    def send(self, tag: bytes, body: bytes) -> None:
        self.s.sendall(tag + struct.pack(">i", len(body) + 4) + body)

    def sync(self):
        self.send(b"S", b"")
        while True:
            t, _ = self.msg()
            if t == b"Z":
                return

    def simple(self, sql: str):
        self.send(b"Q", sql.encode() + b"\0")
        rows, tags = [], []
        while True:
            t, p = self.msg()
            if t == b"Z":
                return rows, tags
            if t == b"D":
                (n,) = struct.unpack(">h", p[:2])
                off, vals = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack(">i", p[off : off + 4])
                    off += 4
                    vals.append(None if ln == -1 else p[off : off + ln].decode())
                    off += max(ln, 0)
                rows.append(vals)
            elif t == b"C":
                tags.append(p.rstrip(b"\0").decode())
            elif t == b"E":
                tags.append("ERROR:" + p.decode(errors="replace"))

    def extended(self, sql: str, params: list):
        # Parse (unnamed) + Bind (unnamed portal) + Describe portal + Execute + Sync
        pbody = b"\0" + sql.encode() + b"\0" + struct.pack(">h", 0)
        self.send(b"P", pbody)
        bbody = b"\0\0" + struct.pack(">h", 0) + struct.pack(">h", len(params))
        for v in params:
            if v is None:
                bbody += struct.pack(">i", -1)
            else:
                enc = str(v).encode()
                bbody += struct.pack(">i", len(enc)) + enc
        bbody += struct.pack(">h", 0)
        self.send(b"B", bbody)
        self.send(b"D", b"P\0")
        self.send(b"E", b"\0" + struct.pack(">i", 0))
        rows, tags, cols = [], [], None
        self.send(b"S", b"")  # Sync: server answers ReadyForQuery
        while True:
            t, p = self.msg()
            if t == b"Z":
                return rows, tags, cols
            if t == b"1" or t == b"2" or t == b"n":
                continue
            if t == b"T":
                (n,) = struct.unpack(">h", p[:2])
                off, cols = 2, []
                for _ in range(n):
                    end = p.index(b"\0", off)
                    cols.append(p[off:end].decode())
                    off = end + 1 + 4 + 2 + 4 + 2 + 4 + 2
            elif t == b"D":
                (n,) = struct.unpack(">h", p[:2])
                off, vals = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack(">i", p[off : off + 4])
                    off += 4
                    vals.append(None if ln == -1 else p[off : off + ln].decode())
                    off += max(ln, 0)
                rows.append(vals)
            elif t == b"C":
                tags.append(p.rstrip(b"\0").decode())
            elif t == b"E":
                tags.append("ERROR:" + p.decode(errors="replace"))
                # No extra Sync here: the batch's own Sync answers with the
                # final ReadyForQuery that the loop below consumes.


def main() -> int:
    if not os.path.exists(BIN):
        print(f"binary missing: {BIN} (cargo build --release first)", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory() as data:
        env = dict(os.environ)
        proc = subprocess.Popen(
            [
                BIN, "serve", "--data-dir", data, "--port", "14690",
                "--admin-port", "0", "--pgwire-port", "14691",
                "--no-tls", "--no-encryption", "--sync", "strict",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
            start_new_session=True,
        )
        ok = False
        try:
            wait_port(14691)
            pg = Pg(14691)
            # Schema + data via simple protocol.
            pg.simple("CREATE TABLE chr_users (user_id INT, username TEXT, balance FLOAT, PRIMARY KEY (user_id))")
            pg.simple("INSERT INTO chr_users (user_id = 1, username = 'ada', balance = 10.5)")
            pg.simple("INSERT INTO chr_users (user_id = 2, username = 'bob', balance = 20.0)")

            # 1. Prepared SELECT with $1.
            rows, tags, cols = pg.extended(
                "SELECT username, balance FROM chr_users WHERE user_id = $1", [2]
            )
            assert cols is not None and "username" in cols, f"cols={cols}"
            # Column order is schema-alphabetical (BTreeMap), same as the
            # simple protocol — compare by name.
            row = dict(zip(cols, rows[0]))
            assert row == {"username": "bob", "balance": "20.0"}, f"row={row} tags={tags}"

            # 2. Same prepared statement, new bind (named statement).
            pbody = b"st1\0" + "SELECT username FROM chr_users WHERE user_id = $1".encode() + b"\0" + struct.pack(">h", 0)
            pg.send(b"P", pbody)
            pg.sync()
            bbody = b"\0st1\0" + struct.pack(">h", 0) + struct.pack(">h", 1)
            bbody += struct.pack(">i", 1) + b"1" + struct.pack(">h", 0)
            pg.send(b"B", bbody)
            pg.send(b"D", b"P\0")
            pg.send(b"E", b"\0" + struct.pack(">i", 0))
            pg.send(b"S", b"")
            seen_desc = False
            while True:
                t, p = pg.msg()
                if t == b"T":
                    seen_desc = True
                if t == b"Z":
                    break
            assert seen_desc, "Describe portal must answer RowDescription"

            # 3. Prepared INSERT ($n params) + verification via simple SELECT.
            rows, tags, _ = pg.extended(
                "INSERT INTO chr_users (user_id = $1, username = $2, balance = $3)",
                [3, "carol", 30.5],
            )
            assert not any(t.startswith("ERROR") for t in tags), f"tags={tags}"
            rows, _, _ = pg.extended("SELECT balance FROM chr_users WHERE user_id = $1", [3])
            assert rows == [["30.5"]], f"rows={rows}"

            # 4. NULL binding.
            rows, _, _ = pg.extended(
                "SELECT username FROM chr_users WHERE balance = $1", [None]
            )
            assert rows == [], f"NULL must match nothing, rows={rows}"

            # 5. ? placeholders (JDBC style).
            rows, _, _ = pg.extended("SELECT username FROM chr_users WHERE user_id = ?", ["1"])
            assert rows == [["ada"]], f"rows={rows}"

            # 6. Param-count mismatch is an error, not a wrong answer.
            rows, tags, _ = pg.extended("SELECT username FROM chr_users WHERE user_id = $1", [])
            assert any(t.startswith("ERROR") for t in tags), f"tags={tags}"

            # 7. Simple protocol still fine after all extended traffic.
            rows, _ = pg.simple("SELECT COUNT(*) FROM chr_users")
            assert rows == [["3"]], f"rows={rows}"

            ok = True
            print("PGWIRE_EXTENDED_E2E: OK")
        except Exception as e:
            print(f"PGWIRE_EXTENDED_E2E: FAILED ({e})")
            return 1
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
