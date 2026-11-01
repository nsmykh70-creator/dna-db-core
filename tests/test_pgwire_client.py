#!/usr/bin/env python3
"""Real PostgreSQL wire protocol v3 client test for DNA-DB pgwire (v3.7).

Speaks the frontend protocol directly over a socket — the same handshake a
psycopg2/JDBC/psql client performs — and validates: startup, auth-ok,
parameter status, simple query, RowDescription/DataRow/CommandComplete,
error handling, transaction statements and Terminate.
"""
import socket
import struct
import sys


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("closed")
        buf += chunk
    return buf


def recv_message(sock):
    tag = recv_exact(sock, 1)
    (length,) = struct.unpack("!i", recv_exact(sock, 4))
    body = recv_exact(sock, length - 4) if length > 4 else b""
    return tag, body


def connect(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    # StartupMessage: protocol 3.0 + user/admin + database/dna
    payload = struct.pack("!i", 196608) + b"user\0admin\0database\0dna\0\0"
    s.sendall(struct.pack("!i", len(payload) + 4) + payload)

    tag, body = recv_message(s)
    if tag == b"N":  # SSL fallback not used by this client
        tag, body = recv_message(s)
    params = {}
    backend_key = None
    while True:
        if tag == b"R":
            (code,) = struct.unpack("!i", body[:4])
            assert code == 0, f"unexpected auth code {code}"
        elif tag == b"S":
            k, v = body.split(b"\0")[:2]
            params[k.decode()] = v.decode()
        elif tag == b"K":
            backend_key = body
        elif tag == b"Z":
            ready = True
            break
        tag, body = recv_message(s)
    return s, params, backend_key


def query(s, sql):
    """Simple query; returns (columns, rows, tag) or raises RuntimeError(msg)."""
    body = sql.encode() + b"\0"
    s.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
    columns, rows, tag_final, error = [], [], None, None
    while True:
        tag, body = recv_message(s)
        if tag == b"T":  # RowDescription
            (n,) = struct.unpack("!h", body[:2])
            off = 2
            for _ in range(n):
                end = body.index(b"\0", off)
                name = body[off:end].decode()
                off = end + 1
                off += 4 + 2  # table oid, attr
                (oid,) = struct.unpack("!i", body[off:off + 4])
                off += 4 + 2 + 4 + 2
                columns.append((name, oid))
        elif tag == b"D":  # DataRow
            (n,) = struct.unpack("!h", body[:2])
            off = 2
            row = []
            for _ in range(n):
                (ln,) = struct.unpack("!i", body[off:off + 4])
                off += 4
                if ln == -1:
                    row.append(None)
                else:
                    row.append(body[off:off + ln].decode())
                    off += ln
            rows.append(row)
        elif tag == b"C":  # CommandComplete
            tag_final = body.rstrip(b"\0").decode()
        elif tag == b"E":  # ErrorResponse
            error = body
        elif tag == b"Z":
            if error is not None:
                msg = b"M\0"
                parts = error.split(b"\0")
                for i, p in enumerate(parts):
                    if p.startswith(b"M"):
                        msg = p
                        break
                raise RuntimeError(msg[1:].decode())
            return columns, rows, tag_final


def main(port):
    s, params, key = connect(port)
    assert "server_version" in params, params
    print("[1] handshake OK:", params["server_version"])

    cols, rows, tag = query(s, "SELECT VERSION()")
    # cms builds route through the MySQL-dialect gate (column VERSION(),
    # MySQL-compat version string); plain builds answer as "version".
    assert cols[0][0] in ("version", "VERSION()"), cols
    assert "DNA-DB" in rows[0][0] or rows[0][0].startswith("8.0."), rows
    print("[2] VERSION() ->", rows[0][0])

    cols, rows, tag = query(s, "CREATE TABLE chr_pg (id INT, label TEXT, PRIMARY KEY (id))")
    assert tag and "CREATE" in tag, tag
    cols, rows, tag = query(s, "INSERT INTO chr_pg (id, label) VALUES (1, 'alpha'), (2, 'beta')")
    assert tag.startswith("INSERT"), tag
    print("[3] DDL/DML via pgwire:", tag)

    cols, rows, tag = query(s, "SELECT id, label FROM chr_pg ORDER BY id ASC")
    assert [r[0] for r in rows] == ["1", "2"], rows
    assert rows[0][1] == "alpha"
    assert tag == "SELECT 2", tag
    print("[4] SELECT rows:", rows, "tag:", tag)

    # v3.6 features over the wire
    cols, rows, tag = query(
        s,
        "SELECT label, ROW_NUMBER() OVER (ORDER BY id DESC) AS rn FROM chr_pg ORDER BY id ASC",
    )
    assert rows[1][1] == "1", rows  # beta is rn=1 (id DESC)
    print("[5] window function over pgwire:", rows)

    # NULL rendering
    query(s, "INSERT INTO chr_pg (id, label) VALUES (3, NULL)")
    cols, rows, tag = query(s, "SELECT id, label FROM chr_pg WHERE id = 3")
    assert rows[0][1] is None, rows
    print("[6] NULL roundtrip OK")

    # Error surface
    err_text = None
    try:
        query(s, "SELECT * FROM chr_missing")
        raise AssertionError("expected error")
    except RuntimeError as err:
        err_text = str(err)
        assert "not registered" in err_text or "not found" in err_text or "does not exist" in err_text, err_text
    print("[7] ErrorResponse surface OK:", err_text)

    # Transactions over pgwire
    query(s, "BEGIN")
    query(s, "INSERT INTO chr_pg (id, label) VALUES (99, 'temp')")
    query(s, "ROLLBACK")
    cols, rows, tag = query(s, "SELECT COUNT(*) AS n FROM chr_pg WHERE id = 99")
    assert rows[0][0] == "0", rows
    query(s, "BEGIN")
    query(s, "INSERT INTO chr_pg (id, label) VALUES (99, 'kept')")
    query(s, "COMMIT")
    cols, rows, tag = query(s, "SELECT COUNT(*) AS n FROM chr_pg WHERE id = 99")
    assert rows[0][0] == "1", rows
    print("[8] BEGIN/ROLLBACK/COMMIT over pgwire OK")

    # Terminate
    s.sendall(b"X" + struct.pack("!i", 4))
    s.close()
    print("PGWIRE CLIENT TEST: ALL PASS")


if __name__ == "__main__":
    main(int(sys.argv[1]) if len(sys.argv) > 1 else 5432)
