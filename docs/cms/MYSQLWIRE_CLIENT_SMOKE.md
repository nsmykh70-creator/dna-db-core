# MySQL wire: smoke реальных клиентов против DNA-DB (2026-11)

Сервер: `dna-db-core mysqld --data-dir /tmp/dna-mysql-smoke --port 13307
--bind 127.0.0.1 --wordpress --user wp:wp_pass` (feature `cms`, ветка
`cms-edition`). Клиенты — два независимых драйвера MySQL:

- **PyMySQL 1.2.3** — чистый Python, текстовый протокол (как wpdb/mysqli);
- **Connector/Py 26.10.0** (Oracle) — серверные prepared statements
  (COM_STMT_PREPARE/EXECUTE) при каждой параметризованной выборке.

Скрипт: `scripts/mysqlwire_client_smoke.py`. Результат: **21/21 PASS**.

## PyMySQL (текстовый протокол)

| Проверка | Результат |
| --- | --- |
| SELECT 1 | PASS |
| VERSION() = 10.11.11-MariaDB | PASS |
| multi-row INSERT, affected=2 | PASS |
| LAST_INSERT_ID() = 1 → 3 | PASS |
| WHERE + ORDER BY (DEFAULT-колонка) | PASS |
| CONCAT(post_title, ' #', ID) | PASS |
| UPPER(col) | PASS |
| IFNULL / COALESCE / IF | PASS |
| UPDATE / DELETE affected=1 | PASS |
| SELECT из отсутствующей таблицы → errno 1146 | PASS |
| SHOW FULL COLUMNS (dbDelta-форма) | PASS |
| SHOW INDEX | PASS |
| utf8mb4: CONCAT('днк','-тест') | PASS |

## Connector/Py (бинарный протокол, COM_STMT_*)

| Проверка | Результат |
| --- | --- |
| prepared SELECT WHERE id = ? | PASS |
| prepared INSERT (affected=1, LAST_INSERT_ID()=4) | PASS |
| read-back через PS | PASS |
| отсутствующая таблица → errno 1146 на PREPARE | PASS |
| ping() (COM_PING) | PASS |

## Попутно найдено и исправлено (паритет с MySQL)

1. **Bare `COMMIT`/`ROLLBACK` вне транзакции** — реальный драйвер
   (autocommit=0) шлёт их после каждой записи; MySQL отвечает OK, гейт
   отвечал errno 1064 → теперь тихий no-op (`cms::compat`).
2. **PREPARE несуществующей таблицы** — сначала отдавали код 1295
   (ER_UNSUPPORTED_PS), MySQL отдаёт реальный errno 1146/42S02 → проба
   пробрасывает исходную ошибку (`cms::mysqlwire`).
3. **Функции SQL** — в движке не было вызовов функций вообще; добавлен узел
   `Expr::Fn` с MySQL NULL-семантикой (CONCAT('a', NULL) → NULL и т.д.),
   см. `src/expr.rs::eval_scalar_fn`.

## Что осталось

- Дифференциальный прогон провода против живого MariaDB на тех же
  запросах (эталон недоступен в текущей песочнице; корпус и методика —
  `docs/cms/COMPARISON_RUN6.md`, движковый дифференциал уже 66/66).
- `caching_sha2_password` без AuthSwitch (RSA-обмен) — не требуется:
  клиенты переключаются на native прозрачно.
- information_schema (фаза 2 по плану).
