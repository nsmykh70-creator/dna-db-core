#!/bin/bash
# E2E: установка WordPress 7.1.4 на DNA-DB CMS Edition через drop-in db.php.
# Сервер живёт только внутри этого вызова (платформа чистит фоновые процессы).
set -u
RT=/home/z/my-project/runtime
WT=/home/z/my-project/worktrees/cms
PHP="$RT/php -d memory_limit=512M $RT/wp-cli.phar"
DNA=/tmp/dna_e2e

# 1) свежий сервер (в battery-only режиме используем существующий data-dir)
pkill -f 'dna-db-core serve' 2>/dev/null
sleep 1
if [ "${SKIP_INSTALL:-0}" = "1" ]; then
  mkdir -p "$DNA"
else
  rm -rf "$DNA"
fi
cd "$WT"
setsid ./target/release/dna-db-core serve --data-dir "$DNA" --port 19998 \
  --pgwire-port 15433 --admin-port 8443 --admin-pass wpadmin-e2e-2026 \
  > /tmp/dna_e2e.log 2>&1 < /dev/null &
for i in $(seq 1 40); do
  curl -sk --max-time 2 https://127.0.0.1:8443/ >/dev/null 2>&1 && break
  sleep 0.5
done
curl -sk --max-time 3 https://127.0.0.1:8443/ >/dev/null 2>&1 || { echo "FATAL: admin не поднялся"; tail -5 /tmp/dna_e2e.log; exit 1; }
echo "== admin up =="

# 2) установка (SKIP_INSTALL=1 для прогона только батареи)
cd /tmp/wptest/wp-dna
if [ "${SKIP_INSTALL:-0}" != "1" ]; then
$PHP core install --url="http://localhost:8091" --title="WP on DNA-DB" \
  --admin_user=admin --admin_password=dnapass123 --admin_email=admin@example.com \
  --skip-email 2>&1 | tee /tmp/wp_dna_install.log
RC=${PIPESTATUS[0]}
echo "INSTALL_RC=$RC"
fi

# 3) батарея проверок (та же, что на эталоне)
B=/tmp/wp_dna_artifacts.txt
{
echo "VERSION: $($PHP core version 2>&1)"
echo "TABLES: $($PHP eval 'global $wpdb; echo implode(",", $wpdb->get_col("SHOW TABLES"));' 2>&1)"
echo "SITEURL: $($PHP option get siteurl 2>&1)"
echo "BLOGNAME: $($PHP option get blogname 2>&1)"
echo "OPT_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_options");' 2>&1)"
echo "POSTS_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_posts");' 2>&1)"
echo "POSTMETA_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_postmeta");' 2>&1)"
echo "USERS_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_users");' 2>&1)"
echo "COMMENTS_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_comments");' 2>&1)"
echo "TERMS_COUNT: $($PHP eval 'global $wpdb; echo (int) $wpdb->get_var("SELECT COUNT(*) FROM wp_terms");' 2>&1)"
NEW_POST=$($PHP post create --post_title='Hello DNA' --post_content='dna body' --porcelain 2>&1)
echo "POST_ID: $NEW_POST"
echo "POST_TITLE: $($PHP post get "$NEW_POST" --field=post_title 2>&1)"
echo "POST_META_ADD: $($PHP post meta add "$NEW_POST" _e2e_meta hello-dna 2>&1)"
echo "POST_META_GET: $($PHP post meta get "$NEW_POST" _e2e_meta 2>&1)"
NEW_USER=$($PHP user create tester tester@example.com --porcelain 2>&1)
echo "USER_ID: $NEW_USER"
echo "USER_ROLE: $($PHP user get "$NEW_USER" --field=roles 2>&1)"
echo "POSTLIST: $($PHP post list --fields=ID,post_title --format=csv 2>&1 | tr '\n' '|')"
echo "LAST_INSERT_ID_EVAL: $($PHP eval 'global $wpdb; echo (int) $wpdb->query("SELECT LAST_INSERT_ID()"); echo ":"; echo (int) $wpdb->get_var("SELECT LAST_INSERT_ID()");' 2>&1)"
} 2>&1 | tee "$B"

echo "== done =="
