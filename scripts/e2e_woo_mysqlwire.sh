#!/bin/bash
# E2E: WooCommerce 11.2.1 (HPOS) через УНИВЕРСАЛЬНЫЙ MySQL wire-адаптер DNA-DB.
# WordPress устанавливается штатным mysqli на dna-db-core mysqld (:13307),
# затем плагин WooCommerce ставится из локального zip и активируется —
# dbDelta создаёт ~40 таблиц Woo через wire. Эталон: MariaDB (:13306).
# Батарея: scripts/e2e_woo_battery.php (один и тот же код на обеих сторонах).
set -u
DNA_BIN=${DNA_BIN:-/home/z/my-project/upload-staging/dna-db-core/target/release/dna-db-core}
WOO_ZIP=${WOO_ZIP:-/home/z/woocommerce.zip}
MR=/home/z/mariadb-root
export LD_LIBRARY_PATH="$MR/usr/lib/x86_64-linux-gnu:/home/z/php-root/usr/lib/x86_64-linux-gnu"
PHP=/home/z/php-root/usr/bin/php8.4
PHAR=/home/z/wp-cli.phar
wp() { "$PHP" -c /home/z/php-ini/dna.ini "$PHAR" "$@"; }
WT=/tmp/wptest
DNA_DATA=/tmp/dna_mysqlwire_woo
MARIADB_DATA=/home/z/mariadb-data
BATTERY=/home/z/my-project/upload-staging/dna-db-core/scripts/e2e_woo_battery.php

rm -rf "$DNA_DATA" "$WT/woo-ref" "$WT/woo-dna"
mkdir -p "$WT/woo-ref" "$WT/woo-dna"
cp -r /tmp/wptest/wordpress/. "$WT/woo-ref/"
cp -r /tmp/wptest/wordpress/. "$WT/woo-dna/"

# 1) эталон MariaDB
pkill -f mariadbd 2>/dev/null; pkill -f "dna-db-core mysqld" 2>/dev/null; sleep 1
"$MR/sbin/mariadbd" --no-defaults --datadir="$MARIADB_DATA" --port=13306 \
  --bind-address=127.0.0.1 --socket=/tmp/mariadb-e2e.sock --skip-networking=0 \
  --sql_mode='' --innodb-buffer-pool-size=256M \
  --lc-messages-dir="$MR/usr/share/mysql" > /tmp/mariadbd_woo.log 2>&1 < /dev/null &
MR_PID=$!
for i in $(seq 1 30); do "$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root -e "SELECT 1" >/dev/null 2>&1 && break; sleep 0.5; done
"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root \
  -e "DROP DATABASE IF EXISTS wooref; CREATE DATABASE wooref CHARACTER SET utf8mb4;" || { echo "FATAL: mariadb"; exit 1; }
echo "== mariadb up = $MR_PID"

# 2) DNA-DB mysqld
setsid "$DNA_BIN" mysqld --data-dir "$DNA_DATA" --port 13307 --bind 127.0.0.1 \
  --user wp:wppass > /tmp/dna_mysqld_woo.log 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 30); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" 2>/dev/null && break; sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),2);s.close()" 2>/dev/null || { echo "FATAL: dna mysqld"; tail -5 /tmp/dna_mysqld_woo.log; exit 1; }
echo "== dna mysqld up = $DNA_PID"

# 3) wp-config.php (одинаковые соли)
SALTS="define('AUTH_KEY','dna-e2e-fixed-key-0123456789');
define('SECURE_AUTH_KEY','dna-e2e-fixed-key-0123456789');
define('LOGGED_IN_KEY','dna-e2e-fixed-key-0123456789');
define('NONCE_KEY','dna-e2e-fixed-key-0123456789');
define('AUTH_SALT','dna-e2e-fixed-salt-0123456789');
define('SECURE_AUTH_SALT','dna-e2e-fixed-salt-0123456789');
define('LOGGED_IN_SALT','dna-e2e-fixed-salt-0123456789');
define('NONCE_SALT','dna-e2e-fixed-salt-0123456789');"
mkcfg() { # $1=dir $2=user $3=pass $4=host $5=db
cat > "$WT/$1/wp-config.php" <<EOF
<?php
define( 'DB_NAME', '$5' );
define( 'DB_USER', '$2' );
define( 'DB_PASSWORD', '$3' );
define( 'DB_HOST', '$4' );
define( 'DB_CHARSET', 'utf8mb4' );
define( 'DB_COLLATE', '' );
$SALTS
\$table_prefix = 'wp_';
define( 'WP_DEBUG', false );
define( 'WP_HOME', 'http://localhost:8091' );
define( 'WP_SITEURL', 'http://localhost:8091' );
define( 'WP_HTTP_BLOCK_EXTERNAL', true );
define( 'WP_ACCESSIBLE_HOSTS', '127.0.0.1, localhost' );
if ( ! defined( 'ABSPATH' ) ) define( 'ABSPATH', __DIR__ . '/' );
require_once ABSPATH . 'wp-settings.php';
EOF
}
mkcfg woo-ref root "" "127.0.0.1:13306" wooref
mkcfg woo-dna wp wppass "127.0.0.1:13307" wordpress

# 4) установка WP + активация Woo на обеих сторонах
setup() { # $1=dir $2=log
  wp --path="$WT/$1" core install --url="http://localhost:8091" \
    --title="Woo E2E" --admin_user=admin --admin_password=dnatest123 \
    --admin_email=admin@example.com --skip-email > "$2" 2>&1
  local rc=$?
  wp --path="$WT/$1" plugin install "$WOO_ZIP" --activate >> "$2" 2>&1
  local rc2=$?
  echo "core=$rc woo=$rc2"
}
echo "REF: $(setup woo-ref /tmp/woo_ref_install.log)"
echo "DNA: $(setup woo-dna /tmp/woo_dna_install.log)"
echo "=== DNA install tail ==="; tail -5 /tmp/woo_dna_install.log

# 5) батарея заказа
run_battery() { # $1=dir $2=out
  wp --path="$WT/$1" eval-file "$BATTERY" > "$2" 2>&1
  echo "rc=$?"
}
echo "REF battery: $(run_battery woo-ref /tmp/woo_ref_battery.json)"
echo "DNA battery: $(run_battery woo-dna /tmp/woo_dna_battery.json)"
echo "--- REF JSON ---"; cat /tmp/woo_ref_battery.json
echo "--- DNA JSON ---"; cat /tmp/woo_dna_battery.json

# 6) вердикт (тайминги исключаем)
python3 - <<'PYEOF'
import json
def load(p):
    for line in open(p):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    return {}
ref, dna = load("/tmp/woo_ref_battery.json"), load("/tmp/woo_dna_battery.json")
timings = {k for k in ref if k.startswith("t_")}
mism = [(k, ref.get(k), dna.get(k)) for k in ref if k not in timings and ref.get(k) != dna.get(k)]
print(f"artifacts={len(ref)-len(timings)} mismatches={len(mism)}")
for k, r, d in mism:
    print(f"  {k}: ref={r!r} dna={d!r}")
print("E2E_WOO_MYSQLWIRE_VERDICT:", "MATCH" if not mism and ref else "DIFF")
PYEOF

# 7) таблицы Woo на стороне DNA
python3 - <<'PYEOF'
import pymysql
c = pymysql.connect(host='127.0.0.1', port=13307, user='wp', password='wppass', database='wordpress')
cur = c.cursor(); cur.execute("SHOW TABLES")
tables = [r[0] for r in cur.fetchall()]
woo = [t for t in tables if 'wc_' in t or 'woocommerce' in t]
print(f"total_tables={len(tables)} woo_tables={len(woo)}")
print("sample:", sorted(woo)[:8])
c.close()
PYEOF
kill $MR_PID $DNA_PID 2>/dev/null
echo "== done =="
