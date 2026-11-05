#!/bin/bash
# E2E: mysqldump / mariadb-dump roundtrip против DNA-DB (Фаза 3, пункт 1).
#
# Направление 1 (IMPORT): mariadb-dump снимает дамп с эталона MariaDB (:13306),
#   клиент mariadb проигрывает его в DNA-DB mysqld (:13307) через wire-гейт.
# Направление 2 (EXPORT): mariadb-dump снимает дамп С DNA-DB (SHOW CREATE TABLE,
#   SELECT SQL_NO_CACHE, LOCK TABLES — реальный дампер как клиент), дамп
#   проигрывается в чистую базу MariaDB.
# Критерий: содержимое (все строки/колонки/типы) трёх баз совпадает побайтно —
#   ref == imp == back; автоинкремент продолжается с max+1.
#
# Переменные: DNA_BIN, MR (mariadb-root), PHP, WORK (по умолчанию /tmp).
set -u
DNA_BIN=${DNA_BIN:-/home/z/my-project/upload-staging/dna-db-core/target/release/dna-db-core}
MR=${MR:-/home/z/my-project/runtime/mariadb}
PHP=${PHP:-/home/z/my-project/runtime/php83/php}
WORK=${WORK:-/tmp/mysqldump_e2e}
HERE="$(cd "$(dirname "$0")" && pwd)"
HARNESS="$HERE/mysqldump_roundtrip_harness.php"
MARIADB_DATA="$WORK/mariadb-data"
DNA_DATA="$WORK/dna-data"
DUMP_REF="$WORK/dna_ref_dump.sql"
DUMP_DNA="$WORK/dna_imp_dump.sql"
DNA_USER=root
DNA_PASS=dnatest

mkdir -p "$WORK"
export LD_LIBRARY_PATH="$MR/lib:${LD_LIBRARY_PATH:-}"

fail() { echo "FATAL: $*"; tail -20 "$WORK"/*.log 2>/dev/null; exit 1; }

pkill -f mariadbd 2>/dev/null; pkill -f "dna-db-core mysqld" 2>/dev/null; sleep 1

# ---------- 1) эталон MariaDB ----------
if [ ! -d "$MARIADB_DATA/mysql" ]; then
  rm -rf "$MARIADB_DATA"
  "$MR/scripts/mariadb-install-db" --no-defaults --basedir="$MR" --datadir="$MARIADB_DATA" \
    --auth-root-authentication-method=normal --skip-test-db --skip-name-resolve > "$WORK/install_db.log" 2>&1 \
    || fail "mariadb-install-db: $(tail -5 "$WORK/install_db.log")"
fi
"$MR/bin/mariadbd" --no-defaults --datadir="$MARIADB_DATA" --port=13306 \
  --bind-address=127.0.0.1 --socket="$WORK/mariadb.sock" --skip-networking=0 \
  --sql_mode='' --innodb-buffer-pool-size=128M --skip-grant-tables \
  --lc-messages-dir="$MR/share/mysql" > "$WORK/mariadbd.log" 2>&1 < /dev/null &
MR_PID=$!
for i in $(seq 1 60); do
  "$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root -e "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.5
done
"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root \
  -e "CREATE DATABASE IF NOT EXISTS dna_ref CHARACTER SET utf8mb4" || fail "mariadb refused"
echo "== mariadb up (pid $MR_PID)"

# ---------- 2) DNA-DB mysqld ----------
rm -rf "$DNA_DATA"
setsid "$DNA_BIN" mysqld --data-dir "$DNA_DATA" --port 13307 --bind 127.0.0.1 \
  --user "$DNA_USER:$DNA_PASS" > "$WORK/dna_mysqld.log" 2>&1 < /dev/null &
DNA_PID=$!
DNA_UP=0
for i in $(seq 1 60); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" 2>/dev/null && { DNA_UP=1; break; }
  sleep 0.5
done
[ "$DNA_UP" = 1 ] || fail "dna mysqld did not come up"
echo "== dna mysqld up (pid $DNA_PID)"

# ---------- 3) фикстура на эталоне ----------
"$PHP" "$HARNESS" fixture 127.0.0.1 13306 dna_ref | tee "$WORK/fixture.out"
grep -q FIXTURE_OK "$WORK/fixture.out" || fail "fixture"

# ---------- 4) IMPORT: mariadb-dump с эталона → клиент mariadb → DNA-DB ----------
"$MR/bin/mariadb-dump" --no-defaults -h 127.0.0.1 -P 13306 -u root dna_ref > "$DUMP_REF" \
  || fail "mariadb-dump (ref): $(head -5 "$DUMP_REF")"
echo "== dump(ref) = $(wc -c < "$DUMP_REF") bytes, $(grep -c '^INSERT INTO' "$DUMP_REF") insert stmts"

"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13307 -u "$DNA_USER" -p"$DNA_PASS" \
  -e "CREATE DATABASE dna_imp" || fail "dna CREATE DATABASE"
"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13307 -u "$DNA_USER" -p"$DNA_PASS" dna_imp \
  < "$DUMP_REF" > "$WORK/import.log" 2>&1 || fail "import into dna: $(tail -5 "$WORK/import.log")"
echo "== import into dna: client rc=0"

# ---------- 5) сверка ref == imp ----------
"$PHP" "$HARNESS" compare 127.0.0.1 13306 dna_ref 127.0.0.1 13307 dna_imp "$DNA_USER" "$DNA_PASS" | tee "$WORK/compare_imp.out"
grep -q COMPARE_MATCH "$WORK/compare_imp.out" || fail "compare ref vs imp"

# ---------- 6) EXPORT: mariadb-dump с DNA-DB → чистая база MariaDB ----------
"$MR/bin/mariadb-dump" --no-defaults -h 127.0.0.1 -P 13307 -u "$DNA_USER" -p"$DNA_PASS" dna_imp > "$DUMP_DNA" \
  || fail "mariadb-dump (dna): $(head -5 "$DUMP_DNA")"
echo "== dump(dna) = $(wc -c < "$DUMP_DNA") bytes, $(grep -c '^CREATE TABLE' "$DUMP_DNA") create tables"

"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root \
  -e "CREATE DATABASE IF NOT EXISTS dna_back CHARACTER SET utf8mb4" || fail "back db"
"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root dna_back \
  < "$DUMP_DNA" > "$WORK/back_import.log" 2>&1 || fail "import into mariadb: $(tail -5 "$WORK/back_import.log")"

"$PHP" "$HARNESS" compare 127.0.0.1 13306 dna_ref 127.0.0.1 13306 dna_back root "" | tee "$WORK/compare_back.out"
grep -q COMPARE_MATCH "$WORK/compare_back.out" || fail "compare ref vs back"

# ---------- 7) автоинкремент продолжается с max+1 ----------
"$PHP" "$HARNESS" probe 127.0.0.1 13307 dna_imp "$DNA_USER" "$DNA_PASS" | tee "$WORK/probe.out"
grep -q PROBE_OK "$WORK/probe.out" || fail "auto-inc probe"

kill "$MR_PID" "$DNA_PID" 2>/dev/null
echo "MYSQLDUMP_E2E_VERDICT: OK"
echo "== done =="
