#!/bin/bash
# E2E: Drupal 11 ставится ШТАТНЫМ web-установщиком (core/install.php) на DNA-DB
# через универсальный MySQL wire-адаптер (mysqld, :13307). Никакого drop-in —
# штатный PDO MySQL внутри Drupal ходит на dna-db-core как на обычный MySQL.
# Схема прогона (один вызов инструмента): старт DNA-DB (setsid) -> ожидание
# порта (python3-socket, НЕ /dev/tcp) -> php -S с docroot Drupal ->
# scripts/drupal_e2e.php водит установщик по формам -> верификация.
set -u
ROOT=${ROOT:-/home/z/my-project/upload-staging/dna-db-core}
DNA_BIN=${DNA_BIN:-$ROOT/prebuilt/linux-x86_64/dna-db-core}
PHP=${PHP_BIN:-/home/z/my-project/cms-e2e/php83/php}
DP=${DP_DIR:-/home/z/my-project/cms-e2e/sites/drupal-11.4.10}
DRIVER=${DRIVER:-$ROOT/scripts/drupal_e2e.php}
PORT=${DP_E2E_PORT:-13307}
WEB=8088
DBNAME=drupal11
DATA=/tmp/drupal_dna_data

fail() {
  echo "DRUPAL_E2E_FAILED: $*"
  echo "--- server alive? ---"; pgrep -af 'dna-db-core' || echo "SERVER DEAD"
  echo "--- server log tail ---"; tail -30 /tmp/drupal_dna.log
  kill $DNA_PID $PHP_S_PID 2>/dev/null; exit 1
}
ok() { echo "[ok] $*"; }

# 0) чистый старт DNA-DB wire-сервера
pkill -f 'dna-db-core mysqld' 2>/dev/null; sleep 1
rm -rf "$DATA"
setsid "$DNA_BIN" mysqld --data-dir "$DATA" --port $PORT --bind 127.0.0.1 \
  --user root:dnatest > /tmp/drupal_dna.log 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 60); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',$PORT),1);s.close()" 2>/dev/null && break
  sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',$PORT),2);s.close()" 2>/dev/null \
  || { echo "FATAL: dna mysqld not listening"; tail -20 /tmp/drupal_dna.log; exit 1; }
ok "dna-db-core mysqld up on :$PORT (pid $DNA_PID)"

# 1) права для установщика (sites/default должен быть доступен на запись)
chmod -R u+w "$DP" 2>/dev/null
mkdir -p "$DP/sites/default/files"
chmod -R a+w "$DP/sites/default" "$DP/sites/default/files" 2>/dev/null
rm -f "$DP/sites/default/settings.php"

# 2) веб-сервер (PHP built-in) поверх docroot Drupal
"$PHP" -S 127.0.0.1:$WEB -t "$DP" > /tmp/drupal_php_s.log 2>&1 < /dev/null &
PHP_S_PID=$!
sleep 1
curl -s -o /dev/null -w '' "http://127.0.0.1:$WEB/core/install.php" || true

# 3) штатный web-установщик под управлением драйвера
"$PHP" "$DRIVER" "http://127.0.0.1:$WEB" "$PORT" 2>&1 | sed 's/^/  /'
RC=${PIPESTATUS[0]}
kill $PHP_S_PID 2>/dev/null
[ "$RC" = "0" ] || fail "drupal_e2e.php rc=$RC"

# 4) вердикт
kill $DNA_PID 2>/dev/null
echo "DRUPAL_E2E_OK"
