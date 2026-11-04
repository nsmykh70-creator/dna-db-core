# DNA-DB CMS Edition — E2E: Joomla 5.4.10 через универсальный MySQL wire-адаптер

Дата: 4 ноября 2026. Ветка: `cms-edition`. Версия движка: v4.0.3 (prebuilt, feature `cms`).

## Что проверяется

Joomla 5.4.10 (последняя стабильная 5.x, `downloads.joomla.org`), установленная
на DNA-DB **как на обычный MySQL**: штатный mysqli из статического PHP 8.3.28 →
`dna-db-core mysqld :13306`. Драйвер сайта — `mysqli` (host `127.0.0.1:13306`),
префикс `jsml_`. Весь сценарий управляется драйвером
`scripts/joomla_e2e.php`, который повторяет AJAX-последовательность
реального установщика (`installation/template/js/setup.js` + `remove.js`):

1. `GET /installation/index.php` — сессия + CSRF-токен (токен ротируется в
   каждом JSON-ответе — драйвер перечитывает);
2. `POST ?task=installation.dbcheck&format=json` — проверка коннекта mysqli;
3. `create` — `CREATE DATABASE` + `ALTER DATABASE … CHARACTER SET/COLLATE`;
4. `populate1…3` — `base.sql`, `supports.sql`, `extensions.sql` (все три
   дампа Joomla целиком, сотни стейтментов);
5. `custom1, custom2` — `localise.sql`, `custom.sql`;
6. `config` — запись `configuration.php` + создание Super User;
7. `disableAutomatedUpdates` + `removeFolder` — установщик удаляет
   `installation/`;
8. проверки: фронтенд `GET /index.php` (HTTP 200), админ-логин
   `GET /administrator/index.php` (HTTP 200), уровень БД через mysqli —
   `#__users` = 1, `#__assets` = 92, `#__extensions` = 248, вставка статьи
   prepared statement'ом с чтением обратно.

## Результат: PASS

| Шаг | Результат |
| --- | --- |
| dbcheck (mysqli через wire) | OK |
| CREATE/ALTER DATABASE | OK |
| base.sql (~1300 стейтментов) | OK |
| supports.sql | OK |
| extensions.sql | OK |
| localise.sql / custom.sql | OK |
| configuration.php + админ | OK |
| removeFolder | OK |
| Фронтенд /index.php | HTTP 200 |
| Админ /administrator | HTTP 200 |
| users=1, assets=92, extensions=248 | OK |
| INSERT/SELECT статьи (prepared stmt) | OK, insert_id = 1 |

## Что пришлось починить в движке (13 фиксов, все с регресс-тестами)

| # | Квирка Joomla | Фикс |
| --- | --- | --- |
| 1 | `#`-комментарии в base.sql | лексер: `#` — строчный комментарий |
| 2 | `ALTER DATABASE … CHARACTER SET/COLLATE` после CREATE | шим-ответ OK |
| 3 | `KEY time (time)` / `KEY userid (userid)` — индексы, названные ключевыми словами типов | lookahead по содержимому скобок: идентификаторы = список колонок, числа/строки = тип с длиной |
| 4 | `PRIMARY KEY (id(100))`, безымянный `INDEX (user_id(100))` (webauthn) | префиксные длины в PK/индексах поглощаются |
| 5 | `DEFAULT -1` (banners) | отрицательные числовые DEFAULT |
| 6 | `CURRENT_TIMESTAMP()` в seed-данных | pseudo-литералы CURRENT_* (WIP-раунд) |
| 7 | `NOW()`, `DATE_FORMAT`, `TIME_FORMAT`, `CONCAT` в VALUES | скалярные функции в value() + `+/- INTERVAL n UNIT` |
| 8 | `ON UPDATE CURRENT_TIMESTAMP`, `COMMENT='…'`, `KEY … USING BTREE` | поглощаются (WIP-раунд) |
| 9 | `CASE WHEN … THEN … ELSE … END` в проекциях (ArticlesModel) | CASE обеих форм → вложенный скалярный IF(), depth-guard |
| 10 | `ORDER BY CASE … END DESC` (frontpage) | ORDER BY-ключ как выражение (rewind + materialization) |
| 11 | `SUM(CASE WHEN … END) AS due_count` (dashboard) | `AggOverExpr`: аргумент агрегата материализуется построчно до агрегации |
| 12 | вычисляемые проекции ссылаются на колонки JOIN-таблиц (`c.published`, `ua.name`) | валидация проекций по JOIN-universe |
| 13 | `#__finder_terms_common` без PRIMARY KEY | неявный скрытый `__rowid` (автоинкремент, не виден в `SELECT *`) |

## Воспроизведение

```bash
# 1. распаковать Joomla 5.4.10 в <joomla-root>, положить рядом dna-db-core
dna-db-core mysqld --data-dir /tmp/dna-joomla --bind 127.0.0.1 --port 13306 --user root:dnatest &
php -d display_errors=0 -S 127.0.0.1:8080 -t <joomla-root> &
# 2. прогнать драйвер (порт БД зашит в db_host как 127.0.0.1:13306)
php scripts/joomla_e2e.php http://127.0.0.1:8080 jsml_
# ожидание: JOOMLA_E2E_OK
```

Требования к PHP: mysqli, session, json (статический bulk-build static-php-cli
8.3.28 подходит целиком). DNA-DB поднимается как единственный каталог-БД —
имя базы из установщика принимается как namespace (как в MySQL-режиме WP).
