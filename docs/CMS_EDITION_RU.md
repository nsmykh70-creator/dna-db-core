# DNA-DB CMS Edition — WordPress / Joomla / OpenCart / Drupal / MediaWiki

Ветка `cms-edition`, feature-гейт `cms`. База: v4.1 (`main`, панель v4.1,
компакция, журнал компакций, Prometheus-гейджи).

## 1. Цель

Заставить популярные PHP-CMS работать на DNA-DB без переписывания приложений:

| Клиент | Версия | Точка входа | Приоритет |
|---|---|---|---|
| WordPress | 6.x | drop-in `wp-content/db.php` (замена `wpdb`) либо MySQL wire | 1 |
| WooCommerce | 8.x+ | тот же `wpdb` + транзакции/блокировки | 1 |
| Joomla | 5.x | `Joomla\Database\DatabaseDriver` (MySQLi/PDO MySQL/**PDO PgSQL**) | 2 |
| OpenCart | 4.x | MySQL wire — штатный mysqli, официальный CLI-установщик | 2 |
| Drupal | 11.x | MySQL wire — штатный PDO MySQL, web-установщик `core/install.php` | 2 |
| MediaWiki | 1.43 LTS | MySQL wire — штатный mysqli, CLI-установщик `maintenance/install.php` | 2 |
| Прочее PHP | — | MySQL wire protocol (фаза 4) | 3 |

Ключевой факт: WordPress официально поддерживает подмену слоя БД
(drop-in `db.php` — так работает официальный SQLite Integration),
Joomla — подключаемые драйверы БД. Полноценный MySQL wire protocol нужен
не сразу, а как универсальный вход на фазе 4.

## 2. Что уже есть в ядре (и чем это помогает)

| Компонент | Где | Значение для CMS |
|---|---|---|
| SQL-движок: JOIN (INNER/LEFT/…), GROUP BY, HAVING, агрегаты, ORDER BY, LIMIT | `src/sql_engine.rs` | Ядро запросов WP_Query / com_content уже покрывается |
| Транзакции: `TransactionCoordinator`, begin/commit/rollback, уровни изоляции | `src/tx.rs` | База для WooCommerce (`SELECT … FOR UPDATE` — доработать) |
| PostgreSQL wire protocol | `src/pgwire.rs` | Joomla умеет PDO PgSQL — самый короткий путь для неё (см. §3) |
| SQL→DnaQL транслятор | `src/sql_to_dnaql.rs` | Зародыш диалект-слоя (без JOIN/GROUP BY — их ведёт sql_engine) |
| Реляционный слой: `ColumnType`, `ForeignLink`, `Value` | `src/relational.rs` | Схемы WP-таблиц ложатся на него |
| Роли viewer/operator/admin, bearer-сессии | `src/admin.rs`, `src/access.rs` | Модель подключений/прав уже есть |
| Prometheus-метрики, компакция, журнал | `src/admin.rs`, `src/engine.rs` | Эксплуатация из коробки |
| CMS-примитивы: errno, сессия, named locks | `src/cms/` (`--features cms`) | **Этот каркас добавлен первой задачей ветки** |

## 3. Три маршрута интеграции

**A. Drop-in драйвер (основной для WP).** PHP-класс `wpdb` поверх нативного
API DNA-DB; SQL-подмножество WP транслируется в ядро. Плюс: не нужен
wire-протокол. Минус: плагины, работающие с mysqli напрямую, обходят его
(то же ограничение у официального SQLite-плагина — приемлемо).

**B. Существующий pgwire — быстрый путь для Joomla.** Joomla 5 содержит
драйвер PostgreSQL. Пока pgwire поддерживает simple protocol (prepared
statements — с рекомендацией отключать), для Joomla это самый дешёвый
эксперимент: поднять DNA-DB с `--pgwire-port`, включить драйвер
`postgresql` в `configuration.php`. Нужны: `SHOW TABLES`-подмножество
каталожных запросов установщика, миграции `ALTER TABLE`, `#__`-префикс.

**C. MySQL wire protocol — ГЛАВНЫЙ маршрут (универсальный вход).**
Решение по архитектуре (2026-11): не адаптировать каждую CMS по
отдельности, а сделать DNA-DB универсальной заменой MySQL для любых
сайтов и сервисов. Реализовано: `src/cms/mysqlwire.rs` — полноценный
сервер клиент/серверного протокола MySQL 4.1, который слушает порт 3306,
принимает запросы CMS, прогоняет их через MySQL-диалектный гейт
(`CmsSqlGate`: SQL → движок → DNAQL-хранилище) и возвращает ответы в
формате MySQL. После него работают mysqli, PDO_mysql, wp-cli, phpMyAdmin,
mysqldump, Connector/Py, ORM — без всяких drop-in'ов и правок кода CMS.
Подробности и упрощённая установка — §3.1.

### 3.1. Универсальный MySQL-шлюз `dna-db-mysqld` и установка «проще, чем MySQL»

Слой адаптации теперь полный: CMS говорит по MySQL-проводу, шлюз переводит
запросы в SQL-подмножество движка (а то, что CMS ожидает от сервера — сессии,
`LAST_INSERT_ID()`, errno, SHOW-каталог, блокировки — обеспечивает гейт),
ответы возвращаются в MySQL-протоколе. Двусторонний конвертер:

```
WordPress/Woo/Joomla/… ──(mysqli/PDO, порт 3306)──▶ mysqlwire
                                                     │ SQL
                                                     ▼
                                               CmsSqlGate (диалект MySQL)
                                                     │ SQL-подмножество
                                                     ▼
                                            SqlEngine → DNA-хранилище
CMS ◀──результаты/errno в MySQL-протоколе──── mysqlwire
```

Минимальный запуск (режим `mysqld`):

```bash
cargo build --release --features cms
./target/release/dna-db-core mysqld --data-dir ./dna_data --wordpress
# готово: MySQL-совместимый сервер на 0.0.0.0:3306, schema WP создана,
# логин root без пароля (trust) или --user wp:secret;
# сервер печатает готовый блок wp-config.php.
```

Что покрывает протокол (проверено реальными клиентами, см.
`docs/cms/MYSQLWIRE_CLIENT_SMOKE.md` — 21/21):

- HandshakeV10, `mysql_native_password` (SHA1-scramble) + прозрачный
  AuthSwitchRequest для клиентов с `caching_sha2_password`;
- COM_QUERY: text-протокол резалтсетов, multi-statement, OK/ERR с настоящими
  errno (1045/1062/1146/1213/1205/…), COM_PING/INIT_DB/QUIT/FIELD_LIST/
  STATISTICS/RESET_CONNECTION;
- COM_STMT_PREPARE/EXECUTE/CLOSE/RESET/SEND_LONG_DATA: бинарные параметры,
  бинарные резалтсеты; метаданные колонок — пробой с NULL-параметрами на
  prepare (mysqli и Connector/Py видят реальные колонки);
- `SET NAMES`, `USE`, autocommit, bare `COMMIT`/`ROLLBACK` вне транзакции =
  тихий no-op (паритет с MySQL — на этом ломались реальные драйверы).

Функции SQL, которых не хватало движку, добавлены в ядро (требование
«допиши недостающее»): `Expr::Fn` — CONCAT, CONCAT_WS, UPPER/LOWER,
LENGTH/CHAR_LENGTH, TRIM/LTRIM/RTRIM, SUBSTRING, REPLACE, LEFT/RIGHT,
REVERSE, LPAD/RPAD, REPEAT, ABS/ROUND/FLOOR/CEIL, SQRT/POW, SIGN, MOD,
COALESCE, IFNULL, NULLIF, IF, GREATEST/LEAST — с NULL-семантикой MySQL.
Гейт дополнительно подставляет `NOW()` в DML.

Установка проще, чем MySQL: не нужен `mysql_install_db`, интерактивный
`mysql_secure_installation`, выбор root-пароля и создание БД/пользователя —
одна команда поднимает сервер с готовой схемой WordPress и печатает блок
`wp-config.php`. Скрипт `scripts/install.sh` ставит бинарь + systemd-юнит:
пребилд x86_64 скачивается с проверкой SHA256 (Rust не нужен, фолбэк —
cargo-сборка); в Docker/WSL без systemd — режим `nohup`+pidfile; есть
`--uninstall`.

### Прогон №7 + E2E через wire + бенчмарки + security (2026-11-02)

* Маршрут C (mysqld) — **главный**: реальные WordPress 7.1.4 и WooCommerce
  11.2.1 работают через штатный mysqli без единого файла адаптации.
  WP: 21/21 артефактов идентичны MariaDB; Woo: 12/12, 0 SQL-ошибок.
  Отчёты: `docs/cms/E2E_WP_MYSQLWIRE_REPORT.md`,
  `docs/cms/E2E_WOO_MYSQLWIRE_REPORT.md`.
* Диф-корпус №7 (86 запросов) против MariaDB 11.8.6: 81 MATCH, 4 BOTH-ERR
  (одинаковый errno), 1 DIFF-VALUE (только строка VERSION()) —
  `docs/cms/COMPARISON_RUN7.md`; воспроизведён позапросно на отдельной
  машине — прогон №8, вердикты идентичны (`docs/cms/COMPARISON_RUN8.md`).
* Бенчмарк wire (pymysql, 8 000 ops/фаза): single INSERT ×1,20 и WP-микс
  ×1,15 к MariaDB 11.8.6, паритет чтений/UPDATE (×0,91–×1,04), RAM демона
  8–13 против 118–123 MiB (примерно на порядок меньше) —
  `docs/cms/wire_bench_results.json`.
* Security wire-поверхности: 24/24 — `docs/cms/MYSQLWIRE_SECURITY_REPORT.md`.
* Пошаговое руководство: `docs/CMS_MYSQLWIRE_GUIDE_RU.md`.

## 4. Пробелы движка (чек-лист из аудита)

Состояние после фазы-1 каркаса и интеграции гейта (ветка `cms-edition`):

1. **AUTO_INCREMENT + `LAST_INSERT_ID()`** — ✅ готово: реестр
   последовательностей (`cms::sequence`, seed от MAX(pk), bump по явным id),
   поле `auto_increment_column` в схеме, назначение в `exec_insert_tx`,
   шим `SELECT LAST_INSERT_ID()` в гейте.
2. **`SELECT … FOR UPDATE` / deadlock-детект** — ✅ готово (см. ниже,
   «Строчные блокировки»): реестр `cms::row_locks` с waits-for графом,
   ожидание с `innodb_lock_wait_timeout` → errno 1205, цикл A↔B → errno
   1213 (SQLSTATE 40001 — Woo retry), `NOWAIT` → errno 3572,
   `SKIP LOCKED` → чужие строки исключаются из результата (Action
   Scheduler); DML ждёт/берёт X-локи по тем же ключам, COMMIT/ROLLBACK/
   disconnect снимают блокировки сессии.
3. **Коллации** `utf8mb4_unicode_ci`: сравнение без регистра, PAD SPACE,
   составные и префиксные индексы (meta_value 191) — префиксы сохраняются
   в оверлее, сравнение — фаза 3.
4. **`FOUND_ROWS()` / `SQL_CALC_FOUND_ROWS`** — ✅ готово в гейте, с
   2026-11-10 — полные семантики MySQL: FOUND_ROWS() возвращает счёт
   запроса БЕЗ LIMIT (тот же SELECT перепроигрывается без лимитного
   хвоста и блокировок); прежнее приближение «счёт по странице»
   устранено (тест `sql_calc_found_rows_unlimited_count`).
5. **Каталог**: `SHOW TABLES LIKE`, `SHOW FULL COLUMNS`, `SHOW INDEX`
   (14 колонок, в форме MariaDB), `DESCRIBE`, `SHOW VARIABLES` — ✅ готово
   через оверлей (`cms::ddl_meta`: MySQL-точный `Type`, чтобы dbDelta не
   видел расхождений; CREATE TABLE через гейт регистрирует оверлей
   автоматически) + fallback из схемы движка; `information_schema` — фаза 3.
6. **ALTER TABLE**: ADD COLUMN (с NOT NULL/DEFAULT → эффективный дефолт),
   DROP COLUMN, CREATE/DROP INDEX — ✅ готово; **онлайн-смена типа — ✅
   готово (2026-11-10)**: `ALTER TABLE … MODIFY [COLUMN] c T [флаги]` и
   `ALTER TABLE … CHANGE [COLUMN] old new T [флаги]` — метаданные + одна
   батч-переписка данных (`Value::coerce_for_column` для каждой строки;
   переименование перекеивает колонку в каждой строке и переносит
   вторичные индексы атомарно); оверлей (SHOW FULL COLUMNS / SHOW CREATE
   TABLE) синхронизируется теперь для ВСЕХ ALTER-колоночных операций (до
   этого ALTER оставлял оверлей в состоянии CREATE — dbDelta видел
   устаревший набор колонок); тест `alter_modify_change_column_parity`.
7. **Именованные блокировки** — ✅ готово: реестр `cms::locks` + SQL-шимы
   `GET_LOCK/RELEASE_LOCK/IS_FREE_LOCK/IS_USED_LOCK` в гейте.
8. **Типы**: BIGINT UNSIGNED/DECIMAL/DATETIME/ENUM — частично: имена
   принимаются и маппятся на базовые типы (cms-грамматика); **DECIMAL по
   scale — ✅ готово (2026-11-06)**: decimal(p,s) в SELECT-выдаче
   пере-рендерится с заявленной шкалой ("250" → "250.00", "0.5" в
   decimal(12,8) → "0.50000000"), metadata колонок — MYSQL_TYPE_NEWDECIMAL
   с decimals=s в text- И binary-протоколах, `SUM(col)` под алиасом
   decimal-колонки хранит scale (как MySQL); резолв — по FROM-таблицам
   (`cms::compat::decimal`, квалифицированные имена, уникальность
   шкалы при join); **SUM(выражение) — ✅ готово (2026-11-10)**: шкала
   аргумента-выражения = MAX среди десятичных колонок выражения (с
   сбалансированным разбором вложенных вызовов); попутно устранён
   двигательный баг «SUM(выражение) не агрегировалась вовсе»
   (`collect_agg_items` не собирал `AggOverExpr` — пустые строки в выдаче);
   семантика unsigned — фаза 3.
9. **SET-функции сессии**: `SET NAMES`, `USE db`, `SET autocommit`,
   `SET sql_mode` — ✅ готово в гейте; `CmsSession` хранит состояние.
10. **Unix domain socket** для хостингов, persistent connections PHP-FPM —
    фаза 4 (wire-протокол).
11. **DEFAULT-fill при INSERT** — ✅ готово: `ChromosomeTable.column_defaults`
    (serde-default) хранит эффективные дефолты MySQL-диалекта (явный
    DEFAULT либо нулевое значение NOT NULL); `exec_insert_tx` заполняет
    пропущенные колонки.

### Диалект-слой: дополнительные закрытые пробелы (дифф-тесты против MariaDB)

Backslash-escape в строках (`\'`, `\\`, `\%`/`\_` для LIKE), backticks,
`LIMIT offset,count`, `LIKE/'NOT LIKE'` в общем грамматике выражений
(ci, `%`/`_`/`\`), `ON DUPLICATE KEY UPDATE … VALUES(col)` (по любому
уникальному ключу), `t.*`/`wp_posts.*` (alias-star по карте алиасов),
`AS t`-алиасы, `ON (a.id = b.id)` в JOIN, квалифицированные `t.col` в
SELECT/GROUP BY, `NOW()` в DML (гейт), zero-row RowDescription, UPDATE
affected = изменённые строки, `LAST_INSERT_ID()` = первый id, сброс
последовательности при DROP TABLE, строковые литералы не превращаются в
псевдо-DNA (lowercase `'cat'` остаётся текстом).

**Остаточные расхождения**: нет. Прогон №3 против MariaDB 10.11.11
(2026-11-02, корпус 66 запросов): **64 MATCH + 2 идентичных паритета
ошибок (1062/1091) = 66/66 (100 %)**. Прогон №2 (61/63) закрыл
`CAST(x AS SIGNED|UNSIGNED|CHAR)`, `COUNT(DISTINCT col)`, арифметику в
`UPDATE SET`, ci-проекции + честную errno 1054, `VERSION()` →
`10.11.11-MariaDB`, SHOW INDEX в 14-колоночной форме MariaDB. Прогон №3
закрыл последние два: `ORDER BY <выражение>` (`meta_value+0` —
числовая сортировка по мете WP_Query: разбор выражения-ключа,
материализация без выходной колонки; строково-числовое приведение
`'250.00'+0 → 250` за `--features cms`) и порядок колонок `SELECT *` /
`t.*, tt.*` — теперь в порядке декларации (`column_order` в схеме
таблицы; ALTER ADD добавляет в конец, DROP убирает). Регистро-
независимость WHERE/GROUP BY — фаза 3 (коллации); ~~DECIMAL-форматирование
по scale — фаза 2~~ — ✅ закрыто 2026-11-06 (`cms::compat::decimal`, см.
§4 item 8 и E2E_DEADLOCK_RETRY_REPORT). Полный отчёт:
docs/cms/COMPARISON_MYSQL_REPORT.md.

### Deadlock-retry в живом заказе + DECIMAL по scale (2026-11-06)

Закрыты последние два приёмочных пункта Фазы 2.

**Deadlock-retry (errno 1213).** E2E `scripts/e2e_deadlock_retry.sh` →
`scripts/e2e_deadlock_retry.php`: два конкурентных «живых заказа»
(main + worker-процесс) в противоположном порядке берут `SELECT … FOR
UPDATE`-холды двух складовых строк SKU-A/SKU-B (prepared-стейтменты,
`innodb_lock_wait_timeout=5`), затем заказывают обе позиции — waits-for
цикл A↔B, движок назначает жертву errno 1213 (SQLSTATE 40001), Woo-обвязка
`tx_with_retry` ретраит всю транзакцию (до 10 попыток, случайный бэкофф,
1213/1205/3572 — ретраятся, прочее — падает). Две фазы меняют роли:
жертва №1 — worker, №2 — main; вердикты PASS/PASS: в каждой фазе 4 заказа
(каждая сессия заказала каждый SKU ровно один раз), склад 10 → 8/8, 0
неожиданных ошибок, счётчик 1213 у ожидаемой жертвы ≥ 1. Форма транзакции
важна: **все холды — до первого DML** (гейт single-writer: отказ оператора
не откатывает уже применённый DML — см. «read-your-writes» в execute()),
поэтому дедлок бьёт в write-free фазу и retry не дублирует эффекты.
Детали: `docs/cms/E2E_DEADLOCK_RETRY_REPORT.md`.

Попутный фикс оверлея — **inline `PRIMARY KEY` в определении колонки**
(`sku varchar(20) NOT NULL PRIMARY KEY`): парсер признавал только табличную
форму `PRIMARY KEY (cols)`, из-за чего строковые локи/PointGet на таких
таблицах молча отключались (PK пуст → lock_select_rows bail). Юнит-тест
`inline_primary_key_column_constraint`.

**DECIMAL по scale (§4 item 8).** Модуль `cms::compat::decimal`: SELECT-выдача
пост-процессится — выходные колонки, разрешимые в decimal(p,s)-колонку
оверлея (по FROM-таблицам: квалифицированные имена по алиасам, неквали-
фицированные — по уникальности шкалы среди таблиц), пере-рендерятся с
фиксированной шкалой (`250` → `"250.00"`, `0.125` в decimal(12,8) →
`"0.12500000"`); `CmsQueryResult.decimal_scales` несёт per-column шкалу в
wire-слой: ColumnDefinition41 теперь отдаёт **MYSQL_TYPE_NEWDECIMAL (246) с
decimals=s** в text- и binary-протоколах (COM_FIELD_LIST — из точного Type
оверлея). `SUM(col) AS col` хранит scale колонки (как MySQL для
SUM(decimal(p,s))); выражения без привязки к оверлею (`total * 2`) остаются
DOUBLE-рендером — документированное расхождение. Проверено живым mysqli:
text-значения "250.00"/"0.00", prepared-метаданные type=246 decimals=2/8,
`SUM(total)` = "250.00" (`scripts/decimal_wire_smoke.php`, вердикт
DECIMAL_WIRE_OK). Тестов: 294 с `--features cms` (+9), 195 без фичи
(4 известных cms-гейтованных отказа без изменений).

### Строчные блокировки `FOR UPDATE` (2026-11-02)

Закрыт последний блокер конкурентных заказов Woo — §4 item 2. Модуль
`cms::row_locks`: реестр `(таблица, PK)` → держатель + waits-for граф.
Гейт (`cms::compat`) резервирует PK основной таблицы из оверлея и берёт
ключи либо из проекции результата (`SELECT *`, Action Scheduler), либо
внутренним `SELECT <pk>` по тому же FROM/WHERE. Семантика InnoDB:
`FOR UPDATE` в транзакции держит X-лок до COMMIT/ROLLBACK/disconnect;
в autocommit — только на время оператора; `UPDATE`/`DELETE` ждут чужие
удержания (в транзакции — берут свои X-локи, поэтому два UPDATE в
обратном порядке дают честный 1213, который и ретраит Woo); `NOWAIT` —
немедленный 3572; `SKIP LOCKED` — чужие строки исключаются из выдачи
(при PK в проекции — фильтрация самого результата, очередь двух
воркеров получает разные строки). Бюджет ожидания — per-session
`SET innodb_lock_wait_timeout = 0.5` (дефолт 50 с, errno 1231 на
некорректное значение). Строковые литералы, содержащие «for update»,
сканер-хвост не считает блокировкой. Дивергенции (документированы):
GROUP BY/агрегаты с FOR UPDATE не блокируются; multi-table DELETE без
пробы; составные PK — по первой колонке (как везде в оверлее).
Юнит-тесты: 6 на реестр (`row_locks`) + 8 на гейт (блокировка 1205,
NOWAIT 3572, дедлок 1213 c фоновым ожидателем, реентерабельность,
autocommit-UPDATE ждёт, X-лок UPDATE в транзакции, DELETE ждёт,
disconnect снимает, SKIP LOCKED очередь). Тестов: 276 с фичей,
221 без — main-сборка не тронута.

### Прогон №6 + e2e-заказ WooCommerce 11.2.1 (2026-11-02)

**Фаза 2, приёмка: e2e-заказ WooCommerce выполнен.** Полный цикл —
товар со складом → `wc_create_order` (HPOS) → позиция ×2 → billing
(кириллица) → `calculate_totals` → `save` → `wc_reduce_stock_levels` —
даёт на DNA-DB артефакты, идентичные эталону MariaDB 10.11.11: ids 10/11,
итого 500.00, `wp_woocommerce_order_itemmeta` побайтно (включая
`_reduced_stock = 2`), склад 10 → 8. Активация самого Woo (dbDelta, ~40
таблиц через гейт) проходит на обеих сторонах. Детали:
`docs/cms/E2E_WOO_REPORT.md`.

E2E вскрыл и закрыл **дефект применения типов колонок при записи**:
`wpdb::insert()` без формата шлёт все значения строками — HPOS пишет
`order_id` как `'15'` в `bigint unsigned`; без приведения ключ вторичного
индекса не совпадал с поиском по Int (`get_items()` пуст, склад не
списывался). Теперь: `Value::mysql_numeric_prefix()` — единый числовой
префикс (Text/Dna); `Value::coerce_for_column()` — приведение к типу
колонки в INSERT/UPDATE (non-strict `sql_mode=''`); приведение литералов
поиска PointGet/IndexScan на чтении. Юнит-тест
`hpos_string_insert_into_bigint_indexed_column`. Корпус 71 → 78
(+7 HPOS-запросов, +2 фикстуры HPOS-легаси); дифф-прогон №6:
MATCH=76, BOTH-ERR=2 (`docs/cms/COMPARISON_RUN6.md`). Тестов: 262 с фичей.

### Прогон №5 + реальный WordPress 7.1.4 (2026-11-02)

**Фаза 1, приёмка: установка WordPress выполнена.** `wp core install` на
DNA-DB через drop-in db.php — Success; артефакты (12 таблиц, 125 опций,
пользователи/посты/меты, LAST_INSERT_ID, FOUND_ROWS) совпадают с эталоном
MariaDB 10.11.11 — детали в `docs/cms/E2E_WP_REPORT.md`.
E2E вскрыл и закрыл: Bearer-заголовок drop-in, TLS/стрим-транспорт,
числовое приведение `WHERE ID = '2'`, `SELECT DISTINCT`, comma-FROM
(→ cross join), `NOT IN`. Корпус 66 → 71; дифф-прогон №5: MATCH=69,
BOTH-ERR=2 (`docs/cms/COMPARISON_RUN5.md`). Тестов: 261 с фичей.

### Прогон №3: что закрыто (2026-11-02)

Арифметические ключи `ORDER BY` (`SelectItem::OrderKeyOnly` —
материализуются перед сортировкой, выходной колонки не дают), числовое
приведение строк в арифметике (числовой префикс MySQL), порядок колонок
по декларации в звёздных проекциях. Корпус расширен 63 → 66
(мульти-row INSERT, две проверки числовой сортировки); юнит-тесты
cms-модуля 13 → 15. Обе сборки зелёные: 190 тестов без фичи, 229 с
`--features cms`.

### Прогон №2: что закрыто (2026-11-02)

`CAST(x AS SIGNED|UNSIGNED|CHAR)` (числовой префикс, округление, NULL),
`COUNT(DISTINCT col)` (проекции/HAVING/ORDER BY), арифметика в
`UPDATE SET` (`stock_quantity = stock_quantity - 1` по старой строке),
ci-проекции + errno 1054, `VERSION()` → `10.11.11-MariaDB`, SHOW INDEX в
14-колоночной форме MariaDB (`Index_comment`/`Ignored`).

### Прогон №9: mysqldump / mariadb-dump roundtrip — Фаза 3 открыта (2026-11-06)

Миграция MySQL ⇄ DNA-DB штатным дампером (первый приёмочный пункт Фазы 3).
Направление IMPORT: дамп с эталона MariaDB 10.11.11 (условные комментарии,
LOCK TABLES, DISABLE KEYS, extended INSERT с `\0 \' \\`-эскейпами, префиксные
и составные ключи, табличные опции) проигрывается в DNA-DB клиентом
`mariadb`. Направление EXPORT: `mariadb-dump` подключается к DNA-DB как к
MySQL (SHOW CREATE TABLE, SQL_NO_CACHE, пробы information_schema.FILES/
TRIGGERS) и дамп проигрывается в чистую MariaDB — реконструированный гейтом
DDL валиден для MariaDB. Содержимое ref→imp→back совпадает побайтно
(кириллица, эмодзи, blob-байты, DECIMAL-шкала, NULL/'', порядок колонок),
автоинкремент после импорта продолжает с max+1 (99 → 100,
LAST_INSERT_ID корректен). Ядро: `unwrap_versioned_comments` (версионный
гейт 101111, строки не трогаются), `strip_fk_constraints`, LOCK/UNLOCK
TABLES + FLUSH + DISABLE/ENABLE KEYS no-op, SHOW CREATE TABLE /
CREATE DATABASE / TABLE STATUS / пустые каталоги рутин, `table_options` в
оверлее, бэктики в `parse_create_table`, байтовые смещения в парсере тела
DDL (не-ASCII в DEFAULT безопасны). Детали:
`docs/cms/E2E_MYSQLDUMP_ROUNDTRIP_REPORT.md`. Тестов: 316 с фичей (+4),
195 без — main-сборка не тронута.

### Прогон №10: бэкап/рестор CMS-базы — Фаза 3, пункт 2 (2026-11-06)

Операторский цикл «сайт на DNA-DB»: бэкап штатным `mariadb-dump` →
DROP DATABASE → рестор тем же дампом → сайт живой. Направление бэкапа —
С DNA-DB (SHOW CREATE TABLE из оверлея), рестор — в DNA-DB и в ванильную
MariaDB (no vendor lock-in). Идемпотентность: повторный рестор поверх
восстановленного (DROP TABLE IF EXISTS в дампе) — содержимое совпадает;
автоинкремент после рестора продолжает с max+1 (99 → 100, LAST_INSERT_ID
корректен); CMS-смоук через mysqli после рестора — OK. Эталон — Debian-
сборка **MariaDB 10.11.18** (в отличие от bintar 10.11.11 Прогона №9):
дампер ≥ 10.11.18 открывает дамп sandbox-маркером `/*M!999999\- enable the
sandbox mode */` — вскрыт и закрыт пробел гейта: `/*M!NNNNN … */`
(MariaDB-форма versioned-комментария) теперь проходит единый версионный
гейт с `/*!NNNNN … */` (999999 → comment-only → OK, реальные директивы
≤ уровня — исполняются). Операторские обёртки `scripts/cms_backup.sh` /
`scripts/cms_restore.sh [--create]`. Наблюдение: дисплейная шкала
SUM-агрегатов через mariadb-клиент (`250.1` vs `250.10`; данные целы,
колонка читается со scale) — не регрессия, ушло в хвосты Фазы 3. Детали:
`docs/cms/E2E_BACKUP_RESTORE_REPORT.md`. Тестов: 332 с фичей, 195 без
(+4 известных отказа cms-батарей — база не тронута).

### Прогон №11: спирали dnadb + веб-панель баз — Фаза 3, пункт 3 (2026-11-07)

Per-базовая dual-spiral архитектура и панель управления (запрос продукта):

- **Две спирали на каждую БД.** `CREATE DATABASE` (SQL или панель) физически
  разворачивает пару спиралей — Strand A (первичная) + Strand B (вторая) —
  каждая из них реальный, движком инициализированный каталог данных с
  `spiral_manifest.json` (pair_id/роль/тип хранилища/версия движка).
  По умолчанию корни `local-a`/`local-b` внутри data-dir; панель задаёт
  любой корень или абсолютный путь на каждое хранилище. Каталог пар —
  `spirals.json` (атомарная запись temp+rename), `DROP DATABASE` снимает
  обе спирали с диска и из каталога.
- **Хранилища: локальные диски или сетевые.** Панель детектит реальные
  маунты из `/proc/mounts` (ext4/xfs/btrfs/zfs/… = локальные, nfs/cifs/… =
  сетевые) с write-пробой; корни регистрируются вручную, годится любая
  сетевая точка монтирования. Перенос спирали между хранилищами на живом
  сервере: copy → verify манифеста → swap → обновление каталога → удаление
  старой копии. Имя БД валидируется `[A-Za-z0-9_$]` (path traversal
  невозможен).
- **Веб-панель** (`mysqld --admin-port N [--admin-bind …]`): embedded
  SPA (`cms::panel`, русский UI, ноль новых зависимостей), bearer-токен на
  12 ч из того же `--user root:pass`, 5 неудач/мин/IP → 429. API:
  базы (list/create/delete/relocate), хранилища (list/add/remove +
  кандидаты), статус. SQL-путь и панель ведут один каталог — SHOW
  DATABASES видит оба источника.
- **Фикс SUM-шкалы** (закрывает наблюдение Прогона №10): unaliased
  агрегаты `SUM/MIN/MAX(col)` резолвятся к scale аргумента через текст
  выражения, `AVG` расширяет на 4 (правило MySQL); mariadb-клиент теперь
  печатает `250.10` (MYSQL_TYPE_NEWDECIMAL, decimals=2 в обоих протоколах).
  Выражения без plain-колонки (`SUM(a+b)`) остаются DOUBLE — задокументировано как
  расхождение. Тест `decimal_aggregate_scale_parity`.

E2E `scripts/e2e_spirals_panel.sh`: панель создаёт базу с A на local-a и
B на сетевом пути → две манифест-спирали; SQL CREATE/DROP делает то же на
дефолтных корнях; SHOW DATABASES видит обе; relocate B → nas подтверждён
манифестом; SUM → 250.10. Регрессии roundtrip и бэкап/рестор зелёные
(CREATE/DROP DATABASE внутри них теперь строит/снимает пары). Тестов:
341 с фичей (+9: spirals ×5, gate-спирали ×1, SUM ×1, panel ×2), 195 без —
база не тронута. Движок: `cms::spirals` (каталог), `cms::panel` +
`panel.html` (панель), гейт: attach_spiral_catalog + panel_*-API,
mysqld: `--admin-port/--admin-bind`. Ограничение (задокументировано в
spirals.rs): табличные данные активного каталога живут в основном data-dir
движка; пары спиралей — per-БД слой хранения, к которому подключается
per-БД движок Фазы 4.

### Прогон №12: раздел «Расхождения и ошибки» устранён (2026-11-10)

Закрыты все пункты разделов «Расхождения и ошибки» / «Документированные
расхождения» отчётов ветки, кроме сознательно архитектурных:

1. **Тексты ошибок = MariaDB** (COMPARISON_MYSQL_REPORT #15/#55,
   прогоны №7/№8): `mysql_parity_message` в гейте переписывает тексты
   движка в точные формулировки MariaDB при том же errno — 1062
   `Duplicate entry '<v>' for key '<idx|PRIMARY>'`, 1091 ``Can't DROP
   COLUMN `c`; check that it exists`` / ``Can't DROP `i`; check that
   column/key exists`` (DROP INDEX несуществующего теперь тоже 1091, а
   не общий 0/HY000), 1054 `Unknown column 'c' in '<контекст>'`. Тест
   `error_message_parity_1062_1091`. VERSION() остаётся ожидаемым
   отличием (эмуляция 10.11.11 против живой сборки эталона).
2. **ON UPDATE CURRENT_TIMESTAMP** (E2E_MYSQLDUMP_ROUNDTRIP #1): флаг
   в оверлее, SHOW CREATE TABLE воспроизводит, Extra = `on update
   current_timestamp()`, UPDATE без явного назначения бампит колонку
   до NOW() (rewrite SET-секции в гейте). Тест
   `on_update_current_timestamp_semantics`.
3. **SHOW TABLE STATUS Rows** (там же #3): точный COUNT(*) вместо 0
   (phpMyAdmin видит живое число строк). Тест
   `table_status_reports_row_count`. Data_length остаётся
   InnoDB-плейсхолдером (байтовой статистики движок не ведёт).
4. **SUM(выражение)** (E2E_BACKUP_RESTORE): шкала = MAX среди
   десятичных колонок выражения; попутно устранён двигательный баг —
   `collect_agg_items` не собирал `AggOverExpr`, из-за чего
   `SELECT SUM(a+b) FROM t` возвращал пустые строки вместо одной
   агрегированной. Тест `decimal_aggregate_scale_parity`.
5. **FOUND_ROWS() = полный счёт** (§4 item 4): при SQL_CALC_FOUND_ROWS
   запрос перепроигрывается без топ-левел LIMIT/блокировок; приближение
   «счёт по странице» устранено. Тест
   `sql_calc_found_rows_unlimited_count`.
6. **Онлайн-смена типа** (§4 item 6): `ALTER TABLE … MODIFY [COLUMN]`
   и `CHANGE [COLUMN] old new` — метаданные + один батч coerced-переписки
   данных, переименование переносит вторичные индексы; DnaQL-пасcтру
   и SQL→DnaQL-трансляция дополнены MODIFY/CHANGE (паритет команд);
   оверлей синхронизируется на все ALTER-колоночные операции (раньше
   ALTER оставлял SHOW-вывод в состоянии CREATE — dbDelta видел
   устаревший набор колонок). Тест `alter_modify_change_column_parity`.

Остаются (осознанно, не «расхождения»): коллации utf8mb4_*_ci —
регистро-независимое сравнение WHERE/GROUP BY (движковая задача
Фазы 4), unsigned-семантика при записи, `LOCK TABLES`-no-op,
вырезание FK-сегментов DDL, inline `/* … */`, `--single-transaction`,
VERSION()-эмуляция. Тестов (lib): **324 с `--features cms`** (+5 новых:
паритет ошибок, TABLE STATUS, ON UPDATE, FOUND_ROWS, MODIFY/CHANGE),
**210 без фичи** — обе сборки 0 отказов (грамматика SQL общая: парсер
MODIFY/CHANGE и coerce_for_column живут вне cms-гейта; полный набор со
всеми целями — 357 passed с фичей).

## 5. Фазы

**Фаза 0 — корпус запросов (1–2 нед).** ✅ Выполнено:
`docs/cms/wp_queries_corpus.txt` — канонический набор SYS/OPTIONS/POSTS/
TAXONOMY/USERS/COMMENTS/DDL/WOO-запросов; это ТЗ транслятора и критерий
полноты.

**Фаза 1 — WP drop-in MVP (4–8 нед).** 🔶 В прогрессе: диалект-гейт
`cms::compat`, AUTO_INCREMENT, errno-маппинг, SHOW-семейство с
dbDelta-совместимым выводом, прототип `cms/php/wp-content/db.php` и
**установка WP 7.1.4 через wp-cli** — готовы; остаётся: wp-admin в браузере,
phpunit ядра WP, ALTER TABLE (частично готов), information_schema.
Приёмка: установка WP 6.x на DNA-DB ✅ (7.1.4, см. E2E-отчёт), wp-admin
работает 🔶, phpunit ядра WP ⬜.

**Фаза 2 — WooCommerce + Joomla (6–10 нед).** ✅ Приёмка выполнена:
**e2e-заказ WooCommerce ✅ (11.2.1, HPOS, артефакты = MariaDB, см.
E2E_WOO_REPORT)**, типы колонок при записи ✅, GET_LOCK/транзакции в
корпусе ✅, **строчные блокировки FOR UPDATE ✅ (1205/1213/3572/SKIP
LOCKED, §4 item 2, см. «Строчные блокировки»)**, **deadlock-retry в живом
заказе ✅ (обвязка поверх 1213: два конкурентных заказа в противоположном
порядке локов, жертва 1213 ретраит всю транзакцию, обе роли — см.
E2E_DEADLOCK_RETRY_REPORT)**, **DECIMAL по scale ✅ (decimal(p,s) читается
фиксированной шкалой "250" → "250.00", metadata MYSQL_TYPE_NEWDECIMAL +
decimals=s в обоих протоколах, SUM хранит scale — §4 item 8)**,
установка Joomla 5 ✅ (5.4.10 через штатный mysqli → wire-адаптер:
установщик, фронтенд и админка — см. E2E_JOOMLA_MYSQLWIRE_REPORT),
**установка OpenCart ✅ (4.1.0.4 официальным CLI-установщиком,
storefront/admin 200, 159 таблиц — см. E2E_OPENCART_MYSQLWIRE_REPORT)**,
**установка Drupal ✅ (11.4.10 штатным web-установщиком через PDO MySQL →
wire: batch standard-профиля, homepage 200, админ-сессия, фикс
compound-ON outer joins — см. E2E_DRUPAL_MYSQLWIRE_REPORT)**, **установка
MediaWiki ✅ (1.43.11 официальным CLI-установщиком через mysqli → wire:
рендер, правка через maintenance/edit.php, API, MCR-цепочка — см.
E2E_MEDIAWIKI_MYSQLWIRE_REPORT)**. Итог приёмки: e2e-заказ ✅, установка
Joomla ✅ (wire; pgwire для Joomla больше не нужен), установки
OpenCart/Drupal/MediaWiki ✅ — **пять CMS зелёные на wire-адаптере,
deadlock-retry и DECIMAL-шкала закрыты — Фаза 2 выполнена**. Остатки
чек-листа §4 (сравнение коллаций, information_schema, онлайн-смена типа,
unsigned-семантика) переносятся в Фазу 3 как инженерные задачи без
приёмочных блокеров.

**Фаза 3 — эксплуатация (2–4 нед).** 🔶 В прогрессе: **mysqldump-совместимый
импорт/экспорт ✅ (6.11: реальный `mariadb-dump` 10.11.11 в обе стороны —
дамп с эталона MariaDB проигрывается в DNA-DB штатным клиентом `mariadb`,
дамп С DNA-DB проигрывается в чистую MariaDB; содержимое ref→imp→back
совпадает побайтно, автоинкремент продолжается с max+1 — см.
E2E_MYSQLDUMP_ROUNDTRIP_REPORT)**, **бэкап/рестор CMS-базы ✅ (6.11: бэкап
mariadb-dump с DNA-DB → DROP DATABASE → рестор в DNA-DB — включая
идемпотентный повтор и автоинкремент с max+1, тот же бэкап встаёт и на
ванильную MariaDB; sandbox-маркер `/*M!999999*/` дамперов ≥ 10.11.18;
обёртки cms_backup.sh / cms_restore.sh — см.
E2E_BACKUP_RESTORE_REPORT)**, **спирали dnadb + веб-панель баз ✅ (7.11:
каждая БД — пара спиралей Strand A + Strand B на выбранных хранилищах
(локальные диски/сетевые монтирования), CREATE/DROP DATABASE через SQL и
панель физически разворачивают/снимают пары, перенос спирали на живом
сервере, SUM-шкала агрегатов — см. E2E_SPIRALS_PANEL_REPORT и
CMS_EDITION_RU.md §Прогон №11)**. Гейт дополнен: разворачивание
условных комментариев `/*!NNNNN … */`, LOCK/UNLOCK TABLES + DISABLE/ENABLE
KEYS + SQL_NO_CACHE как no-op/стриппинг, `SHOW CREATE TABLE` (реконструкция
из оверлея с табличными опциями; идемпотентен parse→emit→parse),
`SHOW CREATE DATABASE`, `SHOW TABLE STATUS`, пустые каталоги
триггеров/рутин, `information_schema.FILES/TRIGGERS/EVENTS/ROUTINES`,
`@@collation_database` и пр. Остаются: docker-образ,
гайд «миграция WP с MySQL за 3 шага», Prometheus-дашборд для CMS-метик,
инженерные хвосты Фазы 2 (коллации, unsigned). Дисплейная шкала
SUM-агрегатов закрыта Прогоном №11; раздел «Расхождения и ошибки»,
SUM-выражения, FOUND_ROWS-семантика, SHOW TABLE STATUS Rows,
ON UPDATE CURRENT_TIMESTAMP и онлайн-смена типа (MODIFY/CHANGE) —
Прогоном №12.

**Фаза 4 — MySQL wire protocol (8–16 нед).** Полный вход B: тогда и
mysqli/PDO_mysql/phpMyAdmin работают сами. CMS-редакция становится
«drop-in заменой MySQL» в полном смысле.

## 6. Правила ветки

- База — `main`; rebase на main не реже чем раз в фазу.
- Всё CMS-специфичное — за feature `cms`: `cargo build --release --features cms`.
- Начиная с v4.3: **SQL-грамматика общая для обеих сборок** — MySQL-диалект
  парсера (типы/длины/флаги, составные и inline PRIMARY KEY, KEY/INDEX-строки,
  table options, `LIMIT n,m`, `FOR UPDATE`) больше не за фича-гейтом; `cms`
  гейтит только wire-диалекты и шимы. Батарея без фичи: **210 тестов, 0
  отказов** (прежние «4 известных cms-гейтованных отказа» устранены, а не
  скрыты), 324 с `--features cms` (lib; полный набор со всеми целями — 357).
- При зрелости фазы 2 — обсуждение слияния `cms` в main как опциональной
  фичи; ветка остаётся для фаз 3–4.
- CI ветки: cargo test с `--features cms` + сборка без неё.

## 7. Сборка CMS-редакции

```bash
cargo build --release --features cms   # бинарь CMS-редакции
cargo test --features cms              # полный набор + cms-тесты
```

Реализовано (2026-11): режим `mysqld` того же бинаря —
`dna-db-core mysqld` (универсальный MySQL-шлюз, см. §3.1). Отдельный
бинарь `dna-db-cmsd` больше не планируется: не нужен.
