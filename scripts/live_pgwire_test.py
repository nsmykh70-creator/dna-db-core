#!/usr/bin/env python3
"""Live pgwire v4.2 verification: pg8000 (real driver, extended protocol).

Checks the closed limitations end to end:
  1. prepared statements (Parse/Bind/Describe/Execute) with parameters
  2. ALTER TABLE DROP/RENAME/MODIFY COLUMN + RENAME TO over the wire
  3. window frames + windows over GROUP BY + WITH RECURSIVE
  4. joins against a renamed table (index/PK probes)
"""
import pg8000.native
import sys

PORT = 15432
failures = []


def check(name, cond, extra=""):
    status = "OK " if cond else "FAIL"
    print(f"[{status}] {name}" + (f" — {extra}" if extra else ""))
    if not cond:
        failures.append(name)


con = pg8000.native.Connection(
    "anonymous", host="127.0.0.1", port=PORT, ssl_context=None, timeout=10
)

# 0. baseline DDL/DML
con.run("DROP TABLE IF EXISTS chr_live")
con.run("CREATE TABLE chr_live (id INT, name TEXT, amount FLOAT, PRIMARY KEY (id))")
con.run("INSERT INTO chr_live (id = 1, name = 'ada', amount = 10.5)")
con.run("INSERT INTO chr_live (id = 2, name = 'bob', amount = 20.0)")

# 1. prepared statements with parameters (extended protocol)
rows = con.run("SELECT name FROM chr_live WHERE id = :id", id=2)
check("prepared SELECT with param", rows == [["bob"]], str(rows))
con.run("INSERT INTO chr_live (id = :id, name = :name, amount = :a)", id=3, name="cyd", a=1.25)
rows = con.run("SELECT COUNT(*) FROM chr_live")
check("prepared INSERT with params", rows[0][0] == 3, str(rows))

# 2. ALTER TABLE family
con.run("ALTER TABLE chr_live ADD COLUMN note TEXT")
con.run("UPDATE chr_live SET note = 'x' WHERE id = 1")
con.run("ALTER TABLE chr_live RENAME COLUMN note TO memo")
rows = con.run("SELECT memo FROM chr_live WHERE id = 1")
check("RENAME COLUMN over wire", rows == [["x"]], str(rows))
con.run("UPDATE chr_live SET memo = '7'")  # numeric text everywhere first
con.run("ALTER TABLE chr_live MODIFY COLUMN memo INT")
rows = con.run("SELECT memo FROM chr_live WHERE id = 1")
check("MODIFY COLUMN over wire", rows == [[7]], str(rows))
con.run("ALTER TABLE chr_live DROP COLUMN memo")
rows = con.run("SELECT * FROM chr_live WHERE id = 1")
check(
    "DROP COLUMN over wire",
    len(rows) == 1 and len(rows[0]) == 3 and rows[0][2] == "ada",
    str(rows),
)
con.run("ALTER TABLE chr_live RENAME TO chr_live2")
rows = con.run("SELECT COUNT(*) FROM chr_live2")
check("RENAME TO over wire", rows[0][0] == 3, str(rows))

# 3. windows: frames, GROUP BY, recursive CTE
# NOTE: pure-ACGT strings are typed as DNA literals by the engine — use
# non-ACGT group labels so TEXT stays TEXT.
con.run("CREATE TABLE chr_w (k INT, g TEXT, v FLOAT, PRIMARY KEY (k))")
con.run("INSERT INTO chr_w (k = 1, g = 'x', v = 1.0)")
con.run("INSERT INTO chr_w (k = 2, g = 'x', v = 2.0)")
con.run("INSERT INTO chr_w (k = 3, g = 'y', v = 4.0)")
rows = con.run(
    "SELECT g, v, SUM(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) "
    "AS run FROM chr_w"
)
# Column order follows the engine's sorted row map — resolve by name.
names = [c["name"] for c in con.columns]
run_i = names.index("run")
check("window frame running total", [r[run_i] for r in rows] == [1.0, 3.0, 7.0], str(rows))
rows = con.run(
    "SELECT g, SUM(v) AS s, ROW_NUMBER() OVER (ORDER BY SUM(v) DESC) AS rnk "
    "FROM chr_w GROUP BY g"
)
# Column order follows the engine's sorted row map — resolve by name.
names = [c["name"] for c in con.columns]
gi, ri = names.index("g"), names.index("rnk")
by_g = {r[gi]: r[ri] for r in rows}
check("window over GROUP BY", by_g == {"y": 1, "x": 2}, str(rows))
rows = con.run(
    "WITH RECURSIVE seq AS (SELECT 1 AS n UNION ALL SELECT n + 1 AS n FROM seq WHERE n < 5) "
    "SELECT COUNT(*) FROM seq"
)
check("WITH RECURSIVE over wire", rows[0][0] == 5, str(rows))

# 4. join against the renamed table (PK probes, pre-DROP)
con.run("CREATE TABLE chr_o (oid INT, uid INT, PRIMARY KEY (oid))")
con.run("INSERT INTO chr_o (oid = 1, uid = 1)")
con.run("INSERT INTO chr_o (oid = 2, uid = 2)")
rows = con.run("SELECT name FROM chr_o o JOIN chr_live2 l ON l.id = o.uid")
check("join correctness after rename", sorted(r[0] for r in rows) == ["ada", "bob"], str(rows))

con.run("DROP TABLE chr_live2")
con.run("DROP TABLE chr_w")
con.run("DROP TABLE chr_o")

con.close()
print()
if failures:
    print(f"LIVE PGWIRE FAILED: {len(failures)} checks: {failures}")
    sys.exit(1)
print("LIVE PGWIRE OK — extended protocol + new DDL + windows + recursive CTE all work")
