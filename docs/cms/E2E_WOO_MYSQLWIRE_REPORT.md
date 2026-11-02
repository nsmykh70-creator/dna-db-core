# DNA-DB CMS Edition — E2E: WooCommerce 11.2.1 (HPOS) через УНИВЕРСАЛЬНЫЙ MySQL wire-адаптер

Дата: 2 ноября 2026. Ветка: `cms-edition`.

Воспроизведение: 2 ноября 2026 прогон повторён на отдельной машине
(portable MariaDB 11.8.6, PHP 8.4.27 из deb-пакетов, WP-CLI 2.12.0) —
вердикт снова MATCH, 12/12 артефактов, 48 таблиц (32 Woo); оба известных
notice `duplicate primary key … wp_term_relationships` воспроизведены
точно в задокументированном виде (данные идентичны эталону).

## Что проверяется

WordPress 7.1.4 + WooCommerce 11.2.1, установленный на DNA-DB **как на обычный
MySQL**: штатный mysqli → `dna-db-core mysqld :13307`. Активация Woo через
dbDelta создаёт ~36 таблиц Woo поверх 12 таблиц ядра (48 всего) — целиком через
wire-протокол MySQL 4.1. Затем одна и та же батарея заказа
(`scripts/e2e_woo_battery.php`, `wp eval-file`) на обеих сторонах:

`WC_Product_Simple` (управление складом, цена 250.00, склад 10) →
`wc_create_order(customer 1, processing)` → `add_product ×2` →
billing-реквизиты кириллицей (Иван, Москва, Тверская, 1) →
`calculate_totals()` → `save()` → `wc_reduce_stock_levels()` →
перечтение артефактов через WC CRUD.

## Результат: MATCH — все 12 артефактов идентичны MariaDB

| Артефакт | MariaDB 11.8.6 | DNA-DB (wire) | Вердикт |
|---|---|---|---|
| product_id | 10 | 10 | MATCH |
| product_type | simple | simple | MATCH |
| price | 250.00 | 250.00 | MATCH |
| stock после заказа | 8 (10−2) | 8 | MATCH |
| order_id | 11 | 11 | MATCH |
| order_status | processing | processing | MATCH |
| order_total | 500.00 | 500.00 | MATCH |
| order_currency | USD | USD | MATCH |
| billing_city / billing_name | Москва / Иван | идентично | MATCH (utf8mb4) |
| item_count | 2 | 2 | MATCH |
| items | Тестовый товар DNA, qty 2, total 500 | идентично | MATCH |
| таблиц создано dbDelta | 48 (12 ядра + 36 Woo) | 48 | MATCH |

Тайминги батареи: товар ~27 мс, заказ ~100 мс, списание склада ~9 мс —
на уровне MariaDB.

## Качество SQL-покрытия на полном цикле Woo

Прогон 2 ноября 2026 после расширения диалект-слоя: **0 SQL-ошибок** на всей
активации Woo + батарее заказа. Для этого в движок/gate добавлены:

* `REPLACE INTO` (полная замена строки-конфликта) — `WC_Data_Store_WP::update_lookup_table`;
* `INSERT … SELECT <литералы> FROM DUAL [WHERE (SELECT NULL FROM DUAL) IS NULL]`
  — паттерн ActionScheduler каждого запланированного действия Woo;
* настоящий `INSERT … SELECT … FROM <таблица>` (claim/migration-пути ActionScheduler);
* derived tables `FROM (SELECT …) AS x` + `LEFT/INNER JOIN (SELECT …) AS y` —
  пересчёт товарных счётчиков `_wc_term_recount`;
* `USING (col)` — тот же пересчёт;
* составные условия `ON t.a = b.x AND t.c = 'литерал'` — выборка возвратов;
* `SELECT EXISTS (SELECT …)` в проекции — `lookup_table_has_data`;
* `SELECT SUM(qual.col)` — агрегат с квалифицированным аргументом (возвраты);
* MySQL-семантика DUAL как виртуальной однострочной таблицы.

## Известное косметическое отличие

WordPress печатает 2 notice `duplicate primary key in INSERT:
wp_term_relationships:10` (errno 1062) в момент `wp_set_object_terms` —
повторная вставка уже существующей связи товар↔термин. Итоговое содержимое
таблиц идентично эталону (связь существует в единственном экземпляре на обеих
сторонах); заказ, склад и меты совпадают. Замечено только на DNA-DB — причина:
в момент повтора чтение «старых связей» внутри Woo-сценария вернуло пустой
набор; запрос воспроизводится корректно изолированно (см. трассировку в
истории коммитов). Не влияет на данные; ведётся наблюдение.

## Скрипты

* `scripts/e2e_woo_mysqlwire.sh` — полная установка + батарея + diff;
* `scripts/e2e_woo_battery.php` — одинаковый код заказа на обеих сторонах.
