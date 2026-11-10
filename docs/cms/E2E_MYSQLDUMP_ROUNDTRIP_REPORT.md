# DNA-DB CMS Edition — E2E: mysqldump / mariadb-dump roundtrip (import + export)

Дата: 6 ноября 2026. Ветка: `cms-edition`. Драйвер:
`scripts/e2e_mysqldump_roundtrip.sh` → `scripts/mysqldump_roundtrip_harness.php`
(статический PHP 8.3.28, mysqli) против **эталона MariaDB 10.11.11** (:13306)
и `dna-db-core mysqld :13307` (feature `cms`). Реальные бинари
`mariadb-dump` / `mariadb` из bintar 10.11.11 — никаких самодельных дамперов.

Первый приёмочный пункт Фазы 3 («эксплуатация»): миграция MySQL → DNA-DB и
обратно штатным инструментом, которым пользуются все хостинги.

## Что проверяется

**Направление 1 — IMPORT (MySQL → DNA-DB).** `mariadb-dump` снимает дамп с
эталона MariaDB; клиент `mariadb` проигрывает его в DNA-DB через wire-гейт.
Дамп содержит полный набор конструкций реального формата:

- условные комментарии `/*!40101 SET @OLD_… */`, `/*!32312 IF NOT EXISTS*/`,
  `LOCK TABLES t READ /*!32311 LOCAL */`, `SELECT /*!40001 SQL_NO_CACHE */ * …`;
- `DROP TABLE IF EXISTS`, `CREATE TABLE` с `KEY col(191)`-префиксами,
  составными ключами и табличными опциями `ENGINE=InnoDB AUTO_INCREMENT=……`;
- extended multi-row INSERT с эскейпами `\0 \' \" \\ \n \t \Z` и юникодом.

**Направление 2 — EXPORT (DNA-DB → MySQL).** `mariadb-dump` подключается к
DNA-DB как к обычному MySQL-серверу и снимает дамп (SHOW CREATE TABLE, SELECT
SQL_NO_CACHE, LOCK TABLES, пробы information_schema), после чего дамп
проигрывается в чистую базу MariaDB. Это проверяет, что реконструированный
гейтом `SHOW CREATE TABLE` — валидный MariaDB-DDL, а типы/данные передаются
без потерь.

**Критерий приёмки:** содержимое трёх баз (`dna_ref` на MariaDB → `dna_imp`
на DNA-DB → `dna_back` на MariaDB) совпадает побайтно по всем таблицам
(канонизация: колонки в порядке объявления, NULL-флаг, построчная сортировка
в PHP), и автоинкремент после импорта продолжается с max(id)+1.

## Фикстура (3 таблицы, 12 строк)

- `wp_posts` — bigint unsigned AUTO_INCREMENT с «дырками» (1, 7, 42, 99),
  decimal(10,2), datetime, int unsigned (4294967295), blob
  (`\x00 ' \x01 \\`), longtext с кириллицей/эмодзи/кавычками/переводами
  строк/табами, NULL-значения, префиксный `KEY post_name(191)`, составной
  `KEY type_status_date (post_status, post_date, ID)`;
- `wp_postmeta` — NULL-ключ meta_key, сериализованный PHP-массив,
  unicode-ключ `unicode_ключ`;
- `wp_term_rel` — составной PK `(object_id, term_taxonomy_id)` без
  автоинкремента.

## Результат

```
FIXTURE_OK tables=3 posts=4 meta=4 rel=4
== dump(ref) = 5003 bytes, 3 insert stmts
== import into dna: client rc=0
HASH_OK wp_postmeta rows=4   HASH_OK wp_posts rows=4   HASH_OK wp_term_rel rows=4
COMPARE_MATCH tables=3
== dump(dna) = 5020 bytes, 3 create tables
HASH_OK wp_postmeta rows=4   HASH_OK wp_posts rows=4   HASH_OK wp_term_rel rows=4
COMPARE_MATCH tables=3
generated_id=100  PROBE_OK id=100  LID_OK
MYSQLDUMP_E2E_VERDICT: OK
```

Побайтное совпадение на обоих направлениях, включая blob-байты
(`00 27 01 5c…`), DECIMAL-шкалу (`250.00`), NULL/''-различие и порядок
колонок. `SHOW CREATE TABLE` гейта переигрывается MariaDB без правок
(idempotency-тест в юнитах: `parse → emit → parse` даёт тот же текст).

## Что добавлено в ядро (гейт `src/cms/`)

| Компонент | Что делает |
|---|---|
| `dialect::unwrap_versioned_comments` | `/*!NNNNN body */` → `body` при NNNNN ≤ 101111 (10.11.11-MariaDB), выше — выкидывается; `/* … */` без `!` — отбрасывается; строковые литералы не трогаются |
| `dialect::strip_fk_constraints` | вырезает `FOREIGN KEY` / `CONSTRAINT …` / `CHECK`-сегменты из CREATE TABLE перед форвардом в движок (оверлей их уже пропускал) |
| `compat::execute` | `LOCK TABLES …` / `UNLOCK TABLES` / `FLUSH …` → OK-no-op; `ALTER TABLE … DISABLE\|ENABLE KEYS` → OK-no-op; `SQL_NO_CACHE` вырезается; пустой после unwrap запрос → OK-no-op (MySQL-паритет) |
| `shims_show::SHOW CREATE TABLE` | реконструкция DDL из оверлея (точные объявленные типы, DEFAULT-рендер в стиле MariaDB, бэктик-идентификаторы, табличные опции из исходного DDL), фолбэк — схема движка; нет таблицы → errno 1146. Парсер оверлея научился бэктикам — прямой вызов `parse_create_table` на выводе SHOW CREATE TABLE воспроизводит ту же мета |
| `shims_show::SHOW CREATE DATABASE` | хедер `--databases`-дампа |
| `shims_show::SHOW TABLE STATUS` | InnoDB-образные строки (Rows=0, автоинкремент из реестра+MAX(pk)) |
| `shims_show` пустые каталоги | SHOW TRIGGERS / EVENTS / FUNCTION STATUS / PROCEDURE STATUS (колоночная форма MariaDB), SHOW CREATE {TRIGGER\|PROCEDURE\|FUNCTION\|EVENT} → errno |
| `shims_select` | `@@collation_database`, `@@character_set_database`, `@@max_allowed_packet` |
| `catalog` (information_schema) | `FILES` (проба undo-tablespace дампера) и `TRIGGERS`/`EVENTS`/`ROUTINES`/`PARAMETERS` — пустые множества |
| `ddl_meta` | `CmsTableMeta.table_options` — сырой хвост DDL сохраняется и воспроизводится в SHOW CREATE TABLE; парсер тела переведён на байтовые смещения (`char_indices`, не-ASCII в DEFAULT/COMMENT безопасны) |

## Документированные расхождения

> **Статус 2026-11-10:** пункты 1 и 3 устранены (см. пометки), 2/4/5 —
> осознанные архитектурные решения, остаются задокументированными.

1. ~~`ON UPDATE CURRENT_TIMESTAMP` не входит в оверлей~~ — **✅ УСТРАНЕНО**:
   флаг парсится в `CmsColumnMeta.on_update_current_timestamp`, SHOW CREATE
   TABLE воспроизводит флаг, SHOW FULL COLUMNS показывает MariaDB-форму
   `on update current_timestamp()` в Extra, а гейт на каждом UPDATE,
   который не назначает колонку явно, дописывает `SET col = NOW()`
   (семантика MySQL; тест `on_update_current_timestamp_semantics`).
2. `LOCK TABLES` — no-op: single-writer-движок уже исключает параллельные
   записи; гранулярные гарантии дают строчные блокировки (§4 item 2).
3. ~~SHOW TABLE STATUS статистика (Rows/Data_length) нулевая~~ —
   **✅ Rows УСТРАНЕН**: Rows теперь точный `COUNT(*)` (phpMyAdmin видит
   живое число строк; тест `table_status_reports_row_count`); Data_length
   остаётся InnoDB-плейсхолдером страницы — движок не ведёт байтовую
   статистику. Автоинкремент честный (как было).
4. FOREIGN KEY-сегменты DDL вырезаются перед форвардом (референциальная
   целостность остаётся на декларативном FK-слое движка).
5. Внутристрочные блоки `/* … */` без `!` отбрасываются (паритет с
   лексером движка, где комментарии — whitespace).

## Тесты

Юниты гейта (`src/cms/compat/tests/dump.rs`, +4): unwrap versioned-комментариев
(внутри/вне строк, выше/ниже версии), OK-ответы всего дамперного потока,
roundtrip SHOW CREATE TABLE с идемпотентностью и errno 1146, CREATE TABLE с
FK/CHECK, пустые каталоги рутин + SHOW TABLE STATUS. Набор: **316 с
`--features cms`, 195 без** (4 известных cms-гейт-отказа батарейных тестов
без изменений — main-сборка не тронута).
