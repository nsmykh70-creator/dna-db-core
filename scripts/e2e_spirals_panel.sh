#!/bin/bash
# E2E: спирали dnadb + веб-панель (Фаза 3, пункт 3).
#
# Сценарий:
#   1. dna-db-core mysqld поднимается с --admin-port (панель) и --user root:dnatest;
#   2. ПАНЕЛЬ: GET / → HTML; логин (верный/неверный токен/401 без токена);
#   3. ПАНЕЛЬ: POST /api/databases → база «shop»: спираль A на корне local-a,
#      спираль B — на сетевом хранилище (путь вручную) → на диске появляются
#      ДВЕ РЕАЛЬНЫЕ спирали (движком инициализированные каталоги с манифестами);
#   4. SQL: CREATE DATABASE webdb через wire-клиент mariadb → та же пара спиралей
#      на корнях по умолчанию; SHOW DATABASES видит обе базы; DROP убирает обе
#      спирали и каталог;
#   5. ХРАНИЛИЩА: GET /api/storage (корни + обнаруженные диски), POST /api/storage
#      (регистрация сетевого корня), перенос спирали B (relocate) между хранилищами;
#   6. ФИКС SUM: SELECT SUM(price) через mariadb-клиент печатает "250.10"
#      (регрессия из E2E_BACKUP_RESTORE_REPORT — дисплейная шкала агрегатов).
#
# Финал: SPIRALS_PANEL_E2E_VERDICT: OK.
#
# Переменные: DNA_BIN, MR, WORK.
set -u
DNA_BIN=${DNA_BIN:-/home/z/my-project/upload-staging/dna-db-core/target/release/dna-db-core}
MR=${MR:-/home/z/my-project/runtime/mariadb}
WORK=${WORK:-/tmp/spirals_e2e}
HERE="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$MR/lib:${LD_LIBRARY_PATH:-}"

fail() { echo "FATAL: $*"; tail -20 "$WORK"/*.log 2>/dev/null; exit 1; }

PANEL=http://127.0.0.1:13310
DNACLI="$MR/bin/mariadb --no-defaults -h 127.0.0.1 -P 13307 -u root -pdnatest"

rm -rf "$WORK"; mkdir -p "$WORK"
pkill -f "dna-db-core mysqld" 2>/dev/null; sleep 1

# ---------- 1) сервер с панелью ----------
setsid "$DNA_BIN" mysqld --data-dir "$WORK/dna-data" --port 13307 --bind 127.0.0.1 \
  --user root:dnatest --admin-port 13310 --admin-bind 127.0.0.1 \
  > "$WORK/dna_mysqld.log" 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 60); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" 2>/dev/null && break
  sleep 0.5
done
for i in $(seq 1 60); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13310),1);s.close()" 2>/dev/null && break
  sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" || fail "wire port down"
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13310),1);s.close()" || fail "panel port down"
echo "== mysqld up (wire :13307, panel :13310)"
grep "spiral catalog" "$WORK/dna_mysqld.log" || fail "spiral catalog log line missing"

# ---------- 2) панель: статика, логин, авторизация ----------
curl -s "$PANEL/" | grep -q "DNA-DB" || fail "panel HTML missing"
TOKEN=$(curl -s -X POST "$PANEL/api/login" -d '{"user":"root","pass":"dnatest"}' | python3 -c "import sys,json;print(json.load(sys.stdin)['token'])") || fail "login"
[ -n "$TOKEN" ] || fail "empty token"
curl -s -X POST "$PANEL/api/login" -d '{"user":"root","pass":"WRONG"}' | grep -q "bad credentials" || fail "wrong-pass not rejected"
curl -s "$PANEL/api/databases" | grep -q "unauthorized" || fail "unauthenticated request accepted"
AUTH="Authorization: Bearer $TOKEN"
echo "== panel: login ok, auth enforced"

# ---------- 3) панель создаёт базу: ДВЕ спирали, B — на сетевом хранилище ----------
NETROOT="$WORK/net-storage"; mkdir -p "$NETROOT"
R=$(curl -s -X POST "$PANEL/api/databases" -H "$AUTH" \
  -d "{\"name\":\"shop\",\"strand_a\":{\"label\":\"local-a\"},\"strand_b\":{\"path\":\"$NETROOT\",\"kind\":\"network\"}}")
echo "$R" | grep -q '"ok":true' || fail "panel create failed: $R"
A="$WORK/dna-data/spirals-a/shop.spiral"
B="$NETROOT/shop.spiral"
[ -d "$A" ] && [ -d "$B" ] || fail "two spiral dirs expected ($A, $B)"
[ -f "$A/spiral_manifest.json" ] && [ -f "$B/spiral_manifest.json" ] || fail "manifests missing"
[ "$(ls -1 "$A" | wc -l)" -gt 1 ] || fail "strand A is not an initialized engine dir"
python3 -c "
import json
a=json.load(open('$A/spiral_manifest.json')); b=json.load(open('$B/spiral_manifest.json'))
assert a['role']=='A' and b['role']=='B', 'roles wrong'
assert a['pair_id']==b['pair_id'], 'pair ids differ'
assert b['kind']=='network', 'strand B kind'
print('pair_id', a['pair_id'], 'A/B manifests consistent')
" || fail "manifest check"
echo "== panel: shop created — two spirals (A on local-a, B on network path)"

# ---------- 4) SQL-путь: CREATE DATABASE тоже разворачивает пару ----------
$DNACLI -e "CREATE DATABASE webdb" || fail "SQL CREATE DATABASE"
W_A="$WORK/dna-data/spirals-a/webdb.spiral"; W_B="$WORK/dna-data/spirals-b/webdb.spiral"
[ -d "$W_A" ] && [ -d "$W_B" ] || fail "SQL CREATE did not build spirals"
NAMES=$($DNACLI -N -e "SHOW DATABASES" | tr '\n' ' ')
echo "$NAMES" | grep -qw "webdb" || fail "SHOW DATABASES missing webdb: $NAMES"
echo "== sql: webdb created via wire — spirals at $W_A + $W_B; SHOW DATABASES = $NAMES"

# ---------- 5) хранилища: список, регистрация, перенос спирали ----------
curl -s "$PANEL/api/storage" -H "$AUTH" | grep -q '"roots"' || fail "storage list"
CANDS=$(curl -s "$PANEL/api/storage" -H "$AUTH" | python3 -c "import sys,json;print(len(json.load(sys.stdin)['candidates']))")
echo "== storage: candidates detected: $CANDS"
NAS="$WORK/nas"; mkdir -p "$NAS"
curl -s -X POST "$PANEL/api/storage" -H "$AUTH" \
  -d "{\"label\":\"nas\",\"path\":\"$NAS\",\"kind\":\"network\"}" | grep -q '"ok":true' || fail "add root"
R=$(curl -s -X POST "$PANEL/api/databases/shop/relocate" -H "$AUTH" \
  -d "{\"role\":\"B\",\"label\":\"nas\"}")
echo "$R" | grep -q '"ok":true' || fail "relocate failed: $R"
[ -d "$NAS/shop.spiral" ] && [ ! -d "$B" ] || fail "relocate did not move strand B"
python3 -c "
import json
m=json.load(open('$NAS/shop.spiral/spiral_manifest.json'))
assert m['root_label']=='nas' and m['role']=='B', 'relocated manifest wrong'
print('strand B relocated to nas, manifest updated')
" || fail "relocated manifest check"
curl -s -X DELETE "$PANEL/api/databases/shop" -H "$AUTH" | grep -q '"ok":true' || fail "panel drop"
[ ! -d "$A" ] && [ ! -d "$NAS/shop.spiral" ] || fail "panel drop left spirals behind"
echo "== storage: relocate ok, panel drop removed both spirals"

# SQL DROP тоже снимает пару с диска.
$DNACLI -e "DROP DATABASE webdb" || fail "SQL DROP DATABASE"
[ ! -d "$W_A" ] && [ ! -d "$W_B" ] || fail "SQL DROP left spirals behind"
echo "== sql: webdb dropped — spirals removed"

# ---------- 6) фикс SUM: дисплейная шкала агрегатов ----------
$DNACLI -e "CREATE DATABASE fin" || fail "fin create"
$DNACLI fin -e "CREATE TABLE items (id bigint(20) unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY, name varchar(50) NOT NULL DEFAULT '', price decimal(10,2) NOT NULL DEFAULT '0.00'); INSERT INTO items (name, price) VALUES ('кружка', 250.00), ('tea', 0.10)"
SUM=$($DNACLI fin -N -e "SELECT SUM(price) FROM fin.items" 2>/dev/null || $DNACLI -N fin -e "SELECT SUM(price) FROM items")
[ "$SUM" = "250.10" ] || fail "SUM scale: got '$SUM', want '250.10'"
echo "== sum fix: SELECT SUM(price) → $SUM (was '250.1' before the fix)"
$DNACLI -e "DROP DATABASE fin" >/dev/null 2>&1

kill "$DNA_PID" 2>/dev/null
echo "SPIRALS_PANEL_E2E_VERDICT: OK"
echo "== done =="
