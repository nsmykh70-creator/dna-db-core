# DNA-DB CMS Edition — E2E: OpenCart 4.1.0.4 через универсальный MySQL wire-адаптер

Дата: 4 ноября 2026. Ветка: `cms-edition`. Версия движка: v4.0.3 (prebuilt, feature `cms`).

## Что проверяется

OpenCart 4.1.0.4 (последняя стабильная 4.x), установленная на DNA-DB **как на
обычный MySQL**: штатный mysqli из статического PHP 8.3.28 →
`dna-db-core mysqld :13306`. Драйвер — `mysqli`, host `127.0.0.1`,
отдельный `--db_port 13306` (OpenCart, в отличие от Joomla, принимает порт
конфигом), префикс `oc_`. Установка — штатным CLI-установщиком
(`install/cli_install.php`): он строит все таблицы из PHP-схемы
(`system/helper/db_schema.php`), опрашивает `information_schema.COLUMNS`
перед каждым CREATE, заливает демо-данные из `install/opencart-en-gb.sql`
и создаёт администратора.

## Результат: PASS

| Проверка | Результат |
| --- | --- |
| CLI-установка (`SUCCESS! OpenCart successfully installed`) | OK |
| Фронтенд магазина `/index.php` | HTTP 200 |
| Админка `/admin/index.php?route=common/login` | HTTP 200 |
| Таблиц с префиксом `oc_` | 159 |
| Демо-данные: `oc_setting` = 220 строк, `oc_product` = 19 товаров | OK |
| Админ `oc_user.user_id=1` = `admin` | OK |
| `information_schema.TABLES WHERE TABLE_NAME LIKE 'oc_%'` | 159 строк (LIKE-фильтр каталога) |

## Квирки OpenCart, закрытые в движке (5 фиксов)

| # | Квирка | Фикс |
| --- | --- | --- |
| 1 | Проба `SELECT * FROM information_schema.COLUMNS` для ещё НЕ созданной таблицы возвращала OK-пакет вместо пустого resultset (mysqli видел `true`, а не объект) | пустой каталог-результат несёт стандартные колонки каталога → wire отдаёт настоящий нуль-строчный resultset |
| 2 | `value double(15,8)` (валюты) | `FLOAT/DOUBLE (M,D)` поглощает спецификатор длины |
| 3 | `CONCAT('INV-', YEAR(CURDATE()), '-00')` в VALUES | семейство `YEAR/MONTH/DAY/DAYOFMONTH/QUARTER/HOUR/MINUTE/SECOND/DAYOFWEEK/DAYOFYEAR/WEEK/DAYNAME/MONTHNAME` |
| 4 | `INSERT INTO oc_user SET col = v, …` | форма `INSERT … SET` (desugar в одну строку именованных значений) |
| 5 | `information_schema.TABLES WHERE TABLE_NAME LIKE 'oc_%'` | LIKE-фильтр в каталог-шиме (wildcards MySQL) |

## Воспроизведение

```bash
# исходники тега + vendor (composer.json → upload/system/storage/vendor)
composer install   # в корне исходников
cp upload/config-dist.php upload/config.php && cp upload/admin/config-dist.php upload/admin/config.php

dna-db-core mysqld --data-dir /tmp/dna-opencart --bind 127.0.0.1 --port 13306 --user root:dnatest &
cd upload && php install/cli_install.php install \
  --username admin --password 'OpenCart!2026x' --email admin@example.com \
  --http_server http://127.0.0.1:8081/ --language en-gb \
  --db_driver mysqli --db_hostname 127.0.0.1 --db_port 13306 \
  --db_username root --db_password dnatest --db_database opencart --db_prefix oc_
# ожидание: SUCCESS! OpenCart successfully installed on your server
```

## Известные ограничения

- Агрегаты поверх каталога (`SELECT COUNT(*) FROM information_schema.TABLES`)
  шимом не покрываются — проекция каталога поддерживает `SELECT *` и список
  колонок. Живой OpenCart таких запросов не делает.
- OpenCart 4.1.0.4 использует свой DI/движок с `information_schema`-пробами
  только на этапе установки/апгрейда — работа магазина на живых данных не
  прогонялась в этом раунде ( storefront 200 / admin 200 зафиксированы).
