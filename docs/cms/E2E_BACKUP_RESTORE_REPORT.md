# DNA-DB CMS Edition — E2E: бэкап/рестор CMS-базы (backup → disaster → restore)

Дата: 6 ноября 2026. Ветка: `cms-edition`. Драйвер:
`scripts/e2e_backup_restore.sh` → `scripts/mysqldump_roundtrip_harness.php`
(режимы fixture/compare/probe) + `scripts/cms_restore_smoke.php` (статический
PHP 8.3.28, mysqli) против эталона **MariaDB 10.11.18** (Debian 12, userland из
.deb; :13306) и `dna-db-core mysqld` (feature `cms`, :13307). Реальные бинари
`mariadb-dump` / `mariadb` — никаких самодельных дамперов.

Второй приёмочный пункт Фазы 3 («эксплуатация»): операторский цикл жизни
CMS-сайта на DNA-DB — регулярный бэкап штатным дампером, потеря продакшена,
восстановление и проверка того, что сайт продолжает работать, а бэкап не
замыкает на себе (восстанавливается и в ванильный MySQL/MariaDB).

⚠️ Отличие окружения от Прогона №9: там эталоном был bintar MariaDB
10.11.11, здесь — Debian-сборка **10.11.18**. Дампер ≥ 10.11.18 открывает
каждый дамп строкой `/*M!999999\- enable the sandbox mode */`
(MariaDB-специфичный versioned-комментарий), которой в 10.11.11 не было —
это вскрыло пробел гейта, закрытый в этом прогоне (см. ниже).

## Что проверяется

1. **Продакшен.** Фикстура Прогона №9 (3 таблицы, 12 строк: bigint unsigned
   AUTO_INCREMENT с «дырками», decimal(10,2), blob-байты `00 27 01 5c`,
   кириллица/эмодзи/кавычки/переводы строк, NULL, префиксные и составные
   ключи) импортируется в DNA-DB (база `wp_prod` — «сайт, переехавший на
   DNA-DB»); та же фикстура разворачивается на эталоне (`wp_ref`) как
   внешняя истина. Пре-дисастер сверка: `wp_ref == wp_prod` побайтно.
2. **Бэкап.** `mariadb-dump` подключается к DNA-DB как к MySQL и снимает
   артефакт `backup_wp_prod.sql` (SHOW CREATE TABLE из оверлея,
   SELECT SQL_NO_CACHE, LOCK TABLES, пробы information_schema).
3. **Дисастер.** `DROP DATABASE wp_prod` на DNA-DB; проверяется, что база
   исчезла (`SHOW DATABASES LIKE 'wp_prod'` пуст).
4. **Рестор (проход 1).** Дамп проигрывается штатным клиентом `mariadb`
   обратно в DNA-DB. Сверка с эталоном; CMS-смоук через mysqli
   (`cms_restore_smoke.php`): счётчики постов/меты, keyed-lookup
   `_thumbnail_id` = 42, чтение trash-поста — путь, которым рендерится
   страница CMS.
5. **Рестор (проход 2) — идемпотентность.** Тот же дамп проигрывается
   поверх уже восстановленной базы (реальный кейс: рестор на «полу
   затёртое» состояние; в дампе `DROP TABLE IF EXISTS` перед каждой
   таблицей). Сверка с эталоном повторно.
6. **Автоинкремент после рестора.** Проба `probe`: INSERT после
   восстановления продолжает с max(id)+1 (99 → 100), LAST_INSERT_ID()
   корректен — новые заказы/посты не коллапсируют в существующие id.
7. **Портабельность (no vendor lock-in).** Тот же артефакт
   `backup_wp_prod.sql` восстанавливается в чистую базу ванильной MariaDB
   (`wp_back`) без единой правки и сверяется с эталоном.

**Критерий приёмки:** все вердикты `FIXTURE_OK` / `COMPARE_MATCH` /
`RESTORE_SMOKE_OK` / `PROBE_OK` / `LID_OK`, финал
`BACKUP_RESTORE_E2E_VERDICT: OK`.

## Результат

```
FIXTURE_OK tables=3 posts=4 meta=4 rel=4
== wp_prod provisioned on dna (migrated CMS site)
HASH_OK wp_postmeta rows=4 | HASH_OK wp_posts rows=4 | HASH_OK wp_term_rel rows=4
COMPARE_MATCH tables=3                                   # pre-disaster
== backup = 5023 bytes, 3 insert stmts, 3 tables
== disaster: wp_prod dropped, production lost
== restore pass 1: rc=0
COMPARE_MATCH tables=3                                   # после рестора
RESTORE_SMOKE_OK posts=4 meta=4 thumbnail=42
== restore pass 2 (over restored db): rc=0
COMPARE_MATCH tables=3                                   # идемпотентность
generated_id=100  PROBE_OK id=100  LID_OK
COMPARE_MATCH tables=3                                   # ванильная MariaDB
BACKUP_RESTORE_E2E_VERDICT: OK
```

## Что добавлено в ядро (гейт `src/cms/`)

**`/*M!NNNNN … */` — MariaDB-форма versioned-комментария.**
`mariadb-dump` ≥ 10.11.18 первой строкой каждого дампа пишет sandbox-маркер
`/*M!999999\- enable the sandbox mode */`. Раньше форма `/*M!` попадала в
ветку plain-комментария и вырезалась целиком, оставляя пустой statement,
который доежал до ядрового парсера и отвечал errno 1064
(`unsupported statement starting at Eof`). Теперь:

- `dialect::unwrap_versioned_comments` распознаёт оба префикса — `/*!` и
  `/*M!` — и применяет к ним один и тот же версионный гейт
  (≤ 101111: тело исполняется, выше — вырезается). Sandbox-маркер 999999
  коллапсирует в comment-only statement, на который гейт отвечает OK
  (существующее правило «comment-only → OK» из Прогона №9);
- эмпирика клиента: клиент 10.11.18 сам срезает `\-` перед отправкой
  (сервер получает `/*M!999999 enable the sandbox mode */`) — покрыты обе
  формы: и сырая wire-форма с `\-`, и почищенная клиентская;
- реальные MariaDB-директивы в M!-форме (≤ уровня совместимости, например
  `/*M!100100 … */`) теперь честно исполняются, как на MariaDB 10.11.

Тесты: `versioned_comments_unwrap_to_inner_statement` расширен тремя
assert_eq (M!-маркер → пусто, M!-директива ≤ уровня → тело, сырая форма с
`\-`), `dump_flow_statements_answer_ok` — двумя gate-утверждениями (обе
формы маркера отвечают OK).

## Операторские скрипты

К пункту приёмки приложены две обёртки — процедура бэкап/рестора теперь
документирована кодом, а не только отчётом:

- `scripts/cms_backup.sh <host> <port> <user> <pass> <db> [outfile]` —
  снимает логический бэкап штатным `mariadb-dump`, печатает
  `BACKUP_OK file=… bytes=… tables=…`;
- `scripts/cms_restore.sh <host> <port> <user> <pass> <db> <file> [--create]` —
  проигрывает бэкап в DNA-DB, `--create` создаёт базу после дисастера,
  печатает `RESTORE_OK db=… tables=…`. Повторный запуск безопасен
  (идемпотентность проверена и в E2E, и на обёртке).

Смоук обёрток (отдельный от E2E прогон): бэкап базы `shop` (2 строки,
decimal 250.00/0.10, кириллица) → `DROP DATABASE` → `cms_restore.sh
--create` → содержимое совпадает, повторный рестор поверх — OK.

## Документированные расхождения

- **Дисплейная шкала агрегатов — ✅ УСТРАНЕНА (Прогон №11, 2026-11-07).**
  Было: `SELECT SUM(price)` mariadb-клиент рендерил как `250.1` тогда как
  MariaDB показывает `250.10` (агрегатная колонка не несла scale исходной
  колонки; воспроизводилось и на бинаре 08a7324 — не регрессия этого
  прогона). Исправлено в `cms::compat::decimal`: unaliased `SUM/MIN/MAX`
  резолвятся к scale аргумента, `AVG` расширяет на 4 (правило MySQL);
  mariadb-клиент печатает `250.10`, метаданные MYSQL_TYPE_NEWDECIMAL с
  decimals=2 в обоих протоколах. Тест: `decimal_aggregate_scale_parity`;
  E2E-подтверждение — `e2e_spirals_panel.sh` (шаг 6).
- **Шкала выражений `SUM(a+b)` — ✅ УСТРАНЕНА (2026-11-10).** Остаток
  предыдущего пункта: аргумент-выражение резолвится к MAX-шкале среди
  десятичных колонок, на которые он ссылается (правило MySQL:
  decimal×int сохраняет шкалу decimal), вложенные вызовы парсятся
  сбалансировано (`SUM(ROUND(price,2))`). Прежний рендер DOUBLE и
  поведение «пустые строки на SUM(выражение)» (движок не агрегировал
  аргументы-выражения — `collect_agg_items` не собирал `AggOverExpr`)
  устранены; тесты: `decimal_aggregate_scale_parity` (ветка
  `SUM(price * 2)` → `500.20`).
- **`--single-transaction`** дампером через гейт не проверялся; обёртка
  `cms_backup.sh` сознательно использует минимальный набор флагов,
  проверенный E2E. Непротиворечивость бэкапа на одиночном писателе даёт
  сама архитектура движка.

## Тесты

`cargo test --features cms` — 332 passed на момент прогона; после Прогона
№11 (спирали + панель + фикс SUM) — **341 passed, 0 failed**; без фичи —
195 passed + 4 известных отказа cms-гейтованных батарейных тестов
(без изменений, база ветки). Релизный бинарь пересобран и обновлён в
`prebuilt/linux-x86_64/` (SHA256SUMS обновлён).
