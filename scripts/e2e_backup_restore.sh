#!/bin/bash
# E2E: бэкап/рестор CMS-базы на DNA-DB (Фаза 3, пункт 2 — «эксплуатация»).
#
# Сценарий оператора CMS-сайта на DNA-DB:
#   1. продакшен  — фиксстура (wp_posts/wp_postmeta/wp_term_rel) импортируется
#                    в DNA-DB mysqld (:13307, база wp_prod); эталон-копия той же
#                    фикстуры — на MariaDB (:13306, база wp_ref);
#   2. бэкап      — mariadb-dump снимает артефакт С DNA-DB (backup_wp_prod.sql);
#   3. дисастер   — DROP DATABASE wp_prod на DNA-DB (потеря продакшена);
#   4. рестор     — дамп проигрывается обратно в DNA-DB; затем повторный рестор
#                    поверх уже восстановленной базы (идемпотентность, DROP TABLE
#                    IF EXISTS внутри дампа);
#   5. верификация— содержимое wp_ref == wp_prod (compare), CMS-смоук через
#                    mysqli (cms_restore_smoke.php), автоинкремент продолжает
#                    с max+1 (probe);
#   6. портабельность — тот же бэкап восстанавливается в чистую MariaDB
#                    (база wp_back) и совпадает с эталоном: бэкап DNA-DB не
#                    замыкает на себя (no vendor lock-in).
#
# Критерий успеха: все вердикты FIXTURE_OK / COMPARE_MATCH / RESTORE_SMOKE_OK /
#   PROBE_OK, финал BACKUP_RESTORE_E2E_VERDICT: OK.
#
# Переменные: DNA_BIN, MR (mariadb-basedir), PHP, WORK (по умолчанию /tmp).
set -u
DNA_BIN=${DNA_BIN:-/home/z/my-project/upload-staging/dna-db-core/prebuilt/linux-x86_64/dna-db-core}
MR=${MR:-/home/z/my-project/runtime/mariadb}
PHP=${PHP:-/home/z/my-project/runtime/php83/php}
WORK=${WORK:-/tmp/backup_restore_e2e}
HERE="$(cd "$(dirname "$0")" && pwd)"
HARNESS="$HERE/mysqldump_roundtrip_harness.php"
SMOKE="$HERE/cms_restore_smoke.php"
MARIADB_DATA="$WORK/mariadb-data"
DNA_DATA="$WORK/dna-data"
BACKUP="$WORK/backup_wp_prod.sql"
DNA_USER=root
DNA_PASS=dnatest
MCLI="$MR/bin/mariadb --no-defaults -h 127.0.0.1 -P 13306 -u root"

mkdir -p "$WORK"
export LD_LIBRARY_PATH="$MR/lib:${LD_LIBRARY_PATH:-}"

fail() { echo "FATAL: $*"; tail -20 "$WORK"/*.log 2>/dev/null; exit 1; }

pkill -f mariadbd 2>/dev/null; pkill -f "dna-db-core mysqld" 2>/dev/null; sleep 1

# ---------- 1) эталон MariaDB (:13306) ----------
if [ ! -d "$MARIADB_DATA/mysql" ]; then
  rm -rf "$MARIADB_DATA"
  "$MR/bin/mariadb-install-db" --no-defaults --basedir="$MR" --datadir="$MARIADB_DATA" \
    --auth-root-authentication-method=normal --skip-test-db --skip-name-resolve > "$WORK/install_db.log" 2>&1 \
    || fail "mariadb-install-db: $(tail -5 "$WORK/install_db.log")"
fi
"$MR/bin/mariadbd" --no-defaults --datadir="$MARIADB_DATA" --port=13306 \
  --bind-address=127.0.0.1 --socket="$WORK/mariadb.sock" --skip-networking=0 \
  --sql_mode='' --innodb-buffer-pool-size=128M --skip-grant-tables \
  --lc-messages-dir="$MR/share/mysql" > "$WORK/mariadbd.log" 2>&1 < /dev/null &
MR_PID=$!
for i in $(seq 1 60); do
  $MCLI -e "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.5
done
$MCLI -e "SELECT 1" >/dev/null 2>&1 || fail "mariadb did not come up"
echo "== mariadb up (pid $MR_PID)"

# ---------- 2) продакшен: DNA-DB mysqld (:13307) ----------
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

# ---------- 3) фиксстура: эталон wp_ref, продакшен wp_prod ----------
$MCLI -e "DROP DATABASE IF EXISTS wp_ref; CREATE DATABASE wp_ref CHARACTER SET utf8mb4" || fail "wp_ref"
"$PHP" "$HARNESS" fixture 127.0.0.1 13306 wp_ref | tee "$WORK/fixture_ref.out"
grep -q FIXTURE_OK "$WORK/fixture_ref.out" || fail "fixture on ref"

DNACLI="$MR/bin/mariadb --no-defaults -h 127.0.0.1 -P 13307 -u $DNA_USER -p$DNA_PASS"
$DNACLI -e "CREATE DATABASE wp_prod" || fail "dna CREATE DATABASE wp_prod"
"$MR/bin/mariadb-dump" --no-defaults -h 127.0.0.1 -P 13306 -u root wp_ref \
  | $DNACLI wp_prod > "$WORK/provision.log" 2>&1 || fail "provision wp_prod: $(tail -5 "$WORK/provision.log")"
echo "== wp_prod provisioned on dna (migrated CMS site)"

"$PHP" "$HARNESS" compare 127.0.0.1 13306 wp_ref 127.0.0.1 13307 wp_prod "$DNA_USER" "$DNA_PASS" | tee "$WORK/compare_pre.out"
grep -q COMPARE_MATCH "$WORK/compare_pre.out" || fail "pre-disaster compare ref vs prod"

# ---------- 4) БЭКАП: mariadb-dump снимает артефакт с DNA-DB ----------
"$MR/bin/mariadb-dump" --no-defaults -h 127.0.0.1 -P 13307 -u "$DNA_USER" -p"$DNA_PASS" wp_prod > "$BACKUP" \
  || fail "mariadb-dump (backup from dna): $(head -5 "$BACKUP")"
echo "== backup = $(wc -c < "$BACKUP") bytes, $(grep -c '^INSERT INTO' "$BACKUP") insert stmts, $(grep -c '^CREATE TABLE' "$BACKUP") tables"

# ---------- 5) ДИСАСТЕР: потеря продакшена ----------
$DNACLI -e "DROP DATABASE wp_prod" || fail "disaster DROP DATABASE"
LOST=$($DNACLI -N -e "SHOW DATABASES LIKE 'wp_prod'" | wc -l)
[ "$LOST" = 0 ] || fail "wp_prod still present after DROP"
echo "== disaster: wp_prod dropped, production lost"

# ---------- 6) РЕСТОР: дамп проигрывается обратно в DNA-DB ----------
$DNACLI -e "CREATE DATABASE wp_prod" || fail "restore CREATE DATABASE"
$DNACLI wp_prod < "$BACKUP" > "$WORK/restore.log" 2>&1 || fail "restore into dna: $(tail -5 "$WORK/restore.log")"
echo "== restore pass 1: rc=0"

"$PHP" "$HARNESS" compare 127.0.0.1 13306 wp_ref 127.0.0.1 13307 wp_prod "$DNA_USER" "$DNA_PASS" | tee "$WORK/compare_restore1.out"
grep -q COMPARE_MATCH "$WORK/compare_restore1.out" || fail "compare after restore pass 1"

"$PHP" "$SMOKE" 127.0.0.1 13307 wp_prod "$DNA_USER" "$DNA_PASS" | tee "$WORK/smoke.out"
grep -q RESTORE_SMOKE_OK "$WORK/smoke.out" || fail "cms smoke after restore"

# ---------- 7) идемпотентность: повторный рестор поверх восстановленного ----------
$DNACLI wp_prod < "$BACKUP" > "$WORK/restore2.log" 2>&1 || fail "restore pass 2 over existing: $(tail -5 "$WORK/restore2.log")"
echo "== restore pass 2 (over restored db): rc=0"

"$PHP" "$HARNESS" compare 127.0.0.1 13306 wp_ref 127.0.0.1 13307 wp_prod "$DNA_USER" "$DNA_PASS" | tee "$WORK/compare_restore2.out"
grep -q COMPARE_MATCH "$WORK/compare_restore2.out" || fail "compare after restore pass 2"

# ---------- 8) автоинкремент продолжает с max+1 после рестора ----------
"$PHP" "$HARNESS" probe 127.0.0.1 13307 wp_prod "$DNA_USER" "$DNA_PASS" | tee "$WORK/probe.out"
grep -q PROBE_OK "$WORK/probe.out" || fail "auto-inc probe"
grep -q LID_OK "$WORK/probe.out" || fail "LAST_INSERT_ID probe"

# ---------- 9) портабельность: тот же бэкап встаёт на ванильную MariaDB ----------
$MCLI -e "DROP DATABASE IF EXISTS wp_back; CREATE DATABASE wp_back CHARACTER SET utf8mb4" || fail "wp_back"
$MCLI wp_back < "$BACKUP" > "$WORK/back_import.log" 2>&1 || fail "restore into vanilla mariadb: $(tail -5 "$WORK/back_import.log")"

"$PHP" "$HARNESS" compare 127.0.0.1 13306 wp_ref 127.0.0.1 13306 wp_back root "" | tee "$WORK/compare_port.out"
grep -q COMPARE_MATCH "$WORK/compare_port.out" || fail "compare ref vs restored-on-mariadb"

kill "$MR_PID" "$DNA_PID" 2>/dev/null
echo "BACKUP_RESTORE_E2E_VERDICT: OK"
echo "== done =="
