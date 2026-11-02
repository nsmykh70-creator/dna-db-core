# DNA-DB CMS Edition — WordPress / Joomla

Ветка `cms-edition`, feature-гейт `cms`. База: v4.1 (`main`, панель v4.1,
компакция, журнал компакций, Prometheus-гейджи).

## 1. Цель

Заставить популярные PHP-CMS работать на DNA-DB без переписывания приложений:

| Клиент | Версия | Точка входа | Приоритет |
|---|---|---|---|
| WordPress | 6.x | drop-in `wp-content/db.php` (замена `wpdb`) либо MySQL wire | 1 |
| WooCommerce | 8.x+ | тот же `wpdb` + транзакции/блокировки | 1 |
| Joomla | 5.x | `Joomla\Database\DatabaseDriver` (MySQLi/PDO MySQL/**PDO PgSQL**) | 2 |
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

**C. MySQL wire protocol (фаза 4, универсальный вход).** Handshake, auth
(`mysql_native_password`), text protocol, OK/ERR с errno из `cms::errno`,
COM_STMT_*. После него работают mysqli, PDO_mysql, phpMyAdmin, mysqldump
без всяких drop-in'ов.

## 4. Пробелы движка (чек-лист из аудита)

Состояние после фазы-1 каркаса и интеграции гейта (ветка `cms-edition`):

1. **AUTO_INCREMENT + `LAST_INSERT_ID()`** — ✅ готово: реестр
   последовательностей (`cms::sequence`, seed от MAX(pk), bump по явным id),
   поле `auto_increment_column` в схеме, назначение в `exec_insert_tx`,
   шим `SELECT LAST_INSERT_ID()` в гейте.
2. **`SELECT … FOR UPDATE` / deadlock-детект** — частично: синтаксис
   `FOR UPDATE/FOR SHARE` принимается (cms-грамматика), транзакции
   `BEGIN/COMMIT/ROLLBACK` живут в сессии гейта (буфер v3.6, ошибочный
   оператор не роняет транзакцию — паритет MySQL); построчные блокировки
   и коды 1213/1205 — фаза 2 (база: `tx.rs`).
3. **Коллации** `utf8mb4_unicode_ci`: сравнение без регистра, PAD SPACE,
   составные и префиксные индексы (meta_value 191) — префиксы сохраняются
   в оверлее, сравнение — фаза 2.
4. **`FOUND_ROWS()` / `SQL_CALC_FOUND_ROWS`** — ✅ готово в гейте
   (приближение: счёт по странице, не по запросу без LIMIT).
5. **Каталог**: `SHOW TABLES LIKE`, `SHOW FULL COLUMNS`, `SHOW INDEX`
   (14 колонок, в форме MariaDB), `DESCRIBE`, `SHOW VARIABLES` — ✅ готово
   через оверлей (`cms::ddl_meta`: MySQL-точный `Type`, чтобы dbDelta не
   видел расхождений; CREATE TABLE через гейт регистрирует оверлей
   автоматически) + fallback из схемы движка; `information_schema` — фаза 2.
6. **ALTER TABLE**: ADD COLUMN (с NOT NULL/DEFAULT → эффективный дефолт),
   DROP COLUMN, CREATE/DROP INDEX — ✅ готово; изменение типа (онлайн) —
   фаза 2.
7. **Именованные блокировки** — ✅ готово: реестр `cms::locks` + SQL-шимы
   `GET_LOCK/RELEASE_LOCK/IS_FREE_LOCK/IS_USED_LOCK` в гейте.
8. **Типы**: BIGINT UNSIGNED/DECIMAL/DATETIME/ENUM — частично: имена
   принимаются и маппятся на базовые типы (cms-грамматика), семантика
   unsigned/decimal — фаза 2.
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
независимость WHERE/GROUP BY — фаза 2 (коллации); DECIMAL-форматирование
по scale — фаза 2. Полный отчёт: `docs/cms/COMPARISON_MYSQL_REPORT.md`.

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

**Фаза 2 — WooCommerce + Joomla (6–10 нед).** 🔶 В прогрессе: **e2e-заказ
WooCommerce ✅ (11.2.1, HPOS, артефакты = MariaDB, см. E2E_WOO_REPORT)**,
типы колонок при записи ✅, GET_LOCK/транзакции в корпусе ✅; остаётся:
MVCC/`FOR UPDATE` (errno 1213/1205 в живом заказе), deadlock-retry,
DECIMAL по scale, установка Joomla 5 (pgwire). Приёмка: e2e-заказ ✅,
установка Joomla ⬜.

**Фаза 3 — эксплуатация (2–4 нед).** mysqldump-совместимый импорт/экспорт,
бэкап/рестор, docker-образ, гайд «миграция WP с MySQL за 3 шага»,
Prometheus-дашборд для CMS-метрик.

**Фаза 4 — MySQL wire protocol (8–16 нед).** Полный вход B: тогда и
mysqli/PDO_mysql/phpMyAdmin работают сами. CMS-редакция становится
«drop-in заменой MySQL» в полном смысле.

## 6. Правила ветки

- База — `main`; rebase на main не реже чем раз в фазу.
- Всё CMS-специфичное — за feature `cms`: `cargo build --release --features cms`.
- Обычная сборка и 189 тестов main не должны меняться (сейчас: 224 теста
  с feature, main-сборка без изменений).
- При зрелости фазы 2 — обсуждение слияния `cms` в main как опциональной
  фичи; ветка остаётся для фаз 3–4.
- CI ветки: cargo test с `--features cms` + сборка без неё.

## 7. Сборка CMS-редакции

```bash
cargo build --release --features cms   # бинарь CMS-редакции
cargo test --features cms              # полный набор + cms-тесты
```

Планируемое имя бинаря при фазе 1: `dna-db-cmsd` (обёртка над main.rs с
пресетами WP/Joomla-схем и autocommit-профилем PHP-FPM).
