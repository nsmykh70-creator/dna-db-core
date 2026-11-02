#!/usr/bin/env python3
"""wp-config.php для DNA-DB E2E (ручной шаблон, эквивалент wp-cli config create)."""
import re, pathlib

ref = pathlib.Path('/tmp/wptest/wp-ref/wp-config.php').read_text()
salts = [l for l in ref.splitlines()
         if re.match(r"define\(\s*'(AUTH|SECURE_AUTH|LOGGED_IN|NONCE)_((KEY)|(SALT))'", l)]
assert len(salts) == 8, f"ожидалось 8 солей, найдено {len(salts)}"

cfg = """<?php
# DNA-DB CMS Edition E2E — вручную собранный wp-config.php.
# MySQL-константы оставлены для совместимости, но wpdb полностью
# подменяется drop-in'ом wp-content/db.php (DNA_DB_* ниже).
define( 'DB_NAME', 'dummy_dna' );
define( 'DB_USER', 'dummy' );
define( 'DB_PASSWORD', 'dummy' );
define( 'DB_HOST', '127.0.0.1:19999' );
define( 'DB_CHARSET', 'utf8mb4' );
define( 'DB_COLLATE', '' );

{SALTS}

$table_prefix = 'wp_';

define( 'WP_DEBUG', true );
define( 'WP_DEBUG_LOG', true );
define( 'WP_DEBUG_DISPLAY', false );

# Песочница без интернета для ядра WP: блокируем внешние HTTP-запросы,
# чтобы установка не зависела от api.wordpress.org.
define( 'WP_HTTP_BLOCK_EXTERNAL', true );
define( 'WP_ACCESSIBLE_HOSTS', '127.0.0.1, localhost' );

define( 'DNA_DB_URL',  'https://127.0.0.1:8443' );
define( 'DNA_DB_USER', 'admin' );
define( 'DNA_DB_PASS', 'wpadmin-e2e-2026' );
define( 'DNA_DB_DBNAME', 'wordpress' );

if ( ! defined( 'ABSPATH' ) ) {{
        define( 'ABSPATH', __DIR__ . '/' );
}}

require_once ABSPATH . 'wp-settings.php';
""".format(SALTS="\n".join(salts))

pathlib.Path('/tmp/wptest/wp-dna/wp-config.php').write_text(cfg)
print("wp-config.php written:", len(cfg), "bytes,", len(salts), "salts")
