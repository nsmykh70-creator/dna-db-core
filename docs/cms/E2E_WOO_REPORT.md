# DNA-DB CMS Edition — E2E: заказ WooCommerce 11.2.1 (HPOS) на DNA-DB

Дата: 2 ноября 2026. Ветка: `cms-edition`. Критерий приёмки фазы 2 из
`docs/CMS_EDITION_RU.md` §5 — **«e2e-заказ WooCommerce» — выполнен**:
полный жизненный цикл заказа (товар → заказ → позиция → расчёт итога →
списание склада) на DNA-DB даёт артефакты, идентичные эталону
MariaDB 10.11.11.

## Методика

Одно ядро WordPress 7.1.4 + WooCommerce 11.2.1, два инстанса:

| Роль | Движок | Подключение |
|---|---|---|
| Эталон | MariaDB 10.11.11 (`--port 13306`) | `wpdb` → mysqli |
| Тестируемый | DNA-DB `cms-edition` (release, `--features cms`), admin-API `:8443` (TLS) | drop-in `wp-content/db.php` → `POST /api/query` |

Активация Woo (`wp plugin install woocommerce --activate`) сама по себе —
большой тест гейта: dbDelta создаёт ~40 таблиц Woo через диалект-слой
(на обеих сторонах — Success, время 4,6 с MariaDB / 4,4 с DNA-DB).

Батарея (`scripts/e2e_woo_battery.php`, один и тот же код через `wp eval-file`):
`WC_Product_Simple` с управлением складом → `wc_create_order(customer 1,
processing)` → `add_product` ×2 → billing-реквизиты (кириллица) →
`calculate_totals()` → `save()` → `wc_reduce_stock_levels()` → перечтение
артефактов через WC CRUD.

## Результат (DNA-DB после исправления — MATCH по всем артефактам)

| Артефакт | MariaDB | DNA-DB | Вердикт |
|---|---|---|---|
| product_id / order_id | 10 / 11 | 10 / 11 | MATCH (AUTO_INCREMENT-паритет) |
| price / order_total | 250.00 / 500.00 | 250.00 / 500.00 | MATCH |
| order_status / currency | processing / USD | processing / USD | MATCH |
| billing (Иван, Москва) | сохранены | сохранены | MATCH (utf8mb4) |
| item_count / items | 2 / qty 2, total 500 | 2 / qty 2, total 500 | MATCH |
| stock после заказа | 8 (10−2) | 8 (10−2) | MATCH |
| `wp_woocommerce_order_items` (1 строка) | идентична | идентична | MATCH |
| `wp_woocommerce_order_itemmeta` (10 мет) | `_product_id`…`_reduced_stock=2` | побайтно идентичны | MATCH |

Тайминги (wp-cli, single-shot, ориентировочно): товар 34/253 мс,
заказ 143/580 мс, склад 8/82 мс (MariaDB/DNA). Разрыв ~4–7× — цена
HTTP-транспорта drop-in'а (запрос = один HTTPS POST) и контролируемого
диалект-слоя; устраняется входом B (MySQL wire, фаза 4).

## Найдено и исправлено в ходе E2E

**Дефект: `get_items()` пуст, склад не списывается.** Первая батарея на
DNA-DB дала `items: []`, `stock_after: 10` при эталоне `2` и `8`.

Диагноз (внутрь наружу):

1. `wc_reduce_stock_levels()` не видит позиций → `get_items()` возвращает `[]`.
2. Сырое чтение: `SELECT … WHERE order_id = 15` на
   `wp_woocommerce_order_items` даёт 0 строк при существующей строке с
   `order_id = 15`; `WHERE order_id = '15'` находит; `order_id + 0 = 15`
   находит. План: `IndexScan(wp_woocommerce_order_items.order_id)` → пусто.
3. Корень: **типы колонок не применялись при записи.** `wpdb::insert()`
   без массива формата шлёт каждое значение СТРОКОЙ — HPOS-путь
   `WC_Order_Item_Data_Store` пишет `order_id` как `'15'` в
   `bigint unsigned`. Движок хранил `Text("15")`, ключ вторичного индекса
   кодировался из текста и никогда не совпадал с ключом поиска по
   `Int(15)` (и наоборот). MariaDB приводит значение к типу колонки при
   записи — и чтении.

Исправления (закрыты юнит-тестом `hpos_string_insert_into_bigint_indexed_column`):

- `Value::mysql_numeric_prefix()` (relational.rs) — единый источник
  числового префикса для Text/Dna (раньше три независимые копии, и
  `Value::compare` не рассматривал Dna-строки как числа);
- `Value::coerce_for_column()` — приведение значения к объявленному типу
  колонки на записи (INSERT в `exec_insert_tx`, UPDATE в `exec_update_tx`;
  нечисловой текст → 0, как в non-strict MySQL `sql_mode=''`);
- приведение литералов поиска к типу колонки на чтении (PointGet по PK и
  `IndexAccess::coerce_values` для Eq/Range-сканов).

## Регрессия в SQL-корпусе

Дифференциальный прогон №6 (корпус 78 запросов: +7 HPOS-запросов,
+2 таблицы `wp_woocommerce_order_items`/`…itemmeta` в фикстурах) против
той же MariaDB: **MATCH=76, BOTH-ERR=2** (безобидные, одинаковые errno
1062/1091 с обеих сторон) — см. `docs/cms/COMPARISON_RUN6.md`.
