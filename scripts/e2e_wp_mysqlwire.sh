#!/bin/bash
# E2E: WordPress 7.1.4 через УНИВЕРСАЛЬНЫЙ MySQL wire-адаптер DNA-DB (mysqld, :13307).
# В отличие от scripts/e2e_dna_install.sh здесь НЕТ drop-in db.php — WordPress
# штатным mysqli подключается к dna-db-core mysqld как к обычному MySQL-серверу.
# Эталон: MariaDB (bintar без root) на :13306. Одна батарея wp-cli на обеих сторонах.
# Серверы живут только внутри этого вызова (платформа чистит фоновые процессы).
set -u
DNA_BIN=${DNA_BIN:-/home/z/my-project/upload-staging/dna-db-core/target/release/dna-db-core}
MR=/home/z/mariadb-root
export LD_LIBRARY_PATH="$MR/usr/lib/x86_64-linux-gnu:/home/z/php-root/usr/lib/x86_64-linux-gnu"
PHP=/home/z/php-root/usr/bin/php8.4
PHAR=/home/z/wp-cli.phar
wp() { "$PHP" -c /home/z/php-ini/dna.ini "$PHAR" "$@"; }
WT=/tmp/wptest
DNA_DATA=/tmp/dna_mysqlwire_e2e
MARIADB_DATA=/home/z/mariadb-data

# 0) чистые стороны
rm -rf "$DNA_DATA" "$WT/wp-ref" "$WT/wp-dna"
mkdir -p "$WT/wp-ref" "$WT/wp-dna"
cp -r /tmp/wptest/wordpress/. "$WT/wp-ref/"
cp -r /tmp/wptest/wordpress/. "$WT/wp-dna/"

# 1) эталон MariaDB
pkill -f mariadbd 2>/dev/null; sleep 1
"$MR/sbin/mariadbd" --no-defaults --datadir="$MARIADB_DATA" --port=13306 \
  --bind-address=127.0.0.1 --socket=/tmp/mariadb-e2e.sock --skip-networking=0 \
  --sql_mode='' --innodb-buffer-pool-size=256M \
  --lc-messages-dir="$MR/usr/share/mysql" > /tmp/mariadbd_e2e.log 2>&1 < /dev/null &
MR_PID=$!
for i in $(seq 1 30); do "$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root -e "SELECT 1" >/dev/null 2>&1 && break; sleep 0.5; done
"$MR/bin/mariadb" --no-defaults -h 127.0.0.1 -P 13306 -u root \
  -e "DROP DATABASE IF EXISTS wpref; CREATE DATABASE wpref CHARACTER SET utf8mb4;" || { echo "FATAL: mariadb"; exit 1; }
echo "== mariadb up = $MR_PID"

# 2) DNA-DB mysqld (mysql wire 4.1) — таблицы создаёт сам wp-cli через dbDelta
setsid "$DNA_BIN" mysqld --data-dir "$DNA_DATA" --port 13307 --bind 127.0.0.1 \
  --user wp:wppass > /tmp/dna_mysqld_e2e.log 2>&1 < /dev/null &
DNA_PID=$!
for i in $(seq 1 30); do
  python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),1);s.close()" 2>/dev/null && break; sleep 0.5
done
python3 -c "import socket;s=socket.create_connection(('127.0.0.1',13307),2);s.close()" 2>/dev/null || { echo "FATAL: dna mysqld"; tail -5 /tmp/dna_mysqld_e2e.log; exit 1; }
echo "== dna mysqld up = $DNA_PID"

# 3) wp-config.php — ОДИНАКОВЫЕ соли на обеих сторонах (детерминированный diff)
SALTS="define('AUTH_KEY','dna-e2e-fixed-key-0123456789');
define('SECURE_AUTH_KEY','dna-e2e-fixed-key-0123456789');
define('LOGGED_IN_KEY','dna-e2e-fixed-key-0123456789');
define('NONCE_KEY','dna-e2e-fixed-key-0123456789');
define('AUTH_SALT','dna-e2e-fixed-salt-0123456789');
define('SECURE_AUTH_SALT','dna-e2e-fixed-salt-0123456789');
define('LOGGED_IN_SALT','dna-e2e-fixed-salt-0123456789');
define('NONCE_SALT','dna-e2e-fixed-salt-0123456789');"
mkcfg() { # $1=dir $2=name $3=user $4=pass $5=host $6=db
cat > "$WT/$1/wp-config.php" <<EOF
<?php
define( 'DB_NAME', '$6' );
define( 'DB_USER', '$3' );
define( 'DB_PASSWORD', '$4' );
define( 'DB_HOST', '$5' );
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
mkcfg wp-ref dummy root "" "127.0.0.1:13306" wpref
mkcfg wp-dna dummy wp wppass "127.0.0.1:13307" wordpress

# 4) одинаковая установка на обеих сторонах
run_install() { # $1=dir $2=log
  wp --path="$WT/$1" core install --url="http://localhost:8091" \
    --title="WP E2E" --admin_user=admin --admin_password=dnatest123 \
    --admin_email=admin@example.com --skip-email > "$2" 2>&1
  echo "RC=$?"
}
RC_REF=$(run_install wp-ref /tmp/wp_ref_install.log)
RC_DNA=$(run_install wp-dna /tmp/wp_dna_install.log)
echo "INSTALL_REF=$RC_REF INSTALL_DNA=$RC_DNA"
[ "$RC_DNA" = "RC=0" ] || { echo "=== DNA install log ==="; tail -15 /tmp/wp_dna_install.log; }

# 5) батарея wp-cli (идентична на обеих сторонах)
battery() { # $1=dir $2=out
  cd "$WT/$1"
  {
  echo "VERSION: $(wp core version 2>&1)"
  echo "TABLES: $(wp eval 'global $wpdb; echo implode(",", $wpdb->get_col("SHOW TABLES"));' 2>&1)"
  echo "SITEURL: $(wp option get siteurl 2>&1)"
  echo "BLOGNAME: $(wp option get blogname 2>&1)"
  echo "OPT_COUNT: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_options");' 2>&1)"
  echo "POSTS: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_posts");' 2>&1)"
  echo "POSTMETA: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_postmeta");' 2>&1)"
  echo "USERS: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_users");' 2>&1)"
  echo "COMMENTS: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_comments");' 2>&1)"
  echo "TERMS: $(wp eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_terms");' 2>&1)"
  NP=$(wp post create --post_title='Hello DNA' --post_content='dna body' --porcelain 2>&1)
  echo "POST_ID: $NP"
  echo "POST_TITLE: $(wp post get "$NP" --field=post_title 2>&1)"
  echo "META_ADD: $(wp post meta add "$NP" _e2e_meta hello-dna 2>&1)"
  echo "META_GET: $(wp post meta get "$NP" _e2e_meta 2>&1)"
  NU=$(wp user create tester tester@example.com --porcelain 2>&1)
  echo "USER_ID: $NU"
  echo "USER_ROLE: $(wp user get "$NU" --field=roles 2>&1)"
  echo "POSTLIST: $(wp post list --fields=ID,post_title --format=csv 2>&1 | tr '\n' '|')"
  echo "CONCAT: $(wp eval 'global $wpdb; echo $wpdb->get_var("SELECT CONCAT(UPPER(\"днк\"), \"-\", LOWER(\"ТЕСТ\"))");' 2>&1)"
  echo "CHARLEN: $(wp eval 'global $wpdb; echo $wpdb->get_var("SELECT CHAR_LENGTH(CONCAT(\"abc\",\"ёёжз\"))");' 2>&1)"
  echo "LOGIN_OK: $(wp eval 'echo wp_authenticate("admin","dnatest123") instanceof WP_User ? "yes" : "no";' 2>&1)"
  echo "LOGIN_BAD: $(wp eval 'echo (wp_authenticate("admin","wrong") instanceof WP_Error) ? "error" : "ok";' 2>&1)"
  echo "CRON_COUNT: $(wp eval 'echo count(wp_get_ready_cron_jobs());' 2>&1)"
  } 2>&1 | tee "$2"
}
battery wp-ref /tmp/wp_ref_artifacts.txt
battery wp-dna /tmp/wp_dna_artifacts.txt

# 6) вердикт
echo "=== DIFF (ref vs dna) ==="
if diff -u /tmp/wp_ref_artifacts.txt /tmp/wp_dna_artifacts.txt; then
  echo "E2E_MYSQLWIRE_VERDICT: MATCH"
else
  echo "E2E_MYSQLWIRE_VERDICT: DIFF"
fi
echo "=== dna tables ==="
python3 - <<'PYEOF'
import pymysql
c = pymysql.connect(host='127.0.0.1', port=13307, user='wp', password='wppass', database='wordpress')
cur = c.cursor()
cur.execute("SHOW TABLES")
tables = [r[0] for r in cur.fetchall()]
print(f"tables={len(tables)}")
cur.execute("SELECT COUNT(*) FROM wp_options")
print("options:", cur.fetchone()[0])
c.close()
PYEOF
kill $MR_PID $DNA_PID 2>/dev/null
echo "== done =="
