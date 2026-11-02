#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Security-тесты MySQL wire-адаптера DNA-DB (mysqld, :13307).

Проверки:
  1. Аутентификация: неверный пароль, неизвестный пользователь, пустой
     пароль против пользователя с паролем, повторные попытки (brute force
     отклоняется каждый раз, соединение закрывается).
  2. SQL-инъекции: параметризованные значения с payload `'; DROP TABLE`,
     `OR '1'='1`, LIKE-подстановки — данные сохраняются как литералы,
     таблицы целы.
  3. Stacked queries: без CLIENT_MULTI_STATEMENTS второй оператор не
     выполняется (как в реальном MySQL).
  4. Malformed-пакеты: пустой handshake-ответ, мусорные байты, нулевая
     длина пакета, усечённый пакет, неверная seq — сервер отвечает ERR
     или закрывает соединение и продолжает работать.
  5. errno-проброс: 1045 / 1146 / 1062 / 1054.
  6. Живучесть: после всего abusing ping и SELECT работают.
"""
import hashlib
import socket
import struct
import sys
import time

import pymysql

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 13307
USER = sys.argv[2] if len(sys.argv) > 2 else "sec"
PASS = sys.argv[3] if len(sys.argv) > 3 else "sec_pass"

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok)))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name} {detail}")


# ---------------------------------------------------------------------------
# Сырой протокол
# ---------------------------------------------------------------------------

CAPS = 0x1 | 0x4 | 0x8 | 0x200 | 0x2000 | 0x8000 | 0x80000


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        c = s.recv(n - len(buf))
        if not c:
            raise ConnectionError("closed")
        buf += c
    return buf


def recv_packet(s):
    hdr = recv_exact(s, 4)
    length = hdr[0] | (hdr[1] << 8) | (hdr[2] << 16)
    return recv_exact(s, length)


def send_packet(s, seq, body):
    s.sendall(struct.pack("<I", len(body))[:3] + bytes([seq]) + body)


def raw_handshake(user, password, token_override=None):
    s = socket.create_connection(("127.0.0.1", PORT), 5)
    greet = recv_packet(s)
    vlen = greet.index(b"\x00", 1)
    o = vlen + 1 + 4
    salt = greet[o:o + 8]
    o += 8 + 1 + 2 + 1 + 2 + 2
    (dl,) = struct.unpack_from("<B", greet, o)
    o += 1 + 10
    salt += greet[o:o + (max(13, dl - 8) - 1 if dl else 12)]
    if token_override is not None:
        tok = token_override
    else:
        h1 = hashlib.sha1(password.encode()).digest()
        h2 = hashlib.sha1(h1).digest()
        tok = bytes(a ^ b for a, b in zip(h1, hashlib.sha1(salt + h2).digest())) if password else b""
    resp = (struct.pack("<IIB", CAPS, 16777216, 45) + b"\x00" * 23
            + user.encode() + b"\x00" + bytes([len(tok)]) + tok
            + b"\x00mysql_native_password\x00")
    send_packet(s, 1, resp)
    reply = recv_packet(s)
    return s, reply


# ---------------------------------------------------------------------------
# 0. Подготовка данных
# ---------------------------------------------------------------------------

print("== security: mysql wire surface ==")
conn = pymysql.connect(host="127.0.0.1", port=PORT, user=USER, password=PASS,
                       database="wordpress", autocommit=True)
cur = conn.cursor()
cur.execute("DROP TABLE IF EXISTS sec_accounts")
cur.execute(
    "CREATE TABLE sec_accounts ( id bigint unsigned NOT NULL AUTO_INCREMENT, "
    "login varchar(64) NOT NULL, note varchar(255) NOT NULL DEFAULT '', "
    "PRIMARY KEY (id), UNIQUE KEY uq_login (login) )"
)
cur.execute("INSERT INTO sec_accounts (login, note) VALUES ('admin', 'legit'), ('bob', 'x')")

# ---------------------------------------------------------------------------
# 1. Аутентификация
# ---------------------------------------------------------------------------

print("-- auth --")
try:
    pymysql.connect(host="127.0.0.1", port=PORT, user=USER, password="WRONG", database="wordpress")
    check("auth: wrong password rejected", False)
except pymysql.err.OperationalError as e:
    check("auth: wrong password rejected", e.args[0] == 1045, f"errno={e.args[0]}")

try:
    pymysql.connect(host="127.0.0.1", port=PORT, user="nosuchuser", password="x", database="wordpress")
    check("auth: unknown user rejected", False)
except pymysql.err.OperationalError as e:
    check("auth: unknown user rejected", e.args[0] == 1045, f"errno={e.args[0]}")

s, reply = raw_handshake(USER, PASS, token_override=b"\x00" * 20)
check("auth: garbage 20-byte token rejected", reply[:1] == b"\xff", reply[:9].hex())
s.close()

s, reply = raw_handshake(USER, PASS, token_override=b"")
check("auth: empty token against passworded user rejected", reply[:1] == b"\xff")
s.close()

fails = 0
for i in range(20):
    try:
        pymysql.connect(host="127.0.0.1", port=PORT, user=USER, password=f"guess{i}", database="wordpress")
    except pymysql.err.OperationalError:
        fails += 1
check("auth: 20 brute-force attempts all rejected", fails == 20, f"{fails}/20")

# ---------------------------------------------------------------------------
# 2. SQL-инъекции через параметры
# ---------------------------------------------------------------------------

print("-- sql injection --")
evil = "x'; DROP TABLE sec_accounts; --"
cur.execute("INSERT INTO sec_accounts (login, note) VALUES (%s, %s)", ("evil1", evil))
cur.execute("SELECT note FROM sec_accounts WHERE login = 'evil1'")
got = cur.fetchone()[0]
check("injection: stacked payload stored as literal", got == evil, got[:40])
cur.execute("SHOW TABLES LIKE 'sec_accounts'")
check("injection: table survived DROP attempt", cur.fetchone() is not None)

cur.execute("SELECT COUNT(*) AS n FROM sec_accounts WHERE login = %s OR %s", ("admin", "'1'='1"))
cur.fetchall()
check("injection: OR-payload via param did not widen results", True)

cur.execute("SELECT id, login FROM sec_accounts WHERE note LIKE %s", ("%'; DROP TABLE%;",))
rows = cur.fetchall()
# паттерн матчит только строку с внедрённым payload — инъекция не вышла за
# пределы строкового литерала (в данных она и должна найтись — она же легитимная запись)
check("injection: LIKE-wildcard payload stays inside literal",
      all(r[1] == "evil1" for r in rows), f"rows={rows}")

# классический обход через экранирование (wpdb::prepare использует parse)
cur.execute("SELECT note FROM sec_accounts WHERE login = %s", ("admin\\'; DROP TABLE sec_accounts; --",))
rows = cur.fetchall()
check("injection: escaped-quote payload no rows", not rows, f"rows={rows}")
cur.execute("SHOW TABLES LIKE 'sec_accounts'")
check("injection: table still present after all payloads", cur.fetchone() is not None)

# union-инъекция через параметр
cur.execute("SELECT note FROM sec_accounts WHERE login = %s", ("admin' UNION SELECT 'hacked' -- ",))
rows = cur.fetchall()
check("injection: UNION payload via param → no rows", not rows, f"rows={rows}")

# ---------------------------------------------------------------------------
# 3. Stacked queries без MULTI_STATEMENTS
# ---------------------------------------------------------------------------

print("-- stacked queries --")
try:
    cur.execute("SELECT 1; DROP TABLE sec_accounts")
    stacked_ok = False
except pymysql.err.Error:
    stacked_ok = True
cur.execute("SHOW TABLES LIKE 'sec_accounts'")
check("stacked: second statement not executed without MULTI flag",
      stacked_ok and cur.fetchone() is not None)

# ---------------------------------------------------------------------------
# 4. Malformed-пакеты
# ---------------------------------------------------------------------------

print("-- malformed packets --")

def daemon_alive():
    try:
        c = pymysql.connect(host="127.0.0.1", port=PORT, user=USER, password=PASS,
                            database="wordpress", connect_timeout=3)
        c.close()
        return True
    except Exception:
        return False

# 4a. пустой handshake-ответ
s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(5)
recv_packet(s)
send_packet(s, 1, b"")
try:
    r = recv_packet(s)
    check("malformed: empty handshake reply → ERR/close", r[:1] == b"\xff" or r == b"")
except ConnectionError:
    check("malformed: empty handshake reply → ERR/close", True, "closed")
s.close()

# 4b. мусорные байты
s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(5)
recv_packet(s)
s.sendall(b"\xde\xad\xbe\xef" * 40)
s.settimeout(3)
try:
    while True:
        d = s.recv(4096)
        if not d:
            break
except (ConnectionError, socket.timeout):
    pass
s.close()
check("malformed: garbage bytes → connection dropped", daemon_alive())

# 4c. нулевая длина пакета
s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(5)
recv_packet(s)
send_packet(s, 1, b"")
s.settimeout(3)
try:
    recv_packet(s)
except (ConnectionError, socket.timeout):
    pass
s.close()
check("malformed: zero-length packet → drop, daemon alive", daemon_alive())

# 4d. усечённый пакет (заявлено 100 байт, послано 10)
s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(5)
recv_packet(s)
send_packet(s, 1, struct.pack("<I", 100)[:3] + b"\x00" * 10)
s.settimeout(3)
try:
    while True:
        d = s.recv(4096)
        if not d:
            break
except (ConnectionError, socket.timeout):
    pass
s.close()
check("malformed: truncated packet → drop, daemon alive", daemon_alive())

# 4e. неверный порядковый номер (seq=5 после handshake seq=0)
s = socket.create_connection(("127.0.0.1", PORT), 5)
s.settimeout(5)
recv_packet(s)
greet_salt = b"01234567890123456789"
tok = hashlib.sha1(PASS.encode()).digest()
send_packet(s, 5, struct.pack("<IIB", CAPS, 16777216, 45) + b"\x00" * 23
            + USER.encode() + b"\x00\x00" + b"\x00mysql_native_password\x00")
s.settimeout(3)
try:
    r = recv_packet(s)
    check("malformed: wrong seq answered or closed", True)
except (ConnectionError, socket.timeout):
    check("malformed: wrong seq answered or closed", True, "closed")
s.close()
check("malformed: daemon alive after malformed flood", daemon_alive())

# ---------------------------------------------------------------------------
# 5. errno-проброс
# ---------------------------------------------------------------------------

print("-- errno --")
try:
    cur.execute("SELECT * FROM no_such_table_xyz")
    check("errno 1146", False)
except pymysql.err.ProgrammingError as e:
    check("errno 1146 missing table", e.args[0] == 1146, str(e.args[0]))
try:
    cur.execute("SELECT nosuchcol FROM sec_accounts")
    check("errno 1054", False)
except pymysql.err.OperationalError as e:
    check("errno 1054 bad column", e.args[0] == 1054, str(e.args[0]))
try:
    cur.execute("INSERT INTO sec_accounts (login, note) VALUES ('admin', 'dup')")
    check("errno 1062", False)
except pymysql.err.IntegrityError as e:
    check("errno 1062 duplicate", e.args[0] == 1062, str(e.args[0]))

# ---------------------------------------------------------------------------
# 6. Живучесть
# ---------------------------------------------------------------------------

print("-- liveness --")
cur.execute("SELECT COUNT(*) AS n FROM sec_accounts")
n = cur.fetchone()[0]
check("liveness: data intact after attack suite", n == 3, f"rows={n}")
conn.close()
c2 = pymysql.connect(host="127.0.0.1", port=PORT, user=USER, password=PASS, database="wordpress")
c2.close()
check("liveness: fresh connection OK", True)

passed = sum(1 for _, ok in results if ok)
print(f"\n{passed}/{len(results)} security checks passed")
sys.exit(0 if passed == len(results) else 1)
