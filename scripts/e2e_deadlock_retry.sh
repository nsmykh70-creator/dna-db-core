#!/bin/bash
# E2E: deadlock (1213) + Woo-style retry over the DNA-DB MySQL wire adapter.
# Boots dna-db-core mysqld, runs scripts/e2e_deadlock_retry.php (both phases:
# worker-victim and main-victim), verifies the artifacts, prints the verdict.
set -u
ROOT=/home/z/my-project/upload-staging/dna-db-core
DNA_BIN=${DNA_BIN:-$ROOT/prebuilt/linux-x86_64/dna-db-core}
PHP=${PHP:-/home/z/my-project/runtime/php83/php}
DNA_DATA=/tmp/dna_deadlock_retry

pkill -f "dna-db-core mysqld" 2>/dev/null
sleep 1
rm -rf "$DNA_DATA"
rm -f /tmp/drl_p*.* /tmp/drl_worker_p*.log

setsid "$DNA_BIN" mysqld --data-dir "$DNA_DATA" --port 13307 --bind 127.0.0.1 \
  --user wp:wppass > /tmp/dna_deadlock_retry.log 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 40); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" 2>/dev/null && break
  sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),2);s.close()" 2>/dev/null \
  || { echo "FATAL: dna mysqld did not start"; tail -5 /tmp/dna_deadlock_retry.log; exit 1; }
echo "== dna mysqld up (pid $DNA_PID) =="

"$PHP" "$ROOT/scripts/e2e_deadlock_retry.php" main
RC=$?

kill "$DNA_PID" 2>/dev/null
echo "== done rc=$RC =="
exit $RC
