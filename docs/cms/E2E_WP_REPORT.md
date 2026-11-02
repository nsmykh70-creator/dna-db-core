# DNA-DB CMS Edition — E2E: реальная установка WordPress 7.1.4 (wp-cli)

Дата: 2 ноября 2026. Ветка: `cms-edition`. Критерий приёмки фазы 1 из
`docs/CMS_EDITION_RU.md` §5 (установка WP + wp-admin + phpunit) — первый и
главный пункт **выполнен**: `wp core install` на DNA-DB проходит успешно,
артефакты установки совпадают с эталоном MariaDB 10.11.11.

## Методика

Одно ядро WordPress 7.1.4 (`wordpress.org/latest.tar.gz`), два инстанса:

| Роль | Движок | Подключение |
|---|---|---|
| Эталон | MariaDB 10.11.11 (Debian-trixie runtime без root, `--port 13306`) | `wpdb` → mysqli |
| Тестируемый | DNA-DB `cms-edition` (release, `--features cms`), admin-API `:8443` (TLS) | drop-in `wp-content/db.php` → `POST /api/query` |

Одинаковая батарея wp-cli-команд на обеих сторонах; сравнение артефактов.
Скрипты: `scripts/e2e_dna_install.sh` (установка + батарея), `make_wp_dna_config.py`
(wp-config.php для DNA-стороны: фиктивные MySQL-константы + `DNA_DB_*`).

## Результат батареи (DNA-DB vs эталон)

| Проверка | DNA-DB CMS Edition | MariaDB 10.11.11 | Вердикт |
|---|---|---|---|
| `wp core install` | Success, rc=0 | Success, rc=0 | MATCH |
| Таблицы после установки | 12 (wp_commentmeta…wp_users) | 12, список идентичен | MATCH |
| `wp_options` строк | 125 | 125 | MATCH |
| `wp_posts` / `wp_postmeta` / `wp_users` / `wp_comments` / `wp_terms` | 3 / 2 / 1 / 1 / 1 | 3 / 2 / 1 / 1 / 1 (до шагов батареи) | MATCH |
| `wp post create` → ID | 4 | 4 (на чистом сайте) | MATCH (AUTO_INCREMENT + `$wpdb->insert_id`) |
| `wp post get/meta add/meta get` | OK | OK | MATCH |
| `wp user create` → ID | 2 | 2 | MATCH |
| `wp user get 2 --field=roles` | subscriber | subscriber | MATCH (после фикса приведения типов) |
| `wp post list` (SQL_CALC_FOUND_ROWS + DISTINCT-путь WP_Query) | OK | OK | MATCH |
| `wp term list category` (DISTINCT + цепочка JOIN) | OK | OK | MATCH |
| `SELECT LAST_INSERT_ID()` на новом соединении | 1:0 | 1:0 | MATCH |
| Site Health / `option get siteurl`,`blogname` | OK | OK | MATCH |

## Найдено и исправлено в ходе E2E

Drop-in `cms/php/wp-content/db.php` (все — реальные дефекты прототипа,
обнаруженные только живым WordPress):

1. **Bearer-токен не отправлялся** — `dna_post()` не прикладывал
   `Authorization` к `/api/query`; все запросы получали `not authenticated`.
2. **TLS**: панель по умолчанию поднимает HTTPS с self-signed парой —
   добавлены `sslverify=false` (wp_remote_post) и `verify_peer=false`
   (stream); транспорт переключён на `file_get_contents(stream)` как
   основной (WP_Http/Requests при установке исчерпывал 512 МБ памяти).
3. **Перехват `FOUND_ROWS()` / `LAST_INSERT_ID()`**: HTTP-транспорт
   одноэлементный (сессия гейта живёт один запрос) — оба шима теперь
   отвечаются из кэша последнего ответа (`found_rows` / `last_insert_id`
   верхнего уровня).
4. **WP 7.1.4 совместимость**: сигнатура `wpdb::select($db, $dbh = null)`;
   переопределён `db_server_info()` (dbDelta вызывает его напрямую).

Ядро движка (`sql_engine.rs`, `relational.rs`, `expr.rs`, `cms/compat.rs`),
каждый пункт закрыт unit-тестом и добавлен в корпус (`wp_queries_corpus.txt`):

5. **Числовое приведение типов** (`Value::compare`): `WHERE ID = '2'`
   не находило строку — WP_User::get_data_by отправляет id строкой (`%s`);
   MariaDB приводит `'2'`→2. Теперь число и числовая строка сравниваются
   численно; нечисловая строка по-прежнему не равна числу (MySQL-ловушка
   «любая строка = 0» намеренно не воспроизводится).
6. **`SELECT DISTINCT`** — проекции DISTINCT в движке не было; гейт
   вырезает флаг и дедуплицирует строки после выполнения (стабильно с
   ORDER BY).
7. **Comma-FROM** (`FROM a, b WHERE …`) — переписывается в
   `a INNER JOIN b ON 1=1` (новая форма cross join в движке), предикаты
   остаются в WHERE — семантика MySQL.
8. **`NOT IN`** — раньше парсер требовал после `NOT` только `LIKE`
   (падал вывод комментариев: `comment_type NOT IN ('note')`). Полная
   поддержка с трёхзначной логикой NULL; `NOT IN (SELECT …)` заворачивается
   в `Expr::Not`.

## Воспроизводимость SQL-корпуса

Дифференциальный прогон №5 (расширенный корпус, 71 запрос) против той же
MariaDB: **MATCH=69, BOTH-ERR=2** (обе СУБД дают одинаковые errno 1062/1091) —
см. `docs/cms/COMPARISON_RUN5.md`. Прогон №4 (66 запросов, до расширения)
воспроизвёл результат №3: MATCH=64, BOTH-ERR=2.

## Оставшиеся пробелы (фаза 2, не блокируют установку WP)

- `DELETE a, b FROM t1 a, t2 b WHERE …` — multi-table DELETE (чистка
  просроченных транзиентов; WP переживает ошибку и продолжает работу).
- `DELETE … WHERE option_name REGEXP '…'` — оператор REGEXP.
- Строковые функции (`LEFT(…)`, …) — встречаются в плагинах.
- `GET_LOCK` поверх HTTP drop-in не держится между запросами (сессия
  гейта одноэлементная) — учитывать до появления постоянных сессий (фаза 4,
  MySQL wire protocol).

## Окружение прогона

- WordPress 7.1.4 (en_US, `latest.tar.gz`), wp-cli 2.12, PHP 8.4.23 (static-php bulk).
- MariaDB 10.11.11 (выделенный runtime в домашнем каталоге, port 13306).
- DNA-DB: `cargo build --release --features cms`, `serve --admin-port 8443`,
  свежий data-dir на каждый прогон, `--admin-pass` (роль admin).
- 261/261 тестов `cargo test --release --features cms` — зелёные.
