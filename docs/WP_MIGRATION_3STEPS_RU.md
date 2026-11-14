# Миграция WordPress с MySQL на DNA-DB за 3 шага

Ветка `cms-edition` · DNA-DB CMS edition · 2026-11

Цель этого руководства — перевести рабочий сайт WordPress с MySQL/MariaDB на
DNA-DB за три коротких шага без изменения ни одной строки кода WordPress,
плагинов или темы. Данные переносятся штатным `mysqldump`, переключение —
одна строка в `wp-config.php`. В конце сайт работает на DNA-DB: каждая
таблица лежит в двойной спирали dnadb (Strand A + Strand B), а состояние
базы видно в веб-панели и в Grafana.

```
Старый сервер MySQL  ──mysqldump──▶  файл .sql  ──mysql-клиент──▶  DNA-DB mysqld :3306
                                                                        ▲
 WordPress (wp-config.php: DB_HOST) ────────────────────────────────────┘
```

Почему это безопасно: `dna-db-core mysqld` говорит по MySQL-протоколу 4.1
(mysqli, PDO, wp-cli, mysqldump), отвечает настоящими кодами ошибок MySQL
(1062/1054/1146/1213/1205…) и честным `SHOW CREATE TABLE`. На бенчмарках
этой ветки артефакты установки WordPress и WooCommerce совпали с MariaDB
побайтово (см. [CMS_EDITION_RU.md](CMS_EDITION_RU.md), прогоны №1–№13).

---

## Шаг 0. Подготовка (5 минут)

1. **Снимите резервную копию старой базы** — она же и есть переносимый
   дамп (шаг 2), но на всякий случай скопируйте артефакт в надёжное место:

   ```bash
   mysqldump --no-defaults -h OLD_HOST -P 3306 -u OLD_USER -pOLD_PASS \
     --single-transaction wordpress > wordpress_pre_migration.sql
   ```

2. **Зафиксируйте паузу писателей.** На время миграции (обычно 10–30 минут
   для сайта до ~1 ГБ базы) отключите планировщик WP-Cron и поставьте
   сайт на обслуживание — так дамп будет консистентным:

   ```bash
   # в wp-config.php на время миграции:
   define( 'DISABLE_WP_CRON', true );
   ```

3. **Выберите схему развёртывания DNA-DB:**

   | Сценарий | Путь |
   |---|---|
   | Быстрая проба на одном сервере | Docker: `docker compose up -d --build` (см. [docker-compose.yml](../docker-compose.yml)) |
   | Прод-сервер Ubuntu/Debian с systemd | `scripts/install.sh --wordpress --user wp:ПАРОЛЬ` (пребилд, sha256-проверка) |
   | Своя сборка из исходников | `cargo build --release --features cms` → `dna-db-core mysqld …` |

   Docker-стек поднимает сразу три сервиса: `dna-db` (MySQL :3306, панель
   и метрики :9310), Prometheus (:9090) и Grafana (:3000) с готовым
   дашбордом «DNA-DB CMS Edition».

---

## Шаг 1. Установить DNA-DB и создать базу с двумя спиралями

**Docker** (из корня репозитория):

```bash
docker compose up -d --build
```

**Бареметал** (Ubuntu/Debian, systemd):

```bash
curl -fsSL https://raw.githubusercontent.com/nsmykh70-creator/dna-db-core/cms-edition/scripts/install.sh \
  | sudo bash -s -- --user wp:СИЛЬНЫЙ_ПАРОЛЬ
```

Откройте веб-панель — `http://СЕРВЕР:9310` (логин те же, что в `--user`;
для Docker — `root` / `dna-db`). Панель показывает базы данных и хранилища
спиралей. Нажмите **«Создать базу (2 спирали)»**:

* имя базы — `wordpress` (как в старом `DB_NAME`);
* хранилище спирали A — выберите корень из списка (по умолчанию
  `local-a` — локальный диск сервера);
* хранилище спирали B — второй корень; для устойчивости выберите
  **сетевое хранилище** (NFS/SMB-монтирование, добавляется кнопкой
  «Хранилища спиралей → Добавить корень»).

Каждая база в DNA-DB хранится парой спиралей dnadb — Strand A и Strand B.
Разнесение их по разным физическим носителям (локальный диск + сетевое
хранилище) даёт переживание отказа любого из носителей.

Эквивалент из SQL-клиента, если панель не используется:

```bash
mysql --no-defaults -h 127.0.0.1 -P 3306 -u root -pdna-db \
  -e "CREATE DATABASE wordpress"
```

`CREATE DATABASE` через wire-протокол создаёт ту же пару спиралей
с тем же выбором хранилищ по умолчанию (`local-a`/`local-b`).

---

## Шаг 2. Перенести данные штатным mysqldump

DNA-DB отвечает на `SHOW CREATE TABLE` и `SELECT` как MySQL, поэтому
обычный дампер работает без доработок (проверено E2E
`scripts/e2e_mysqldump_roundtrip.sh` и `scripts/e2e_backup_restore.sh`).

**Выгрузка со старого MySQL:**

```bash
mysqldump --no-defaults -h OLD_HOST -P 3306 -u OLD_USER -pOLD_PASS \
  wordpress > wordpress.sql
```

**Импорт в DNA-DB** (любой mysql-клиент, даже из комплекта MariaDB):

```bash
mysql --no-defaults -h DNA_DB_HOST -P 3306 -u root -pdna-db wordpress \
  < wordpress.sql
```

Гейт сам разворачивает условные комментарии `/*!NNNNN … */` и
`/*M!NNNNN … */` (MariaDB-форма), `LOCK TABLES`, `AUTO_INCREMENT` и
`ON UPDATE CURRENT_TIMESTAMP`, так что артефакт mariadb-dump ≥ 10.11
импортируется без правок.

**Альтернатива — чистая установка** (если контент переносится вручную или
базой является свежий проект): запустите `wp core install` прямо против
DNA-DB:

```bash
wp core install --url=https://example.com --title="Сайт" \
  --admin_user=admin --admin_password=ПАРОЛЬ --admin_email=it@example.com \
  --path=/var/www/wordpress \
  --dbhost=DNA_DB_HOST:3306 --dbname=wordpress --dbuser=root --dbpass=dna-db
```

На этой ветке полный цикл `wp-cli` (установка, dbDelta, посты, меты,
авторизация) воспроизводит артефакты MariaDB 21/21.

---

## Шаг 3. Переключить WordPress на DNA-DB и проверить

1. **Правка `wp-config.php`** — единственная строка кода, которая меняется:

   ```php
   define( 'DB_NAME',     'wordpress' );
   define( 'DB_USER',     'root' );
   define( 'DB_PASSWORD', 'dna-db' );
   define( 'DB_HOST',     'DNA_DB_HOST:3306' );   // был OLD_HOST:3306
   define( 'DB_CHARSET',  'utf8mb4' );
   ```

2. **Проверки через wp-cli:**

   ```bash
   wp db check --dbhost=DNA_DB_HOST:3306 --dbuser=root --dbpass=dna-db   # CHECK TABLE по всем таблицам
   wp core verify-checksums                                              # целостность ядра
   wp plugin list                                                        # ответы движка
   ```

3. **Проверки в веб-панели** (`http://СЕРВЕР:9310`):
   * база `wordpress` в списке — пара спиралей A/B, счётчик баз вырос;
   * карточки «Данные на диске» и «Обслуживание» (компакция) живут;
   * вкладка «Хранилища спиралей» — корни, куда легли A и B.

4. **Мониторинг.** Grafana (Docker-стек) — дашборд
   **«DNA-DB CMS Edition»**: активные сессии, запросы/с, ошибки/с, uptime,
   базы/спирали/корни хранения. Скрипты, которые скрейпят метрики, —
   `monitoring/prometheus.yml` (цель `dna-db:9310/metrics`).

5. **Верните крон**: уберите `DISABLE_WP_CRON` из `wp-config.php`.

### Что поехало? Быстрая диагностика

| Симптом | Где смотреть | Действие |
|---|---|---|
| `Error establishing a database connection` | лог сервера `[MYSQLD]` | Проверьте `DB_HOST`/пароль; trust-режим принимает любой логин |
| Плагин жалуется на дубликат | errno 1062 уже «настоящий» | Это штатная логика плагина (unique key) |
| Медленные выборки | Grafana: запросы/с, ошибки/с | Включите `DNA_LOG_SQL=1` — трассировка SQL в stderr |

---

## Откат (если что-то пошло не так)

Артефакт, снятый с DNA-DB, совместим с ванильным MySQL/MariaDB в обратную
сторону (проверено прогона №10 — один дамп восстанавливается в оба
движка):

```bash
mysqldump --no-defaults -h DNA_DB_HOST -P 3306 -u root -pdna-db \
  wordpress > wordpress_from_dna.sql
mysql --no-defaults -h OLD_HOST -P 3306 -u OLD_USER -pOLD_PASS wordpress \
  < wordpress_from_dna.sql
```

Затем верните прежний `DB_HOST` в `wp-config.php`. Данные на спиралях
DNA-DB при этом сохраняются — повторная миграция займёт те же 3 шага.

## Чеклист перед переключением

- [ ] Дамп старой базы снят и хранится вне сервера (`wordpress_pre_migration.sql`)
- [ ] DNA-DB запущена, панель отвечает на `:9310`
- [ ] База `wordpress` создана, видны две спирали на выбранных хранилищах
- [ ] Импорт дампа прошёл без ошибок (`mysql … < wordpress.sql` вернул 0)
- [ ] `wp db check` и `wp core verify-checksums` зелёные
- [ ] Grafana показывает непустые графики запросов/с
- [ ] `DISABLE_WP_CRON` снят

## Известные ограничения

Актуальный список диалектных особенностей и целевых работ — раздел
«Документированные расхождения» в [CMS_EDITION_RU.md](CMS_EDITION_RU.md)
(на ветке `cms-edition` все пункты отчёта «Расхождения и ошибки» закрыты,
прогон №12). Существенное для миграции: collation-независимость
`WHERE`/`GROUP BY` для нестандартных сортировок — см. таблицу ограничений
в том же документе.
