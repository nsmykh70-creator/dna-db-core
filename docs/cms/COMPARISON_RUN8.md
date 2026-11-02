# DNA-DB CMS Edition — дифференциальные тесты против MySQL (MariaDB 11.8.6)

Методика: один и тот же корпус SQL WordPress/WooCommerce (`docs/cms/wp_queries_corpus.txt`),
одни и те же фикстуры (12 таблиц WP-пресета + 2 таблицы WooCommerce/HPOS-lookup) и одни и те же
данные подаются в MariaDB 11.8.6 (sql_mode='', как на WP-хостингах) и в DNA-DB CMS Edition
(ветка `cms-edition`, `--features cms`, MySQL-dialect gate через pgwire, порт 15432).

Прогон №8 (2026-11-02) — воспроизведение RUN7 на отдельной машине
(portable MariaDB 11.8.6, Debian glibc 2.41, 2 vCPU). Вердикты совпали с RUN7
позапросно (90 строк вердиктов diff-равны): MATCH 81, BOTH-ERR 4, DIFF-VALUE 1.
DNA-DB — release-сборка `--features cms` (pgwire 15432, --no-tls для
raw-клиента harness). Сравниваются: успех/ошибка, errno, колонки и значения
строк (без ORDER BY — с сортировкой).

## Сводка (86 запросов)

| Вердикт | Запросов |
|---|---|
| MATCH | 81 |
| BOTH-ERR | 4 |
| DIFF-VALUE | 1 |

## Детали по запросам

| # | Метка | Запрос | Вердикт |
|---|---|---|---|
| 1 | P1:SYS | `SET NAMES utf8mb4` | MATCH (ctl) |
| 2 | P1:SYS | `SET sql_mode = ''` | MATCH (ctl) |
| 3 | P1:SYS | `SET autocommit = 1` | MATCH (ctl) |
| 4 | P1:SYS | `SELECT @@SESSION.sql_mode` | MATCH (1 rows) (column names differ: ['@@SESSION.sql_mode'] vs ['@@sql_mode']) |
| 5 | P1:SYS | `SELECT VERSION()` | DIFF-VALUE |
| 6 | P1:SYS | `SHOW FULL COLUMNS FROM wp_options` | MATCH (4 rows) |
| 7 | P1:SYS | `SHOW INDEX FROM wp_options` | MATCH (2 rows) |
| 8 | P1:SYS | `SHOW TABLES LIKE 'wp\\_%'` | MATCH (16 rows) (column names differ: ['Tables_in_wptest (wp\\_%)'] vs ['Tables_in_wordpress']) |
| 9 | P2:SYS | `SELECT GET_LOCK('wp_upgrade_lock', 0)` | MATCH (1 rows) (column names differ: ["GET_LOCK('wp_upgrade_lock', 0)"] vs ['GET_LOCK()']) |
| 10 | P2:SYS | `SELECT RELEASE_LOCK('wp_upgrade_lock')` | MATCH (1 rows) (column names differ: ["RELEASE_LOCK('wp_upgrade_lock')"] vs ['RELEASE_LOCK()']) |
| 11 | P2:SYS | `SELECT IS_FREE_LOCK('wp_cron_lock')` | MATCH (1 rows) (column names differ: ["IS_FREE_LOCK('wp_cron_lock')"] vs ['IS_FREE_LOCK()']) |
| 12 | P1 | `SELECT option_name, option_value FROM wp_options WHERE autoload…` | MATCH (6 rows) |
| 13 | P1 | `SELECT option_value FROM wp_options WHERE option_name = 'siteur…` | MATCH (1 rows) |
| 14 | P1 | `SELECT option_value FROM wp_options WHERE option_name IN ('temp…` | MATCH (3 rows) |
| 15 | P1 | `INSERT INTO wp_options (option_name, option_value, autoload) VA…` | BOTH-ERR (same errno 1062) |
| 16 | P1 | `SELECT LAST_INSERT_ID()` | MATCH (1 rows) |
| 17 | P1 | `UPDATE wp_options SET option_value = 'bar' WHERE option_name = …` | MATCH (affected=0) |
| 18 | P1 | `INSERT INTO wp_options (option_name, option_value, autoload) VA…` | MATCH (affected=0) |
| 19 | P1 | `DELETE FROM wp_options WHERE option_name = '_transient_timeout_…` | MATCH (affected=0) |
| 20 | P1 | `DELETE FROM wp_options WHERE option_name LIKE ('\_transient\_%')` | MATCH (affected=2) |
| 21 | P1 | `SELECT COUNT(*) FROM wp_options WHERE option_name LIKE '_transi…` | MATCH (1 rows) |
| 22 | P1 | `SELECT ID, post_author, post_date, post_content, post_title, po…` | MATCH (1 rows) |
| 23 | P1 | `SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts WHERE 1=1 …` | MATCH (1 rows, ordered) |
| 24 | P1 | `SELECT FOUND_ROWS()` | MATCH (1 rows) |
| 25 | P1 | `SELECT ID FROM wp_posts WHERE post_name = 'hello-world' AND pos…` | MATCH (1 rows) |
| 26 | P1 | `INSERT INTO wp_posts (post_author, post_date, post_date_gmt, po…` | MATCH (affected=1) |
| 27 | P1 | `UPDATE wp_posts SET post_content = '<p>new</p>', post_modified …` | MATCH (affected=1) |
| 28 | P1 | `DELETE FROM wp_posts WHERE ID = 42` | MATCH (affected=1) |
| 29 | P1 | `SELECT post_date_gmt FROM wp_posts WHERE post_status = 'auto-dr…` | MATCH (0 rows) |
| 30 | P1 | `SELECT wp_posts.ID FROM wp_posts INNER JOIN wp_postmeta ON ( wp…` | MATCH (0 rows) |
| 31 | P1 | `SELECT post_id, meta_key, meta_value FROM wp_postmeta WHERE pos…` | MATCH (5 rows) |
| 32 | P1 | `SELECT meta_value FROM wp_postmeta WHERE meta_key = '_wp_attach…` | MATCH (1 rows) |
| 33 | P1 | `INSERT INTO wp_postmeta (post_id, meta_key, meta_value) VALUES …` | MATCH (affected=1) |
| 34 | P1 | `UPDATE wp_postmeta SET meta_value = 'v2' WHERE meta_key = '_edi…` | MATCH (affected=2) |
| 35 | P1 | `INSERT INTO wp_postmeta (post_id, meta_key, meta_value) VALUES …` | MATCH (affected=3) |
| 36 | P1 | `SELECT post_id, meta_value FROM wp_postmeta WHERE meta_key = 'p…` | MATCH (4 rows, ordered) |
| 37 | P1 | `SELECT post_id FROM wp_postmeta WHERE meta_key = 'price' ORDER …` | MATCH (2 rows, ordered) |
| 38 | P1 | `SELECT wp_posts.* FROM wp_posts INNER JOIN wp_postmeta AS mt1 O…` | MATCH (0 rows) |
| 39 | P1 | `SELECT SQL_CALC_FOUND_ROWS wp_posts.ID FROM wp_posts INNER JOIN…` | MATCH (1 rows, ordered) |
| 40 | P1 | `SELECT t.*, tt.* FROM wp_terms AS t INNER JOIN wp_term_taxonomy…` | MATCH (1 rows, ordered) |
| 41 | P1 | `SELECT tr.object_id FROM wp_term_relationships AS tr INNER JOIN…` | MATCH (1 rows) |
| 42 | P1 | `SELECT term_id FROM wp_terms WHERE slug = 'news' OR name = 'New…` | MATCH (1 rows) |
| 43 | P1 | `INSERT INTO wp_terms (name, slug, term_group) VALUES ('News', '…` | MATCH (affected=1) |
| 44 | P1 | `INSERT INTO wp_term_relationships (object_id, term_taxonomy_id,…` | MATCH (affected=0) |
| 45 | P1 | `SELECT * FROM wp_users WHERE user_login = 'admin'` | MATCH (1 rows) |
| 46 | P1 | `SELECT user_id FROM wp_usermeta WHERE meta_key = 'wp_capabiliti…` | MATCH (1 rows) |
| 47 | P1 | `INSERT INTO wp_usermeta (user_id, meta_key, meta_value) VALUES …` | MATCH (affected=1) |
| 48 | P1 | `SELECT COUNT(*) FROM wp_users` | MATCH (1 rows) |
| 49 | P1 | `SELECT * FROM wp_comments WHERE comment_post_ID = 42 AND commen…` | MATCH (1 rows, ordered) |
| 50 | P1 | `SELECT COUNT(*) FROM wp_comments WHERE comment_post_ID = 42 AND…` | MATCH (1 rows) |
| 51 | P1 | `UPDATE wp_comments SET comment_approved = '1' WHERE comment_ID …` | MATCH (affected=0) |
| 52 | DDL | `CREATE TABLE wp_options ( option_id bigint(20) unsigned NOT NUL…` | MATCH (ctl) |
| 53 | DDL | `SHOW FULL COLUMNS FROM wp_options` | MATCH (4 rows) |
| 54 | DDL | `ALTER TABLE wp_options ADD COLUMN new_col varchar(64) NOT NULL …` | MATCH (ctl) |
| 55 | DDL | `ALTER TABLE wp_options DROP COLUMN old_col` | BOTH-ERR (same errno 1091) |
| 56 | DDL | `CREATE INDEX option_name_idx ON wp_options (option_name)` | MATCH (ctl) |
| 57 | DDL | `SHOW FULL COLUMNS FROM wp_postmeta` | MATCH (4 rows) |
| 58 | DDL | `DROP INDEX option_name_idx ON wp_options` | MATCH (ctl) |
| 59 | P2 | `START TRANSACTION` | MATCH (ctl) |
| 60 | P2 | `SELECT stock_quantity FROM wp_wc_product_meta_lookup WHERE prod…` | MATCH (1 rows) |
| 61 | P2 | `UPDATE wp_wc_product_meta_lookup SET stock_quantity = stock_qua…` | MATCH (affected=1) |
| 62 | P2 | `COMMIT` | MATCH (ctl) |
| 63 | P2 | `ROLLBACK` | MATCH (ctl) |
| 64 | P2 | `SELECT TOTAL_SALES FROM wp_wc_product_meta_lookup WHERE product…` | MATCH (1 rows) |
| 65 | P2 | `SELECT id FROM wp_wc_orders WHERE status = 'processing' ORDER B…` | MATCH (1 rows, ordered) |
| 66 | P2 | `SELECT COUNT(DISTINCT customer_id) FROM wp_wc_orders WHERE stat…` | MATCH (1 rows) |
| 67 | P2 | `SELECT order_item_id, order_item_name FROM wp_woocommerce_order…` | MATCH (2 rows, ordered) |
| 68 | P2 | `SELECT COUNT(*) FROM wp_woocommerce_order_items WHERE order_id …` | MATCH (1 rows) |
| 69 | P2 | `SELECT meta_key, meta_value FROM wp_woocommerce_order_itemmeta …` | MATCH (1 rows) |
| 70 | P2 | `INSERT INTO wp_woocommerce_order_items (order_item_name, order_…` | MATCH (affected=1) |
| 71 | P2 | `SELECT LAST_INSERT_ID()` | MATCH (1 rows) |
| 72 | P2 | `UPDATE wp_woocommerce_order_itemmeta SET meta_value = '3' WHERE…` | MATCH (affected=1) |
| 73 | P2 | `SELECT meta_value FROM wp_woocommerce_order_itemmeta WHERE orde…` | MATCH (1 rows) |
| 74 | P1 | `SELECT DISTINCT t.term_id, tr.object_id FROM wp_terms AS t INNE…` | MATCH (0 rows, ordered) |
| 75 | P1 | `SELECT COUNT(*) FROM wp_term_relationships, wp_posts WHERE wp_p…` | MATCH (1 rows) |
| 76 | P1 | `SELECT * FROM wp_users WHERE ID = '2' LIMIT 1` | MATCH (0 rows) |
| 77 | P1 | `SELECT wp_comments.comment_ID FROM wp_comments WHERE ( ( commen…` | MATCH (2 rows) |
| 78 | P1 | `INSERT INTO wp_terms (name, slug, term_group) VALUES ('News','n…` | MATCH (affected=2) |
| 79 | P2:LOCK | `SET innodb_lock_wait_timeout = 3` | MATCH (ctl) |
| 80 | P2:LOCK | `START TRANSACTION` | MATCH (ctl) |
| 81 | P2:LOCK | `SELECT meta_value FROM wp_postmeta WHERE post_id = 77 AND meta_…` | MATCH (0 rows) |
| 82 | P2:LOCK | `UPDATE wp_postmeta SET meta_value = '8' WHERE post_id = 77 AND …` | MATCH (affected=0) |
| 83 | P2:LOCK | `COMMIT` | MATCH (ctl) |
| 84 | P2:LOCK | `SELECT action_id FROM wp_actionscheduler_actions WHERE hook = '…` | BOTH-ERR (same errno 1146) |
| 85 | P2:LOCK | `SELECT stock FROM wp_wc_product_meta_lookup WHERE product_id = …` | BOTH-ERR (same errno 1054) |
| 86 | P2:LOCK | `UPDATE wp_wc_product_meta_lookup SET stock_quantity = stock_qua…` | MATCH (affected=1) |

## Расхождения и ошибки

### #5 P1:SYS — DIFF-VALUE

```sql
SELECT VERSION()
```
- row 0 col VERSION(): mysql='11.8.6-MariaDB' dna='10.11.11-MariaDB'

### #15 P1 — BOTH-ERR (same errno 1062)

```sql
INSERT INTO wp_options (option_name, option_value, autoload) VALUES ('foo','bar','yes')
```
- MySQL error: [1062] Duplicate entry 'foo' for key 'option_name'
- DNA error: [1062/23000] [MySQL errno 1062] UNIQUE constraint failed: wp_options.option_name = foo (index 'option_name')

### #55 DDL — BOTH-ERR (same errno 1091)

```sql
ALTER TABLE wp_options DROP COLUMN old_col
```
- MySQL error: [1091] Can't DROP COLUMN `old_col`; check that it exists
- DNA error: [1091/42000] [MySQL errno 1091] SQL: can't DROP COLUMN 'old_col'; check that it exists in table 'wp_options'

### #84 P2:LOCK — BOTH-ERR (same errno 1146)

```sql
SELECT action_id FROM wp_actionscheduler_actions WHERE hook = 'wc_schedule_sale' AND status IN ('pending') ORDER BY action_id FOR UPDATE SKIP LOCKED
```
- MySQL error: [1146] Table 'wptest.wp_actionscheduler_actions' doesn't exist
- DNA error: [1146/42S02] [MySQL errno 1146] SQL: relation 'wp_actionscheduler_actions' does not exist (CREATE TABLE first)

### #85 P2:LOCK — BOTH-ERR (same errno 1054)

```sql
SELECT stock FROM wp_wc_product_meta_lookup WHERE product_id = 42 FOR UPDATE NOWAIT
```
- MySQL error: [1054] Unknown column 'stock' in 'SELECT'
- DNA error: [1054/42S22] [MySQL errno 1054] SQL: unknown column 'stock' in SELECT projection (tables: wp_wc_product_meta_lookup)
