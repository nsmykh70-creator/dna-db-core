# DNA-DB CMS Edition — E2E: deadlock (errno 1213) + Woo-style retry поверх wire-адаптера

Дата: 6 ноября 2026. Ветка: `cms-edition`. Драйвер:
`scripts/e2e_deadlock_retry.sh` → `scripts/e2e_deadlock_retry.php`
(статический PHP 8.3.28, штатный mysqli + prepared statements) против
`dna-db-core mysqld :13307` (prebuilt, feature `cms`).

## Что проверяется

Последний приёмочный пункт Фазы 2: **deadlock-retry в живом заказе** —
конкурентные заказы WooCommerce-формы на DNA-DB обязаны вести себя как на
InnoDB: waits-for цикл → errno 1213 (SQLSTATE 40001) жертве → приложение
ретраит транзакцию целиком → артефакты заказов сходятся без дублей и
потерь.

Сценарий (обе стороны — чистый wire, без drop-in'ов):

1. `inventory` (inline `PRIMARY KEY`: `sku varchar(20) NOT NULL PRIMARY
   KEY`) со строками SKU-A/SKU-B (по 10) + `orders` (AUTO_INCREMENT id).
2. Две сессии (main + отдельный worker-процесс) в ОДНОЙ транзакции каждая:
   `SELECT qty FROM inventory WHERE sku = ? FOR UPDATE` по ДВУМ строкам в
   противоположном порядке (classic checkout deadlock), затем INSERT в
   `orders` и `UPDATE inventory SET qty = qty - 1` по обеим.
3. Координация по файлам гарантирует перекрытие холдов; тайминг «кто
   замыкает цикл» управляется паузой → движок назначает жертву предсказуемо.
4. Обвязка `tx_with_retry` (Woo-паттерн): на 1213/1205/3572 — ROLLBACK,
   случайный бэкофф, до 10 попыток; прочие errno — немедленный отказ.

## Форма транзакции: холды строго до DML

Гейт CMS-редакции — single-writer с read-your-writes: DML применяется к
движку после КАЖДОГО оператора, ROLLBACK не разворачивает уже применённые
операции (нет MVCC-снапшота — документировано в `execute()`). Поэтому
транзакция заказа держит форму «**все `FOR UPDATE`-холды → потом весь
DML**»: дедлок поражает write-free фазу, жертва откатывается без эффектов,
retry применяет заказ ровно один раз. (На InnoDB порядок не важен — там
откат честный; здесь он важен.) Для WooCommerce это соответствует
реальности: `wc_reduce_stock_levels` сначала читает локирующим SELECT'ом
все строки позиции, потом пишет.

## Результаты

| Фаза | Ожидаемая жертва | 1213 у жертвы | Ретраев | Заказы | Склад | Вердикт |
|---|---|---|---|---|---|---|
| 1 | worker | 1 | 1 | 4 | 8/8 | PASS |
| 2 | main | 1 | 1 | 4 | 8/8 | PASS |

- Фаза 1: worker держит SKU-B и ждёт, main берёт SKU-A и первым идёт на
  SKU-B; запрос worker'а на SKU-A замыкает цикл → worker — жертва, его
  обвязка ретраит → заказы сходятся.
- Фаза 2: worker идёт на SKU-A немедленно → замыкает цикл запрос main'а →
  жертва — main (проверка retry-обвязки на обеих ролях).
- Артефакты каждой фазы: 4 заказа (каждая сессия заказала каждый SKU ровно
  один раз — по одной строке orders на пару сессия×SKU), склад 10 → 8/8,
  0 неожиданных ошибок, счётчик 1213 у ожидаемой жертвы ≥ 1.
- Журнал сервера: `session closed (N lock(s) released)` — холды живут до
  COMMIT/ROLLBACK и снимаются им (§4 item 2).

## Попутный фикс: inline PRIMARY KEY в оверлее

E2E вскрыл: `CmsTableMeta::parse_create_table` признавал только табличную
форму `PRIMARY KEY (cols)`; inline `sku varchar(20) NOT NULL PRIMARY KEY`
оставлял `primary_key` пустым → `lock_select_rows`/`write_target_probe`
молча отключали строковые локи (и PointGet) на таких таблицах. Парсер
дополнен inline-формой (юнит-тест `inline_primary_key_column_constraint`).
CMS-DDL с табличной формой не затронуты.

## Как воспроизвести

```bash
bash scripts/e2e_deadlock_retry.sh
# ожидание: {"phases":[{...,"verdict":"PASS"},{...,"verdict":"PASS"}],"verdict":"PASS"}
```

Требует только prebuilt-бинарь и статический PHP (пути — переменные
`DNA_BIN`/`PHP` внутри скрипта).
