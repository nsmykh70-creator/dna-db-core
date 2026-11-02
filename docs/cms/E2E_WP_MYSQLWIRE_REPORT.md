# DNA-DB CMS Edition — E2E: WordPress 7.1.4 через УНИВЕРСАЛЬНЫЙ MySQL wire-адаптер

Дата: 2 ноября 2026. Ветка: `cms-edition`.

Воспроизведение: 2 ноября 2026 прогон повторён на отдельной машине
(portable MariaDB 11.8.6, PHP 8.4.27 из deb-пакетов, WP-CLI 2.12.0) —
вердикт снова MATCH, все артефакты побайтово совпали с эталоном.

## Что проверяется

Установка **настоящего WordPress 7.1.4** на DNA-DB **без какого-либо кода
адаптации**: wp-cli + штатный mysqli подключаются к `dna-db-core mysqld`
(порт 13307) как к обычному MySQL-серверу. Никаких drop-in'ов, никаких
плагинов — только `DB_HOST`.

| Роль | Движок | Подключение |
|---|---|---|
| Эталон | MariaDB 11.8.6 (`:13306`) | wpdb → mysqli |
| Тестируемый | DNA-DB `cms-edition` (release, `--features cms`), `mysqld :13307` | wpdb → mysqli → **wire-протокол MySQL 4.1** |

Скрипт: `scripts/e2e_wp_mysqlwire.sh` — обе стороны получают один и тот же
wp-config (идентичные соли), одну батарею wp-cli-команд; артефакты сравниваются
построчно.

## Результат: MATCH — все артефакты идентичны эталону

| Артефакт | MariaDB | DNA-DB (wire) | Вердикт |
|---|---|---|---|
| `wp core install` | rc=0 | rc=0 | MATCH |
| Таблицы (dbDelta по wire) | 12: wp_commentmeta…wp_users | идентичный список | MATCH |
| `siteurl` / `blogname` | http://localhost:8091 / WP E2E | идентично | MATCH |
| `wp_options` строк | 125 | 125 | MATCH |
| `wp_posts` / `wp_postmeta` / `wp_users` / `wp_comments` / `wp_terms` | 3 / 2 / 1 / 1 / 1 | идентично | MATCH |
| `wp post create` → ID | 4 | 4 | MATCH (AUTO_INCREMENT-паритет по wire) |
| Заголовок поста | Hello DNA | Hello DNA | MATCH |
| `post meta add/get` | hello-dna | hello-dna | MATCH |
| `wp user create` → ID / роль | 2 / subscriber | идентично | MATCH |
| `wp post list` (csv) | 4,"Hello DNA"\|1,"Hello world!" | идентично | MATCH |
| `SELECT CONCAT(UPPER('днк'),'-',LOWER('ТЕСТ'))` | ДНК-тест | ДНК-тест | MATCH (utf8mb4 + скалярные функции) |
| `SELECT CHAR_LENGTH(CONCAT('abc','ёёжз'))` | 7 | 7 | MATCH (CHAR_LENGTH в символах, не байтах) |
| `wp_authenticate` верный пароль | WP_User | WP_User | MATCH (PHPASS-хэш: SELECT по `user_login`) |
| `wp_authenticate` неверный пароль | WP_Error | WP_Error | MATCH |
| cron-задачи готовые к запуску | 1 | 1 | MATCH |

`diff /tmp/wp_ref_artifacts.txt /tmp/wp_dna_artifacts.txt` → пусто
(**21/21 строк-артефактов идентичны**).

## Значение

Это первый прогон, в котором WordPress работает на DNA-DB **без единого файла
адаптации** — универсальный MySQL wire-адаптер полностью заменяет MySQL для
ядра WordPress, wp-cli и dbDelta. Вместе с `E2E_WOO_MYSQLWIRE_REPORT.md`
(WooCommerce) закрывается сценарий «сайт как на обычном MySQL-хостинге».
