# DNA-DB CMS Edition — пошаговое руководство: MySQL-режим для WordPress, WooCommerce и любых MySQL-приложений

Дата: 2 ноября 2026 · ветка `cms-edition` · DNA-DB v4.x + `--features cms`

Начиная с этого релиза DNA-DB умеет **притворяться MySQL-сервером**: подкоманда
`dna-db-core mysqld` слушает порт 3306 и говорит на протоколе MySQL 4.1. Любое
приложение, которое умеет работать с MySQL — WordPress, WooCommerce, Joomla,
Drupal, Laravel, самописные скрипты на PHP/Python — подключается **без единой
строчки своего кода**: никакого drop-in `db.php`, никаких драйверов, только
`DB_HOST`.

Архитектура:

```
WordPress / WooCommerce / любой MySQL-клиент
        │  протокол MySQL 4.1 (mysqli, PDO, PyMySQL, mysql CLI)
        ▼
dna-db-core mysqld :3306        ← CmsSqlGate: SQL (диалект MySQL) → DNAQL
        ▼
DNA-движок (LSM + WAL + RS(4+2)) — то же хранилище, что и у native/pgwire
```

---

## Шаг 0. Что вы получите

* MySQL-совместимый слушатель на 3306: handshake v10, `mysql_native_password`
  (включая AuthSwitch для клиентов с `caching_sha2_password`), text- и
  binary-протоколы (`COM_STMT_PREPARE`/`EXECUTE`), multi-statement, `COM_PING`,
  `COM_INIT_DB`, `COM_FIELD_LIST`, `COM_STATISTICS`, `COM_RESET_CONNECTION`,
  `SEND_LONG_DATA`.
* Реальные коды ошибок MySQL: 1045 (доступ), 1054 (колонка), 1062 (дубликат),
  1146 (таблица), 1050/1091/1205/1213/3572 (DDL и блокировки) — плагины
  WordPress реагируют на них ровно так же, как на настоящем MySQL.
* Диалект MySQL: `AUTO_INCREMENT` + `LAST_INSERT_ID()`, `ON DUPLICATE KEY
  UPDATE`, `REPLACE INTO`, `INSERT … SELECT … FROM DUAL`, `SQL_CALC_FOUND_ROWS`
  + `FOUND_ROWS()`, `SHOW FULL COLUMNS` / `SHOW INDEX` (формат dbDelta),
  `GET_LOCK`/`RELEASE_LOCK`, `START TRANSACTION`/`COMMIT`/`ROLLBACK`,
  `SET NAMES/sql_mode/autocommit`, скалярные функции
  (`CONCAT`, `CONCAT_WS`, `UPPER/LOWER`, `LENGTH/CHAR_LENGTH`,
  `SUBSTRING`, `TRIM`, `REPLACE`, `LPAD/RPAD`, `MD5`, `SHA1`, `IF/IFNULL/
  NULLIF/COALESCE`, `GREATEST/LEAST`, `NOW()`, `UNIX_TIMESTAMP`…),
  составные `ON … AND …`, derived tables `(SELECT …) AS x`, `USING()`,
  `FOR UPDATE [SKIP LOCKED | NOWAIT]` с InnoDB-семантикой блокировок.

Проверено на реальных CMS (артефакты побайтово совпадают с MariaDB):

| Тест | Результат |
|---|---|
| WordPress 7.1.4 через `wp-cli` (установка, dbDelta, посты, меты, авторизация) | 21/21 артефактов MATCH |
| WooCommerce 11.2.1 (HPOS): товар → заказ → итоги → списание склада, 48 таблиц | 12/12 артефактов MATCH, 0 SQL-ошибок |
| Дифференциальный корпус WP/Woo (86 запросов) против MariaDB 11.8.6 | 81 MATCH + 4 BOTH-ERR (тот же errno) + 1 VERSION()-строка |
| Smoke реальными клиентами PyMySQL + MySQL Connector/Py | 21/21 |
| Security-батарея wire-поверхности | 24/24 |

---

## Шаг 1. Установка (проще, чем MySQL)

### Вариант А — скрипт (Ubuntu/Debian, systemd)

```bash
curl -fsSL https://raw.githubusercontent.com/nsmykh70-creator/dna-db-core/cms-edition/scripts/install.sh \
  | sudo bash -s -- --wordpress --user wp:сильный_пароль
```

Rust не нужен: скрипт скачивает пребилд-бинарник
(`prebuilt/linux-x86_64`, проверка по `SHA256SUMS`) и кладёт его в
`/usr/local/bin/dna-db-core`; если пребилд недоступен (не x86_64,
зеркало закрыто) — соберёт из исходников через cargo. Дальше сам создаёт
каталог данных `/var/lib/dna-db`, systemd-юнит на порту 3306 и запускает
сервис. Ни `mysql_install_db`, ни `mysql_secure_installation`, ни
`CREATE DATABASE`/`GRANT` — не нужны.

Docker/WSL без работающего systemd: скрипт детектирует это и запускает
сервер через `nohup` + pidfile (стоп: `kill $(cat /var/lib/dna-db/dna-db.pid)`,
логи: `/var/lib/dna-db/dna-db.log`). Снос: тот же скрипт с флагом
`--uninstall` (юнит и бинарь удаляются, данные остаются).

### Вариант Б — руками из исходников

```bash
git clone --branch cms-edition https://github.com/nsmykh70-creator/dna-db-core.git
cd dna-db-core
cargo build --release --features cms      # 1–2 минуты
sudo install -m 0755 target/release/dna-db-core /usr/local/bin/
```

---

## Шаг 2. Первый запуск

```bash
sudo mkdir -p /var/lib/dna-db
sudo dna-db-core mysqld --data-dir /var/lib/dna-db \
  --port 3306 --bind 0.0.0.0 \
  --wordpress \
  --user wp:сильный_пароль
```

Что делают флаги:

| Флаг | Смысл |
|---|---|
| `--data-dir` | каталог данных DNA-движка (WAL, сегменты, индексы) |
| `--port` | порт MySQL-протокола (по умолчанию 3306) |
| `--bind` | адрес слушателя (по умолчанию `0.0.0.0`; для локального сайта — `127.0.0.1`) |
| `--wordpress` | создать 12 таблиц ядра WordPress сразу при старте (опционально — можно доверить dbDelta) |
| `--user USER:PASS` | логин MySQL. Флаг повторяемый — несколько пользователей. **Без флага** сервер работает в trust-режиме (как `--skip-grant-tables`) и печатает WARNING — только для тестов! |

При старте сервер печатает готовый блок `wp-config.php`:

```
[MYSQLD] ---- wp-config.php ----
define( 'DB_NAME', 'wordpress' );
define( 'DB_USER', 'wp' );
define( 'DB_PASSWORD', '***' );
define( 'DB_HOST', '127.0.0.1:3306' );
[MYSQLD] -----------------------
```

Проверка любым MySQL-клиентом:

```bash
mysql -h 127.0.0.1 -P 3306 -u wp -p -e "SELECT VERSION(), CONCAT(UPPER('днк'), '-', LOWER('ТЕСТ'));"
```

---

## Шаг 3. Подключение WordPress

В `wp-config.php` сайта:

```php
define( 'DB_NAME', 'wordpress' );
define( 'DB_USER', 'wp' );
define( 'DB_PASSWORD', 'сильный_пароль' );
define( 'DB_HOST', '127.0.0.1:3306' );   // ← единственное отличие от обычного хостинга
define( 'DB_CHARSET', 'utf8mb4' );
$table_prefix = 'wp_';
```

Установка штатная:

```bash
wp core install --url=http://example.com --title="Мой сайт" \
  --admin_user=admin --admin_password=... --admin_email=me@example.com --skip-email
```

`dbDelta` создаёт/обновляет таблицы через `SHOW FULL COLUMNS`/`SHOW INDEX` —
формат ответов DNA-DB побайтово совпадает с MySQL, поэтому и ядро, и плагины
(Autoptimize, Woo и т. д.) проходят обновление схемы без изменений.

WooCommerce: `wp plugin install woocommerce --activate` — HPOS-таблицы
(`wp_wc_orders`, `wp_wc_product_meta_lookup`, `wp_woocommerce_order_items`…)
создаются через тот же диалект-слой; заказ, расчёт итогов и списание склада
дают артефакты, идентичные MariaDB (см. `docs/cms/E2E_WOO_MYSQLWIRE_REPORT.md`).

---

## Шаг 4. Прочие MySQL-приложения

```bash
# mysqldump-совместимый экспорт (read-only путь через wire)
mysql -h 127.0.0.1 -P 3306 -u wp -p wordpress -e "SHOW TABLES;"

# Python
import pymysql
conn = pymysql.connect(host="127.0.0.1", port=3306, user="wp",
                       password="сильный_пароль", database="wordpress")

# PDO (PHP)
new PDO('mysql:host=127.0.0.1;port=3306;dbname=wordpress', 'wp', 'сильный_пароль');
```

Имена баз: DNA-DB — единое хранилище; `USE`/`INIT_DB` принимаются и
запоминаются в сессии, чтобы драйверы работали без изменений.

---

## Шаг 5. systemd (продакшен)

`/etc/systemd/system/dna-db.service`:

```ini
[Unit]
Description=DNA-DB (MySQL-compatible)
After=network.target

[Service]
ExecStart=/usr/local/bin/dna-db-core mysqld --data-dir /var/lib/dna-db \
  --port 3306 --bind 0.0.0.0 --wordpress --user wp:сильный_пароль
Restart=always
RestartSec=2
User=dna
# защита: демон не должен писать вне своего каталога
ReadWritePaths=/var/lib/dna-db
ProtectSystem=strict
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now dna-db
sudo systemctl status dna-db
```

---

## Шаг 6. Безопасность

1. **Всегда задавайте `--user user:pass`.** Без него — trust-режим с
   предупреждением в логе; это только для локальных тестов.
2. **Бинд-адрес.** Для сайта на той же машине — `--bind 127.0.0.1`. Для
   удалённых клиентов закройте 3306 файрволом или используйте SSH-туннель.
3. **SQL-инъекции.** Драйверы с параметрами (`%s`, `?`) передают значения
   отдельными полями протокола; DNA-DB хранит их как литералы — проверено
   батареею `scripts/mysqlwire_security_test.py` (24/24): payload
   `'; DROP TABLE … --`, UNION- и LIKE-инъекции, brute force, malformed-пакеты —
   данные целы, сервер жив, errno корректны.
4. **Stacked queries.** Без флага `CLIENT_MULTI_STATEMENTS` второй оператор в
   строке не выполняется — как в реальном MySQL.
5. Полная модель угроз DNA-DB (TLS 1.3, Argon2id, lockout, XChaCha20-at-rest)
   описана в `docs/SECURITY_RU.md`.

---

## Шаг 7. Производительность и память

Одинаковый SQL через pymysql, схема `wp_posts`/`wp_options`, 8 000 операций на
фазу (методика `scripts/cms_wire_bench.py`, MariaDB 11.8.6 — эталон):

| Фаза | DNA-DB mysqld | MariaDB 11.8.6 | Отношение |
|---|---|---|---|
| INSERT по одной строке (autocommit) | 15 682 ops/s | 13 092 ops/s | ×1,20 |
| Пакетный INSERT (100 строк) | 125 083 ops/s | 216 268 ops/s | ×0,58 |
| SELECT по PK | 8 046 ops/s | 8 889 ops/s | ×0,91 |
| SELECT по вторичному индексу | 8 647 ops/s | 8 290 ops/s | ×1,04 |
| UPDATE по PK | 11 731 ops/s | 12 611 ops/s | ×0,93 |
| WordPress-микс (опции SELECT+UPSERT) | 5 385 ops/s | 4 701 ops/s | ×1,15 |
| Память демона (пик RSS за фазу) | 8–13 MiB | 118–123 MiB | ~×9–10 меньше |

Скорость — паритет или лучше (кроме пакетного INSERT, где MariaDB выигрывает
на массовой загрузке); память демона — на порядок меньше. Сырые данные и
полная методика: `docs/cms/wire_bench_results.json` / `.md`. Повторить:

```bash
python3 scripts/cms_wire_bench.py --dna-port 13307 --mysql-port 13306 --rows 8000
```

---

## Шаг 8. Диагностика и типовые проблемы

| Симптом | Причина / решение |
|---|---|
| `Access denied for user 'wp'` | Пароль не совпадает с `--user wp:…`. Проверьте флаг; логин чувствителен к регистру |
| Порт 3306 занят | На машине уже стоит MySQL/MariaDB: остановите его (`systemctl stop mysql`) или запустите DNA-DB на другом порту и укажите его в `DB_HOST` |
| `Unknown database 'wordpress'` | База задаётся флагом `--wordpress` (регистрирует схему) либо просто начните установку WP — dbDelta создаст таблицы; имя базы запоминается через `INIT_DB` |
| Плагин ругается на `information_schema` | Часть плагинов читает `information_schema.tables`; на текущий момент используйте альтернативы плагина или заведите issue — покрытие расширяется |
| Как посмотреть, что лежит физически | `SHOW STORAGE FOR wp_options WHERE option_id = 1;` — хромосома, локус и A/C/G/T-представление каждой величины |
| Бэкап | `pitr --data-dir … --target ./snap` — консистентный снапшот; restore — `pitr --restore` |

Полезные команды:

```bash
dna-db-core mysqld --help            # все флаги режима
journalctl -u dna-db -f              # логи сервиса
cargo test --features cms            # 300+ тестов, включая wire-интеграцию
python3 scripts/mysqlwire_client_smoke.py 13307   # smoke реальными клиентами
```

---

## Приложение. Что именно проверено (артефакты)

* `docs/cms/E2E_WP_MYSQLWIRE_REPORT.md` — WordPress 7.1.4 через wire: 12 таблиц
  dbDelta, 125 опций, посты/меты/пользователи, `wp_authenticate`, `CONCAT`
  с кириллицей — 21/21 артефактов идентичны MariaDB.
* `docs/cms/E2E_WOO_MYSQLWIRE_REPORT.md` — WooCommerce 11.2.1 (HPOS): заказ
  500.00, склад 10−2, меты заказа побайтно, 0 SQL-ошибок на полном цикле.
* `docs/cms/COMPARISON_RUN7.md` — дифференциальный корпус 86 запросов против
  MariaDB 11.8.6: 81 MATCH, 4 BOTH-ERR (одинаковый errno), 1 DIFF-VALUE
  (строка `VERSION()`); воспроизведён позапросно на отдельной машине —
  `docs/cms/COMPARISON_RUN8.md` (вердикты идентичны).
* `docs/cms/MYSQLWIRE_SECURITY_REPORT.md` — 24/24 security-проверок.
* `docs/cms/MYSQLWIRE_CLIENT_SMOKE.md` — PyMySQL + Connector/Py 21/21.
