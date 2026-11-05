#!/bin/bash
# E2E: MediaWiki 1.43 устанавливается штатным CLI-установщиком на DNA-DB
# через универсальный MySQL wire-адаптер (mysqld, :13306). Никакого drop-in —
# stock mysqli внутри MediaWiki ходит на dna-db-core как на обычный MySQL.
# Схема прогона (один вызов инструмента): старт DNA-DB (setsid) -> ожидание
# порта (python3-socket, НЕ /dev/tcp) -> maintenance/install.php -> php -S ->
# рендер Main_Page -> правка через maintenance/edit.php -> API -> проверки БД.
set -u
ROOT=${ROOT:-/home/z/my-project/upload-staging/dna-db-core}
DNA_BIN=${DNA_BIN:-$ROOT/prebuilt/linux-x86_64/dna-db-core}
PHP=${PHP_BIN:-/home/z/my-project/cms-e2e/php83/php}
MW=${MW_DIR:-/home/z/my-project/cms-e2e/sites/mediawiki-1.43.11}
PORT=${MW_E2E_PORT:-13306}
BASE=http://127.0.0.1:8080
DBNAME=wikidna
ADMIN=dnadmin
ADMINPASS='DnaDb!MW2026x'
DATA=/tmp/mw_dna_data

fail() {
  echo "MEDIAWIKI_E2E_FAILED: $*"
  echo "--- server alive? ---"; pgrep -af 'dna-db-core' || echo "SERVER DEAD"
  echo "--- server log tail ---"; tail -30 /tmp/mw_dna.log
  kill $DNA_PID 2>/dev/null; exit 1
}
ok() { echo "[ok] $*"; }

# 0) чистый старт DNA-DB wire-сервера
pkill -f 'dna-db-core mysqld' 2>/dev/null; sleep 1
rm -rf "$DATA"
setsid "$DNA_BIN" mysqld --data-dir "$DATA" --port $PORT --bind 127.0.0.1 \
  --user root:dnatest > /tmp/mw_dna.log 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 60); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',$PORT),1);s.close()" 2>/dev/null && break
  sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',$PORT),2);s.close()" 2>/dev/null \
  || { echo "FATAL: dna mysqld not listening"; tail -20 /tmp/mw_dna.log; exit 1; }
ok "dna-db-core mysqld up on :$PORT (pid $DNA_PID)"
kill -0 $DNA_PID 2>/dev/null || { echo "FATAL: server dead right after port check"; exit 1; }
"$PHP" -r '$m=@new mysqli("127.0.0.1","root","dnatest","",'"$PORT"'); if($m->connect_errno){fwrite(STDERR,"probe: {$m->connect_error}\n");exit(1);} echo "[ok] mysqli probe: {$m->server_info}\n";' \
  || { echo "FATAL: mysqli probe failed"; exit 1; }
sleep 5
kill -0 $DNA_PID 2>/dev/null || { echo "FATAL: server died during idle window"; tail -20 /tmp/mw_dna.log; exit 1; }
ok "server survived probe + 5s idle window"
cd "$MW" || { echo "FATAL: no $MW"; exit 1; }
rm -f LocalSettings.php
"$PHP" maintenance/install.php \
  --dbtype mysql --dbserver "127.0.0.1:$PORT" \
  --dbname $DBNAME --dbuser root --dbpass dnatest \
  --installdbuser root --installdbpass dnatest \
  --dbprefix mwd_ \
  --scriptpath '' \
  --pass "$ADMINPASS" \
  "DNA-DB Wiki" $ADMIN 2>&1 | tail -12
RC=${PIPESTATUS[0]}
[ "$RC" = "0" ] || fail "install.php rc=$RC"
[ -f LocalSettings.php ] || fail "LocalSettings.php not written"
ok "maintenance/install.php rc=0, LocalSettings.php written"

# 2) веб-сервер (PHP built-in) и рендер вики
"$PHP" -S 127.0.0.1:8080 -t "$MW" > /tmp/mw_php_s.log 2>&1 < /dev/null &
PHP_S_PID=$!
sleep 1
HOMEPAGE=$(curl -s -o /tmp/mw_home.html -w '%{http_code}' "$BASE/index.php/Main_Page")
[ "$HOMEPAGE" = "200" ] || fail "Main_Page HTTP $HOMEPAGE"
grep -q 'DNA-DB Wiki' /tmp/mw_home.html || fail "Main_Page: sitename not found in HTML"
ok "GET /index.php/Main_Page -> 200, sitename rendered"

# 3) правка страницы штатным maintenance/edit.php (запись через wire)
EDIT_TEXT="DNA-DB wire E2E edit $(date +%s)"
echo "$EDIT_TEXT" | "$PHP" maintenance/run.php edit --user $ADMIN --summary 'DNA-DB E2E edit' 'Main_Page' 2>&1 | tail -3
[ "${PIPESTATUS[0]}" = "0" ] || fail "edit.php rc"
sleep 1
curl -s "$BASE/index.php/Main_Page?action=purge" -o /tmp/mw_home2.html
curl -s "$BASE/index.php?title=Main_Page&action=raw" -o /tmp/mw_raw.txt
grep -q "DNA-DB wire E2E edit" /tmp/mw_raw.txt || fail "edit not visible via action=raw"
ok "edit via maintenance/edit.php visible through action=raw"

# 4) API: siteinfo (general -> sitename) + статистика (edits через wire)
API=$(curl -s "$BASE/api.php?action=query&meta=siteinfo&siprop=general%7Cstatistics&format=json")
echo "$API" | grep -q '"sitename":"DNA-DB Wiki"' || fail "api.php siteinfo broken: $(echo "$API" | head -c 200)"
echo "$API" | grep -q '"edits":' || fail "api.php statistics missing edits: $(echo "$API" | head -c 200)"
ok "api.php siteinfo+statistics OK: $(echo "$API" | head -c 160)…"

# 5) уровень БД через wire (mysqli): схема MW 1.43 (MCR: slots/content/text)
"$PHP" -r '
$m = new mysqli("127.0.0.1", "root", "dnatest", "wikidna", '"$PORT"');
if ($m->connect_errno) { fwrite(STDERR, "mysqli: {$m->connect_error}\n"); exit(1); }
$t = 0; $rt = $m->query("SHOW TABLES"); while ($rt->fetch_row()) $t++;
$p = $m->query("SELECT page_id, page_latest FROM mwd_page WHERE page_title = \x27Main_Page\x27")->fetch_assoc();
$last = $m->query("SELECT r.rev_id, r.rev_len, c.content_address FROM mwd_revision r JOIN mwd_slots s ON s.slot_revision_id = r.rev_id JOIN mwd_content c ON c.content_id = s.slot_content_id WHERE r.rev_page = {$p["page_id"]} ORDER BY r.rev_id DESC LIMIT 1")->fetch_assoc();
$oldId = (int)substr($last["content_address"], 3);
$txt = $m->query("SELECT old_text FROM mwd_text WHERE old_id = $oldId")->fetch_assoc()["old_text"];
$rc = $m->query("SELECT COUNT(*) AS c FROM mwd_recentchanges")->fetch_assoc()["c"];
printf("DB: tables=%s page_id=%s page_latest=%s last_rev=%s rev_len=%s recentchanges=%s\n", $t, $p["page_id"], $p["page_latest"], $last["rev_id"], $last["rev_len"], $rc);
if ((int)$t < 40) exit(1);
if ((int)$p["page_latest"] !== (int)$last["rev_id"]) exit(3);
if (strpos($txt ?? "", "DNA-DB wire E2E edit") === false) exit(2);
' || fail "DB-level checks"
ok "DB: 1.43 schema chain page->revision->slots->content->text consistent through wire"

# 6) вердикт
kill $PHP_S_PID $DNA_PID 2>/dev/null
echo "MEDIAWIKI_E2E_OK"
