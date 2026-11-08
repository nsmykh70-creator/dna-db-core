# RELEASE NOTES v4.2.0 — ограничения сняты

Дата: 2026-11 · Ветка: `main` · Предыдущий релиз: v4.1.0

Цель релиза — устранить документированные «честные ограничения» из
[RELEASE_NOTES_v4.0.0](RELEASE_NOTES_v4.0.0.md) («Честные ограничения») и
[SQL_MANUAL_RU §3.5](SQL_MANUAL_RU.md). Всё, что входило в этот список и
поддавалось честной реализации, реализовано и покрыто тестами.

## Верификация v4.2.0

- `cargo test --release`: **217 lib + 31 integration тестов, 0 failed**
  (13 новых тестов: ALTER×3, окна×2, ROLLUP/CUBE×1, WITH RECURSIVE×1,
  IndexJoin×1, pgwire×5).
- SQL conformance (`scripts/sql_feature_test.py --skip-mysql --skip-postgres`):
  **32/32 ok, 0 unsupported, 0 mismatch, 0 error** — регрессий нет.
- Smoke протокола (`scripts/smoke_test.py`): **15/15 PASS**.
- Новый E2E расширенного протокола (`scripts/pgwire_extended_e2e.py`):
  **OK** — Parse/Bind/Describe/Execute с `$n`- и `?`-плейсхолдерами,
  NULL-биндинги, ошибки счётчика параметров, живая сессия psycopg2 (libpq):
  параметризованные SELECT/INSERT, `ALTER TABLE DROP COLUMN` через протокол.

## Что снято

### 1. ALTER TABLE: DROP / RENAME / MODIFY / CHANGE COLUMN

Было: только `ADD COLUMN` (metadata-only).

Стало — полный DDL-цикл колонки:

| Команда | Механика | Гарантии |
|---|---|---|
| `ADD [COLUMN] c T` | metadata-only, мгновенно (как в v3.9) | строки не переписываются, чтение новой колонки на старых строках = NULL |
| `DROP [COLUMN] c` | один атомарный WAL-батч: схема + все строки без поля + сметание индексов | колонка не «воскресает» при повторном ADD; PK- и FK-колонки защищены ошибками |
| `RENAME COLUMN a TO b` | атомарный батч: схема + перезапись строк + индексы | PK-переименование поддержано; FK других таблиц обновляются |
| `MODIFY [COLUMN] c T` | конверсия значений в памяти → атомарный батч | один неконвертируемый символ отменяет всё до записи |
| `CHANGE [COLUMN] a b T` | переименование + конверсия одним батчем | то же |

Матрица конверсии: NULL → NULL всегда; →INT: FLOAT округляется
(половина от нуля, MySQL-семантика), TEXT парсится как i64; →FLOAT: INT
расширяется, TEXT парсится как f64; →TEXT: любой Display-рендер (DNA
сохраняет цепочку нуклеотидов); →DNA: TEXT обязан быть непустой чистой
ACGT (то же правило, что у INSERT-литералов); →TIMESTAMP: значения
проходят без изменений. Смена типа PK-колонки не поддерживается (ключи
строк выводятся из значения PK) — честная ошибка.

### 2. pgwire: prepared statements (была ошибка 0A000)

Расширенный протокол PostgreSQL v3 реализован полностью: `Parse`/`Bind`/
`Describe`/`Execute`/`Close`/`Flush`/`Sync`, именованные и безымянные
стейтменты и порталы. Плейсхолдеры `$n` и `?` (JDBC-стиль; смешивать
нельзя), текстовый формат параметров, NULL через длину −1, двоичный формат
— явная понятная ошибка. После ErrorResponse остальные сообщения батча
пропускаются до Sync (поведение по спецификации). `Describe portal` для
SELECT выполняет чтение один раз и кэширует rowset для Execute.

Попутные фиксы протокола: `CommandComplete` теперь NUL-терминированный
(требование libpq — без этого psycopg2 отвергал ответ), тег `INSERT`
считает вставленные строки по `inserted_key`.

### 3. Оконные функции: рамки, агрегаты, поверх GROUP BY

Было: только `ROW_NUMBER/RANK/DENSE_RANK`, без рамок, запрещено сочетание
с GROUP BY/агрегатами.

Стало:
- агрегаты как оконные функции: `SUM/COUNT/AVG/MIN/MAX OVER (...)`;
- рамки `ROWS BETWEEN ... AND ...` — полная грамматика (`UNBOUNDED
  PRECEDING/FOLLOWING`, `n PRECEDING/FOLLOWING`, `CURRENT ROW`, шорткат
  `ROWS <bound>`); `RANGE` — для `UNBOUNDED PRECEDING / CURRENT ROW /
  UNBOUNDED FOLLOWING`; value-offset RANGE-рамки — честная ошибка;
- стандартные дефолты: без ORDER BY — вся партиция; с ORDER BY —
  `RANGE UNBOUNDED PRECEDING..CURRENT ROW` с пирами;
- окна вычисляются **после** агрегации (стандартный порядок SQL), поэтому
  работают поверх GROUP BY; ссылки в PARTITION BY / ORDER BY могут быть
  агрегатами: `RANK() OVER (ORDER BY SUM(total_usd) DESC)`;
- вложенные агрегаты `SUM(SUM(x)) OVER ()` — внутренний агрегат считается
  групповым; без GROUP BY — честная ошибка (как в PostgreSQL).

### 4. ROLLUP / CUBE

`GROUP BY a, b WITH ROLLUP` (MySQL), `GROUP BY ROLLUP (a, b)` и
`GROUP BY CUBE (a, b)` (стандарт SQL). Супер-агрегатные строки несут
NULL-маркеры в свёрнутых колонках, последний набор — гранд-итог. CUBE
ограничен 12 колонками (4096 наборов) — честная ошибка сверх лимита.
HAVING и ORDER BY применяются к каждой строке результата как обычно.

### 5. WITH RECURSIVE

`WITH RECURSIVE name AS (base UNION [ALL] recursive) body`. Базовое плечо
выполняется один раз, рекурсивное — итеративно, и видит только дельту
прошлого раунда (стандартная семантика). `UNION` дедуплицирует полные
строки — циклы в данных завершаются сами; `UNION ALL` — нет, поэтому
защищён жёсткими лимитами: 10 000 раундов / 1 000 000 строк, дальше —
понятная ошибка вместо бесконечного цикла. Определение без UNION
отвергается на парсинге.

### 6. Индексные JOIN-планы (IndexJoin)

INNER/LEFT соединения по равенству используют `IndexJoin` — пробу
вторичного индекса (или точечное чтение по PK) на каждую строку внешней
стороны — когда правая таблица индексирована по колонке соединения и
заметно больше левой (эвристика стоимости `|left|·8+16 < |right|`).
RIGHT/FULL остаются на симметричных hash-планах. Семантика слияния строк
идентична hash-join (включая `joined_<col>` при конфликте имён и
NULL-расширение LEFT). План всегда виден: `IndexJoin(...)`/`HashJoin(...)`
в `execution_plan`.

## Что осталось честно (вне рамок релиза)

- **Raft**: одна группа, без шардирования; снапшотов/компакции лога нет;
  линейные записи, чтения на лидере. (Изменение требует отдельного
  проекта с crash-тестами.)
- **S3-клиент**: SigV4 без multipart — сегменты ≤ 5 ГБ укладываются.
- **LATERAL** и коррелированные производные таблицы в FROM.
- **Value-offset RANGE-рамки** (`RANGE BETWEEN n PRECEDING ...`).
- **Смена типа PK-колонки** (пересоздание таблицы вместо этого).

## Тесты

13 новых автотестов покрывают каждую снятую опцию:
`alter_table_drop_column_hides_and_sweeps_index`,
`alter_table_rename_column_rewrites_rows_and_indexes`,
`alter_table_modify_column_converts_values`,
`window_aggregates_frames_and_running_totals`,
`window_functions_over_group_by_result`, `group_by_with_rollup_and_cube`,
`with_recursive_walks_hierarchy_and_dedupes_cycles`,
`index_join_plan_for_small_left_side` (sql_engine), `placeholder_counting`,
`parameter_substitution`, `bind_message_roundtrip`, `parse_message_roundtrip`,
`select_shape_detection` (pgwire).
