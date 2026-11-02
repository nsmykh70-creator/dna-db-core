# DNA-DB CMS Edition — PHP-комплект для WordPress (прототип фазы 1)

Каталог содержит drop-in для WordPress, который заменяет слой БД без правки
ядра — тем же механизмом, что и официальный SQLite Integration
(`wp-content/db.php` подменяет класс `wpdb` до его инстанцирования).

## Состав

| Файл | Назначение |
|---|---|
| `php/wp-content/db.php` | drop-in: класс `DNA_DB_wpdb extends wpdb` поверх HTTP-API DNA-DB |
| `../docs/cms/wp_queries_corpus.txt` | канонический список запросов, который покрывает гейт |
| `src/cms/compat.rs` (Rust) | серверная сторона: перехват SET/LOCKS/SHOW, errno, последовательности |

## Установка (эксперимент)

1. Собрать и запустить DNA-DB CMS-редакции:
   ```bash
   cargo build --release --features cms
   ./target/release/dna-db-core serve --admin-port 8443 --data-dir /var/lib/dnadb
   ```
2. В панели создать пользователя с ролью `operator` (write без админ-прав).
3. В `wp-config.php` заменить блок MySQL на:
   ```php
   define('DNA_DB_URL',  'http://127.0.0.1:8443');
   define('DNA_DB_USER', 'wp_app');
   define('DNA_DB_PASS', '…');
   // define('DNA_DB_DBNAME', 'wordpress');   // опционально
   ```
4. Скопировать `db.php` в `wp-content/db.php`.
5. Схему WordPress создать заранее (можно из SQL-консоли панели) —
   пресет ядра встроен в бинарь (`cms::wordpress::WP_CORE_TABLES`),
   установка WP на чистый сервер в фазе 1 ещё не цель.

## Что уже покрывается

- SELECT/INSERT/UPDATE/DELETE в диалекте WP; `SQL_CALC_FOUND_ROWS` /
  `FOUND_ROWS()`; `SELECT LAST_INSERT_ID()`; `SET NAMES`/`SET sql_mode`.
- `AUTO_INCREMENT` на уровне движка + `$wpdb->insert_id`.
- `SHOW TABLES [LIKE]`, `SHOW FULL COLUMNS`, `SHOW INDEX` — путь dbDelta
  (оверлей каталога даёт MySQL-точный `Type` из исходного DDL).
- Ошибки с MySQL-кодами (1062/1146/1064/…) — `wpdb::$last_error`.
- CREATE TABLE в MySQL-диалекте (bigint/varchar(n)/longtext/datetime/
  decimal, UNSIGNED/DEFAULT/AUTO_INCREMENT, KEY/UNIQUE KEY, ENGINE/CHARSET).

## Ограничения прототипа

- Транзакций в API drop-in пока нет (фаза 2: WooCommerce требует
  `SELECT … FOR UPDATE` — их ведёт `cms::compat` + `tx.rs`).
- HTTP-транспорт одноэлементный: каждый запрос — своя сессия гейта, поэтому
  `GET_LOCK` не держится между запросами и `FOUND_ROWS()`/`LAST_INSERT_ID()`
  отвечаются из кэша последнего ответа drop-in'а (см. db.php).
- Составные PK/UNIQUE ключи: движок хранит первую колонку, полный ключ —
  в каталоге-оверлее (`cms::ddl_meta`).
- Двоичные данные проходят как строки (JSON-транспорт).
- Единый HTTP-путь: ~1 RTT на запрос; для продакшена фаза 4 заменит его
  wire-протоколом MySQL.
- Multi-table `DELETE a, b FROM …`, `REGEXP` и строковые функции (`LEFT()`)
  пока не поддержаны (фаза 2; WP переживает их отсутствие — см. E2E-отчёт).

## Проверка без WordPress

Реальная проверка выполнена: **WordPress 7.1.4 устанавливается на DNA-DB
через этот drop-in** (`wp core install` через wp-cli, артефакты совпадают с
эталоном MariaDB 10.11.11) — методика и результаты:
`docs/cms/E2E_WP_REPORT.md`.

Минимальный PHP-скрипт (без WP-функций): убедиться, что `/api/query`
отвечает, — токен нужен как Bearer на каждый запрос:

```php
<?php // php -S для тестов не требуется; CLI-проверка
$login = file_get_contents('https://127.0.0.1:8443/api/login', false, stream_context_create(['http' => [
    'method' => 'POST', 'ignore_errors' => true,
    'header' => "Content-Type: application/json\r\n",
    'content' => json_encode(['user' => 'wp_app', 'pass' => '…']),
    // самоподписанный сертификат панели:
    'ssl' => ['verify_peer' => false, 'verify_peer_name' => false, 'allow_self_signed' => true],
]]));
$token = json_decode($login, true)['token'] ?? '';
$c = file_get_contents('https://127.0.0.1:8443/api/query', false, stream_context_create(['http' => [
    'method' => 'POST', 'ignore_errors' => true,
    'header' => "Content-Type: application/json\r\nAuthorization: Bearer {$token}\r\n",
    'content' => json_encode(['query' => 'SELECT option_name FROM wp_options LIMIT 3', 'kind' => 'sql']),
    'ssl' => ['verify_peer' => false, 'verify_peer_name' => false, 'allow_self_signed' => true],
]]));
var_dump(json_decode($c, true));
```

Rust-сторона покрыта тестами (`cargo test --features cms`) вплоть до
dbDelta-сравнения полей; PHP-код проверяется php -l в CI после установки
toolchain (см. cms-test job).
